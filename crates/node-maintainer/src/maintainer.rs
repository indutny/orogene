use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic;
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use async_std::fs;
use futures::lock::Mutex;
use futures::{FutureExt, StreamExt, TryStreamExt};
use kdl::KdlDocument;
use nassun::{Nassun, NassunOpts, Package, PackageSpec};
use oro_common::CorgiManifest;
use petgraph::stable_graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use unicase::UniCase;
use url::Url;

use crate::edge::{DepType, Edge};
use crate::error::NodeMaintainerError;
use crate::{Graph, IntoKdl, Lockfile, Node};

#[derive(Debug, Clone, Default)]
pub struct NodeMaintainerOptions {
    nassun_opts: NassunOpts,
}

impl NodeMaintainerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn registry(mut self, registry: Url) -> Self {
        self.nassun_opts = self.nassun_opts.registry(registry);
        self
    }

    pub fn scope_registry(mut self, scope: impl AsRef<str>, registry: Url) -> Self {
        self.nassun_opts = self.nassun_opts.scope_registry(scope, registry);
        self
    }

    pub fn base_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.nassun_opts = self.nassun_opts.base_dir(path);
        self
    }

    pub fn default_tag(mut self, tag: impl AsRef<str>) -> Self {
        self.nassun_opts = self.nassun_opts.default_tag(tag);
        self
    }

    pub async fn from_kdl(self, kdl: impl IntoKdl) -> Result<NodeMaintainer, NodeMaintainerError> {
        async fn inner(
            me: NodeMaintainerOptions,
            kdl: KdlDocument,
        ) -> Result<NodeMaintainer, NodeMaintainerError> {
            let nassun = me.nassun_opts.build();
            let graph = Lockfile::from_kdl(kdl)?.into_graph(&nassun).await?;
            let mut nm = NodeMaintainer { nassun, graph };
            nm.run_resolver_new().await?;
            Ok(nm)
        }
        inner(self, kdl.into_kdl()?).await
    }

    pub async fn resolve(
        self,
        root_spec: impl AsRef<str>,
    ) -> Result<NodeMaintainer, NodeMaintainerError> {
        let nassun = self.nassun_opts.build();
        let package = nassun.resolve(root_spec).await?;
        let mut nm = NodeMaintainer {
            nassun,
            graph: Default::default(),
        };
        let node = nm.graph.inner.add_node(Node::new(package));
        nm.graph[node].root = node;
        nm.run_resolver_new().await?;
        Ok(nm)
    }
}

pub struct NodeMaintainer {
    nassun: Nassun,
    graph: Graph,
}

impl NodeMaintainer {
    pub fn builder() -> NodeMaintainerOptions {
        NodeMaintainerOptions::new()
    }

    pub async fn resolve(
        root_spec: impl AsRef<str>,
    ) -> Result<NodeMaintainer, NodeMaintainerError> {
        Self::builder().resolve(root_spec).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn render_to_file(&self, path: impl AsRef<Path>) -> Result<(), NodeMaintainerError> {
        fs::write(path.as_ref(), self.graph.render()).await?;
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn write_lockfile(&self, path: impl AsRef<Path>) -> Result<(), NodeMaintainerError> {
        fs::write(path.as_ref(), self.graph.to_kdl()?.to_string()).await?;
        Ok(())
    }

    pub fn to_resolved_tree(&self) -> Result<crate::Lockfile, NodeMaintainerError> {
        self.graph.to_resolved_tree()
    }

    pub fn to_kdl(&self) -> Result<kdl::KdlDocument, NodeMaintainerError> {
        self.graph.to_kdl()
    }

    pub fn render(&self) -> String {
        self.graph.render()
    }

    async fn run_resolver_new(&mut self) -> Result<(), NodeMaintainerError> {
        let (idx_sender, idx_receiver) = futures::channel::mpsc::unbounded::<NodeIndex>();
        idx_sender.unbounded_send(self.graph.root)?;

        let (resolve_sender, resolve_receiver) =
            futures::channel::mpsc::unbounded::<(String, PackageSpec, DepType, NodeIndex)>();

        // Root node is in flight
        let in_flight = Arc::new(atomic::AtomicUsize::new(1));

        let nassun = Arc::new(&self.nassun);
        let graph_mutex = Arc::new(Mutex::new(&mut self.graph));
        let graph_mutex_copy = graph_mutex.clone();
        let in_flight_copy = in_flight.clone();
        let idx_sender_copy = idx_sender.clone();
        let resolve_sender_copy = resolve_sender.clone();
        let resolver = resolve_receiver
            .map(|(name, requested, dep_type, dependent_idx)| {
                let idx_sender = idx_sender.clone();
                let in_flight = in_flight.clone();
                let graph_mutex = graph_mutex.clone();
                let nassun = nassun.clone();
                let resolve_sender = resolve_sender_copy.clone();
                async move {
                    let nassun = nassun.clone();

                    let package = nassun.resolve(name).await?;
                    // println!("  downloaded {:?}", package.name());

                    let mut graph = graph_mutex.lock().await;
                    let in_flight = in_flight.clone();
                    let name = UniCase::new(package.name().to_string());
                    let satisfies = Self::satisfy_dependency(
                        &mut graph,
                        dependent_idx,
                        &dep_type,
                        &name,
                        &requested,
                    )?;
                    if satisfies {
                        if in_flight.fetch_sub(1, atomic::Ordering::SeqCst) == 1 {
                            idx_sender.close_channel();
                            resolve_sender.close_channel();
                        }
                    } else {
                        let child_idx =
                            Self::place_child(&mut graph, dependent_idx, package, dep_type)?;
                        idx_sender.unbounded_send(child_idx)?;
                    }
                    Ok::<_, NodeMaintainerError>(())
                }
            })
            .buffer_unordered(1000)
            .try_for_each(|_| futures::future::ready(Ok(())));

        let graph_mutex = graph_mutex_copy;
        let in_flight = in_flight_copy;
        let idx_sender = idx_sender_copy;
        let traverser = idx_receiver
            .map(|node_idx| {
                let graph_mutex = graph_mutex.clone();
                let in_flight = in_flight.clone();
                let idx_sender = idx_sender.clone();
                let resolve_sender = resolve_sender.clone();

                async move {
                    let graph_mutex = graph_mutex.clone();
                    let mut graph = graph_mutex.lock().await;
                    let in_flight = in_flight.clone();
                    let mut names = HashSet::new();
                    // println!("start {:?}", graph[node_idx].package.name());
                    let manifest = graph[node_idx].package.corgi_metadata().await?.manifest;

                    let resolve_sender = resolve_sender.clone();

                    let mut to_resolve = Vec::new();

                    // Grab all the deps from the current package and fire off a
                    // lookup. These will be resolved concurrently.
                    for ((name, spec), dep_type) in Self::package_deps(&graph, node_idx, &manifest)
                    {
                        // `dependencies` > `optionalDependencies` ->
                        // `peerDependencies` -> `devDependencies` (if we're looking
                        // at root)
                        let name = UniCase::new(name.clone());
                        if names.contains(&name) {
                            continue;
                        }
                        let requested = format!("{name}@{spec}").parse()?;

                        // Walk up the current hierarchy to see if we find a
                        // dependency that already satisfies this request. If so,
                        // make a new edge and move on.
                        let satisfies = Self::satisfy_dependency(
                            &mut graph, node_idx, &dep_type, &name, &requested,
                        )?;
                        if satisfies {
                            names.insert(name);
                            continue;
                        }

                        // Otherwise, we have to fetch package metadata to
                        // create a new node (which we'll place later).
                        in_flight.fetch_add(1, atomic::Ordering::SeqCst);
                        // println!("  scheduled {:?}@{:?}", name, spec);
                        to_resolve.push((
                            format!("{name}@{spec}"),
                            requested,
                            dep_type.clone(),
                            node_idx,
                        ));
                        names.insert(name);
                    }

                    // Release mutex so that code below doesn't dead lock
                    drop(graph);

                    for tuple in to_resolve {
                        resolve_sender.unbounded_send(tuple)?;
                    }

                    if in_flight.fetch_sub(1, atomic::Ordering::SeqCst) == 1 {
                        idx_sender.close_channel();
                        resolve_sender.close_channel();
                    }

                    // println!("end");
                    Ok::<(), NodeMaintainerError>(())
                }
            })
            .buffered(1000)
            .try_for_each(|_| futures::future::ready(Ok(())));

        let (a, b) = futures::future::join(traverser, resolver).await;
        a?;
        b?;

        Ok(())
    }

    async fn run_resolver_old(&mut self) -> Result<(), NodeMaintainerError> {
        let mut package_groups = Vec::new();
        let mut q = VecDeque::new();
        q.push_back(self.graph.root);
        // Start iterating over the queue. We'll be adding things to it as we find them.
        while !q.is_empty() {
            while let Some(node_idx) = q.pop_front() {
                let mut names = HashSet::new();
                let mut packages = Vec::new();
                let manifest = self.graph[node_idx]
                    .package
                    .corgi_metadata()
                    .await?
                    .manifest;
                // Grab all the deps from the current package and fire off a
                // lookup. These will be resolved concurrently.
                for ((name, spec), dep_type) in Self::package_deps(&self.graph, node_idx, &manifest)
                {
                    // `dependencies` > `optionalDependencies` ->
                    // `peerDependencies` -> `devDependencies` (if we're looking
                    // at root)
                    let name = UniCase::new(name.clone());
                    if !names.contains(&name) {
                        let requested = format!("{name}@{spec}").parse()?;
                        // Walk up the current hierarchy to see if we find a
                        // dependency that already satisfies this request. If so,
                        // make a new edge and move on.
                        if !Self::satisfy_dependency(
                            &mut self.graph,
                            node_idx,
                            &dep_type,
                            &name,
                            &requested,
                        )? {
                            // Otherwise, we have to fetch package metadata to
                            // create a new node (which we'll place later).
                            packages.push(
                                self.nassun
                                    .resolve(format!("{name}@{spec}"))
                                    .map(move |p| (p, requested, dep_type, node_idx)),
                            );
                        };
                        names.insert(name);
                    }
                }

                package_groups.push(futures::stream::iter(packages));
            }

            // Order doesn't matter here: each node name is unique, so we
            // don't have to worry about races messing with placement.
            let mut stream = futures::stream::iter(package_groups.drain(..))
                .flatten()
                .buffer_unordered(30);
            while let Some((package, requested, dep_type, dependent_idx)) = stream.next().await {
                let package = package?;
                let name = UniCase::new(package.name().to_string());
                if Self::satisfy_dependency(
                    &mut self.graph,
                    dependent_idx,
                    &dep_type,
                    &name,
                    &requested,
                )? {
                    continue;
                }
                q.push_back(Self::place_child(
                    &mut self.graph,
                    dependent_idx,
                    package,
                    dep_type,
                )?);
            }

            // We sort the current queue so we consider more shallow
            // dependencies first, and we also sort alphabetically.
            q.make_contiguous().sort_by(|a_idx, b_idx| {
                let a = &self.graph[*a_idx];
                let b = &self.graph[*b_idx];
                match a.depth(&self.graph).cmp(&b.depth(&self.graph)) {
                    Ordering::Equal => a.package.name().cmp(b.package.name()),
                    other => other,
                }
            })
        }
        Ok(())
    }

    fn satisfy_dependency(
        graph: &mut Graph,
        dependent_idx: NodeIndex,
        dep_type: &DepType,
        name: &UniCase<String>,
        requested: &PackageSpec,
    ) -> Result<bool, NodeMaintainerError> {
        if let Some(satisfier_idx) = graph.find_by_name(dependent_idx, &name)? {
            if graph[satisfier_idx]
                .package
                .resolved()
                .satisfies(&requested)?
            {
                let edge_idx = graph.inner.add_edge(
                    dependent_idx,
                    satisfier_idx,
                    Edge::new(requested.clone(), dep_type.clone()),
                );
                graph[dependent_idx]
                    .dependencies
                    .insert(name.clone(), edge_idx);
                return Ok(true);
            }
            return Ok(false);
        }
        return Ok(false);
    }

    fn place_child(
        graph: &mut Graph,
        dependent_idx: NodeIndex,
        package: Package,
        dep_type: DepType,
    ) -> Result<NodeIndex, NodeMaintainerError> {
        let requested = package.from().clone();
        let child_name = UniCase::new(package.name().to_string());
        let child_node = Node::new(package);
        let child_idx = graph.inner.add_node(child_node);
        graph[child_idx].root = graph.root;
        // We needed to generate the node index before setting it in the node,
        // so we do that now.
        graph[child_idx].idx = child_idx;

        // Edges represent the logical dependency relationship (not the
        // hierarchy location).
        let edge_idx = graph.inner.add_edge(
            dependent_idx,
            child_idx,
            Edge::new(requested.clone(), dep_type),
        );

        // Now we calculate the highest hierarchy location that we can place
        // this node in.
        let mut parent_idx = Some(dependent_idx);
        let mut target_idx = dependent_idx;
        'outer: while let Some(curr_target_idx) = parent_idx {
            if let Some(resolved) = graph.resolve_dep(curr_target_idx, &child_name) {
                for edge_ref in graph.inner.edges_directed(resolved, Direction::Incoming) {
                    let (from, _) = graph
                        .inner
                        .edge_endpoints(edge_ref.id())
                        .expect("Where did the edge go?!?!");
                    if graph.is_ancestor(curr_target_idx, from)
                        && !graph[resolved].package.resolved().satisfies(&requested)?
                    {
                        break 'outer;
                    }
                }
            }

            // No conflict yet. Let's try to go higher!
            target_idx = curr_target_idx;
            parent_idx = graph[curr_target_idx].parent;
        }

        // Finally, we put everything in its place.
        {
            let mut child_node = &mut graph[child_idx];
            // The parent is the _hierarchy_ location, so we set its parent
            // accordingly.
            child_node.parent = Some(target_idx);
        }
        {
            // Now we set backlinks: first, the dependent node needs to point
            // to the child, wherever it is in the graph.
            let dependent = &mut graph[dependent_idx];
            dependent.dependencies.insert(child_name.clone(), edge_idx);
        }
        {
            // Finally, we add the backlink from the parent node to the child.
            let node = &mut graph[target_idx];
            node.children.insert(child_name, child_idx);
        }
        Ok(child_idx)
    }

    fn package_deps<'b>(
        graph: &Graph,
        node_idx: NodeIndex,
        manifest: &'b CorgiManifest,
    ) -> Box<dyn Iterator<Item = ((&'b String, &'b String), DepType)> + 'b> {
        let deps = manifest
            .dependencies
            .iter()
            .map(|x| (x, DepType::Prod))
            .chain(
                manifest
                    .optional_dependencies
                    .iter()
                    .map(|x| (x, DepType::Opt)),
            )
            .chain(
                manifest
                    .peer_dependencies
                    .iter()
                    .map(|x| (x, DepType::Peer)),
            );

        if node_idx == graph.root {
            Box::new(deps.chain(manifest.dev_dependencies.iter().map(|x| (x, DepType::Dev))))
        } else {
            Box::new(deps)
        }
    }
}
