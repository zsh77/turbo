use std::{
    collections::{HashMap, HashSet},
    fs::File,
    sync::Arc,
    time::Instant,
};

use futures::{future::Either, stream, Stream};
use itertools::Itertools;
use notify::{Event, EventKind};
use serde_json::Value;
use tokio::{
    select,
    sync::{broadcast, oneshot, watch},
    time::error::Elapsed,
};
use turbopath::{
    AbsoluteSystemPath, AbsoluteSystemPathBuf, AnchoredSystemPath, AnchoredSystemPathBuf,
    RelativeUnixPathBuf,
};
use turborepo_filewatch::{
    cookies::{CookieError, CookieRegister, CookiedOptionalWatch},
    package_watcher::{self, PackageWatcher, WatchingPackageDiscovery},
    NotifyError, OptionalWatch,
};
use turborepo_repository::{
    change_mapper::{ChangeMapper, PackageChanges},
    discovery::WorkspaceData,
    package_graph::{self, PackageGraph, PackageName},
    package_json::PackageJson,
};
use turborepo_scm::SCM;

use crate::{
    engine::{Engine, PackageLookup, TaskDefinitionBuilder, TaskNode},
    run::{
        package_hashes::{LocalPackageHashes, PackageHasher},
        task_id::{TaskId, TaskName},
    },
    task_graph::TaskDefinition,
    task_hash::PackageInputsHashes,
    turbo_json::TurboJson,
};

pub struct PackageHashWatcher {
    repo_root: AbsoluteSystemPathBuf,
    package_watcher: Arc<PackageWatcher>,
    _handle: tokio::task::JoinHandle<()>,

    /// the subscriber will automatically stop when this is dropped
    exit_tx: oneshot::Sender<()>,

    updates: broadcast::Receiver<HashUpdate>,
    packages: CookiedOptionalWatch<
        FileHashes,
        CookiedOptionalWatch<HashMap<PackageName, WorkspaceData>, ()>,
    >,

    sub_tx: tokio::sync::mpsc::Sender<SubscriberCommand>,
}

#[derive(Clone)]
pub struct HashUpdate {
    pub package: String,
    pub task: String,
    pub hash: String,
}

enum SubscriberCommand {
    /// track the given tasks
    Update(Vec<TaskNode>),
}

#[derive(Debug, Clone)]
pub struct FileHashes(pub HashMap<TaskId<'static>, String>);

impl PackageHashWatcher {
    pub fn new(
        repo_root: AbsoluteSystemPathBuf,
        recv: OptionalWatch<broadcast::Receiver<Result<Event, NotifyError>>>,
        package_watcher: Arc<PackageWatcher>,
    ) -> Self {
        tracing::debug!("creating package hash watcher");
        let (exit_tx, exit_rx) = oneshot::channel();
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel(128);
        let subscriber = Subscriber::new(
            exit_rx,
            recv,
            package_watcher.clone(),
            repo_root.clone(),
            sub_rx,
        );
        let updates = subscriber.update_rx.resubscribe();
        let packages = subscriber.map_rx.clone();
        let handle = tokio::spawn(subscriber.watch());
        Self {
            _handle: handle,
            repo_root,
            package_watcher,
            exit_tx: exit_tx,
            updates,
            packages,
            sub_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HashUpdate> {
        self.updates.resubscribe()
    }

    pub async fn packages(&self) -> Result<FileHashes, CookieError> {
        let mut packages = self.packages.clone();
        packages.get().await.map(|i| i.to_owned())
    }

    pub async fn track(
        &self,
        tasks: Vec<TaskNode>,
    ) -> Result<PackageInputsHashes, watch::error::RecvError> {
        // in here we add the tasks to the file watcher
        let start = Instant::now();
        self.sub_tx.send(SubscriberCommand::Update(tasks)).await;
        let mut packages = self.packages.clone();
        let packages = packages.get_change().await.unwrap();
        tracing::debug!(
            "calculated {} in {}ms",
            (*packages).0.len(),
            Instant::now().duration_since(start).as_millis()
        );
        Ok(PackageInputsHashes::default())
    }
}

struct Subscriber {
    exit_rx: oneshot::Receiver<()>,
    recv: OptionalWatch<broadcast::Receiver<Result<Event, NotifyError>>>,
    package_watcher: Arc<PackageWatcher>,
    repo_root: AbsoluteSystemPathBuf,
    update_tx: broadcast::Sender<HashUpdate>,
    update_rx: broadcast::Receiver<HashUpdate>,
    map_tx: watch::Sender<Option<FileHashes>>,
    map_rx: CookiedOptionalWatch<
        FileHashes,
        // our cookie requires upstream data so we must register it
        CookiedOptionalWatch<HashMap<PackageName, WorkspaceData>, ()>,
    >,
    cookie_tx: CookieRegister,
    sub_rx: tokio::sync::mpsc::Receiver<SubscriberCommand>,
}

struct WorkspaceLookup(pub HashMap<PackageName, WorkspaceData>);
impl PackageLookup for WorkspaceLookup {
    fn package_json(&self, workspace: &PackageName) -> Option<PackageJson> {
        let data = self.0.get(workspace)?;
        let file = File::open(&data.package_json).ok()?;
        let value = serde_json::from_reader::<_, Value>(file).ok()?;
        PackageJson::from_value(value).ok()
    }

    fn package_dir(&self, workspace: &PackageName) -> Option<turbopath::AnchoredSystemPathBuf> {
        let data = self.0.get(workspace)?;
        data.package_json.parent().map(|p| p.to_owned())
    }
}

/// The underlying task that listens to file system events and updates the
/// internal package state.
impl Subscriber {
    fn new(
        exit_rx: oneshot::Receiver<()>,
        recv: OptionalWatch<broadcast::Receiver<Result<Event, NotifyError>>>,
        package_watcher: Arc<PackageWatcher>,
        repo_root: AbsoluteSystemPathBuf,
        sub_rx: tokio::sync::mpsc::Receiver<SubscriberCommand>,
    ) -> Self {
        let (update_tx, update_rx) = broadcast::channel(128);
        let (map_tx, cookie_tx, map_rx) = package_watcher.watch().new_child();
        Self {
            recv,
            exit_rx,
            package_watcher,
            repo_root,
            update_tx,
            update_rx,
            map_tx,
            map_rx,
            cookie_tx,
            sub_rx,
        }
    }

    // watching for package changes means we need to:
    //
    // a) wait for changes to the list of packages
    // b) wait for changes to the contents of those packages
    //
    // If either of those two change we need to recalculate the hashes
    // for the packages and send out an update
    #[tracing::instrument(skip(self))]
    async fn watch(mut self) {
        tracing::debug!("starting package hash watcher");
        let packages_rx = self.package_watcher.watch();

        let (package_graph_tx, mut package_graph_rx) = OptionalWatch::new();
        let package_graph_tx = Arc::new(package_graph_tx);

        let (root_package_json_tx, mut root_package_json_rx) = OptionalWatch::new();
        let root_package_json_tx = Arc::new(root_package_json_tx);

        let (root_turbo_json_tx, mut root_turbo_json_rx) = OptionalWatch::new();
        let root_turbo_json_tx = Arc::new(root_turbo_json_tx);

        let (package_hasher_tx, mut package_hasher_rx) = OptionalWatch::new();
        let package_hasher_tx = Arc::new(package_hasher_tx);

        let scm = SCM::new(&self.repo_root);

        let handle_package_list_changes = {
            let repo_root = self.repo_root.clone();
            let root_package_json_rx = root_package_json_rx.clone();
            handle_package_list_changes(
                packages_rx,
                self.package_watcher,
                repo_root,
                root_package_json_rx,
                package_graph_tx.clone(),
            )
        };

        let handle_file_update = async move {
            let mut recv = self.recv.get().await.unwrap().resubscribe();
            tracing::debug!("package hash watcher ready");

            let mut task_hashes = HashMap::<TaskId<'static>, String>::new();

            loop {
                let incoming = select! {
                    event = recv.recv() => Either::Left(event),
                    update = self.sub_rx.recv() => Either::Right(update),
                };

                match incoming {
                    Either::Left(Ok(Ok(event))) => {
                        handle_file_event(
                            event,
                            &mut task_hashes,
                            &mut package_graph_rx,
                            &mut root_package_json_rx,
                            &mut package_hasher_rx,
                            root_package_json_tx.clone(),
                            root_turbo_json_tx.clone(),
                            package_hasher_tx.clone(),
                            &self.cookie_tx,
                            &self.repo_root,
                        )
                        .await;
                    }
                    Either::Right(Some(SubscriberCommand::Update(tasks))) => {
                        let hasher = match package_hasher_rx.get_immediate() {
                            Some(Ok(hasher)) => hasher.to_owned(),
                            None | Some(Err(_)) => {
                                tracing::error!("no package graph, exiting");
                                break;
                            }
                        };
                        let hashes = hasher
                            .calculate_hashes(Default::default(), tasks)
                            .await
                            .unwrap();

                        for x in hashes.hashes {
                            task_hashes.insert(x.0, x.1);
                        }
                    }
                    Either::Left(Err(_) | Ok(Err(_))) => break,
                    Either::Right(None) => break,
                }
            }
        };

        tokio::select! {
            biased;
            _ = &mut self.exit_rx => {
                tracing::debug!("closing due to signal");

            }
            _ = handle_file_update => {
                tracing::debug!("closing due to file watcher stopping");
            }
            _ = handle_package_list_changes => {
                tracing::debug!("closing due to package list watcher stopping");
            }
        }
    }
}

#[tracing::instrument(skip(
    packages_rx,
    package_watcher,
    repo_root,
    root_package_json,
    package_graph_tx
))]
/// When the list of packages changes, we need to update the package graph
/// so that the change detector can detect the correct changes
async fn handle_package_list_changes(
    mut packages_rx: CookiedOptionalWatch<HashMap<PackageName, WorkspaceData>, ()>,
    package_watcher: Arc<PackageWatcher>,
    repo_root: AbsoluteSystemPathBuf,
    mut root_package_json: OptionalWatch<PackageJson>,
    package_graph_tx: Arc<watch::Sender<Option<PackageGraph>>>,
) {
    // we could would use a while let here, but watcher::Ref cannot be held across
    // an await so we use a loop and do the transformation in its own scope to keep
    // the borrow checker happy
    loop {
        let _workspaces = match packages_rx.get_change().await {
            Ok(workspaces) => (),
            Err(_) => {
                tracing::debug!("no package list, stopping");
                break;
            }
        };

        let root_package_json = match root_package_json.get().await {
            Ok(root_package_json) => root_package_json.to_owned(),
            Err(_) => {
                tracing::debug!("no root package json, stopping");
                break;
            }
        };

        tracing::debug!("packages changed, rebuilding package graph");

        let package_graph = PackageGraph::builder(&repo_root, root_package_json.clone());

        let res = match package_graph
            .with_package_discovery(WatchingPackageDiscovery::new(package_watcher.clone()))
            .build()
            .await
        {
            Ok(package_graph) => package_graph_tx.send(Some(package_graph)),
            Err(e) => {
                tracing::warn!("unable to build package graph: {}, disabling for now", e);
                package_graph_tx.send(None)
            }
        };

        if res.is_err() {
            tracing::debug!("no package hash listeners, stopping");
            break;
        }
    }
}

/// A file event can mean a few things:
///
/// - a file in a package was changed. we need to recalculate the hashes for the
///   tasks that depend on that package
/// - the root package json was changed. we need to recalculate the hashes for
///   all the tasks, using the new package graph / turbo json
/// - the turbo json was changed. we need to recalculate the hashes for all the
///   tasks, using the new turbo json
#[tracing::instrument(skip_all)]
async fn handle_file_event(
    event: Event,
    task_hashes: &mut HashMap<TaskId<'static>, String>,
    package_graph_rx: &mut OptionalWatch<PackageGraph>,
    root_package_json_rx: &mut OptionalWatch<PackageJson>,
    package_hasher_rx: &mut OptionalWatch<LocalPackageHashes>,
    root_package_json_tx: Arc<watch::Sender<Option<PackageJson>>>,
    root_turbo_json_tx: Arc<watch::Sender<Option<TurboJson>>>,
    package_hasher_tx: Arc<watch::Sender<Option<LocalPackageHashes>>>,
    cookie_tx: &CookieRegister,
    repo_root: &AbsoluteSystemPath,
) {
    let root_package_json_path = repo_root.join_component("package.json");
    match event.kind {
        EventKind::Any | EventKind::Access(_) | EventKind::Other => {
            // no-op
        }
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
            tracing::trace!("file event: {:?} {:?}", event.kind, event.paths);

            let turbo_json_changed = event.paths.iter().any(|p| p.ends_with("turbo.json"));

            // - reload root turbo json
            // - create a new task def builder
            // - add all the task ids
            // - recalculate the hashes for the task ids we are tracking
            if turbo_json_changed {
                root_turbo_json_tx.send(None);
                package_hasher_tx.send(None);

                let package_hasher = {
                    let Some(Ok(root_package_json)) = root_package_json_rx.get_immediate() else {
                        tracing::error!(
                            "turbo json changed, but we have no root package json, clearing \
                             downstream state"
                        );
                        return;
                    };

                    let root_turbo_json = {
                        TurboJson::load(
                            repo_root,
                            AnchoredSystemPath::empty(),
                            &root_package_json,
                            false,
                        )
                        .unwrap()
                    };

                    root_turbo_json_tx.send(Some(root_turbo_json.clone()));

                    let Some(Ok(package_graph)) = package_graph_rx.get_immediate() else {
                        tracing::error!(
                            "turbo json changed, but we have no package graph, clearing \
                             downstream state"
                        );
                        return;
                    };

                    let scm = SCM::new(&repo_root);

                    let task_definitions = create_task_defitions(
                        repo_root.to_owned(),
                        root_turbo_json.clone(),
                        &scm,
                        &*package_graph,
                    );

                    LocalPackageHashes::new(
                        scm,
                        package_graph
                            .workspaces()
                            .map(|(k, v)| (k.to_owned(), v.package_json_path.to_owned()))
                            .collect(),
                        task_definitions
                            .into_iter()
                            .map(|(k, v)| (k, v.into()))
                            .collect(),
                        repo_root.to_owned(),
                    )
                };

                let hashes = package_hasher
                    .calculate_hashes(
                        Default::default(),
                        task_hashes
                            .keys()
                            .cloned()
                            .map(|id| TaskNode::Task(id))
                            .collect(),
                    )
                    .await
                    .unwrap();

                for x in hashes.hashes {
                    task_hashes.insert(x.0, x.1);
                }

                package_hasher_tx.send(Some(package_hasher));
            }

            let package_json_change = event
                .paths
                .iter()
                .find(|p| p.as_path() == root_package_json_path.as_std_path());

            if let Some(root_package_json_path) = package_json_change {
                let root_package_json = {
                    let Ok(root_package_json) = File::open(root_package_json_path) else {
                        tracing::error!("unable to open root package json, exiting");
                        return;
                    };

                    let Ok(root_package_json) =
                        serde_json::from_reader::<_, Value>(root_package_json)
                    else {
                        tracing::error!("unable to parse root package json, exiting");
                        return;
                    };

                    match PackageJson::from_value(root_package_json) {
                        Ok(root_package_json) => root_package_json,
                        Err(e) => {
                            tracing::error!("unable to parse root package json: {}, exiting", e);
                            return;
                        }
                    }
                };

                root_package_json_tx.send(Some(root_package_json));
            }

            let changed_packages = {
                let Ok(package_graph) = package_graph_rx.get().await else {
                    tracing::error!("package graph not available, exiting");
                    return;
                };
                let change_mapper = ChangeMapper::new(&package_graph, vec![], vec![]);
                change_mapper
                    .changed_packages(
                        event
                            .paths
                            .iter()
                            .map(|p| {
                                AnchoredSystemPathBuf::new(
                                    repo_root,
                                    AbsoluteSystemPathBuf::new(p.to_string_lossy()).unwrap(),
                                )
                                .unwrap()
                            })
                            .collect(),
                        None,
                    )
                    .unwrap()
            };

            // recalculate the hashes for the changed packages

            // for all the tasks that we have for those packages,
            // recalculate the hashes

            let changed_tasks: Vec<_> = match changed_packages {
                PackageChanges::All => task_hashes.keys().collect(),
                PackageChanges::Some(data) => task_hashes
                    .keys()
                    .filter(|t| {
                        data.iter()
                            .find(|w| w.name == PackageName::Other(t.package().to_string()))
                            .is_some()
                    })
                    .collect(),
            };

            if changed_tasks.is_empty() {
                return;
            }

            tracing::debug!(
                "tasks {:?} changed",
                changed_tasks.iter().map(|t| t.to_string()).join(", ")
            );

            let hasher = match package_hasher_rx.get_immediate() {
                Some(Ok(hasher)) => hasher.to_owned(),
                None | Some(Err(_)) => {
                    tracing::error!("no package graph, exiting");
                    return;
                }
            };

            let hashes = hasher
                .calculate_hashes(
                    Default::default(),
                    changed_tasks
                        .into_iter()
                        .map(|t| TaskNode::Task(t.to_owned()))
                        .collect(),
                )
                .await
                .unwrap();

            let left: HashSet<_> = hashes.hashes.keys().collect();
            let right: HashSet<_> = task_hashes.keys().collect();

            let updated = left.intersection(&right).map(|k| k.to_string()).join(", ");
            let added = left.difference(&right).map(|k| k.to_string()).join(", ");

            tracing::debug!("added {} and updated {}", added, updated);

            task_hashes.extend(hashes.hashes);

            // finally update the cookie
            cookie_tx.register(
                &event
                    .paths
                    .iter()
                    .map(|p| AbsoluteSystemPath::from_std_path(p).expect("absolute"))
                    .collect::<Vec<_>>(),
            );
        }
    }
}

fn create_task_defitions(
    repo_root: AbsoluteSystemPathBuf,
    root_turbo_json: TurboJson,
    scm: &SCM,
    workspaces: &PackageGraph,
) -> HashMap<TaskId<'static>, TaskDefinition> {
    let mut task_definitions = TaskDefinitionBuilder::new(repo_root.clone(), workspaces, false);

    let mut turbo_jsons = [(PackageName::Root, root_turbo_json.clone())]
        .into_iter()
        .collect();

    for task_id in workspaces
        .workspaces()
        .cartesian_product(root_turbo_json.pipeline.keys())
        .map(|((package, _), task)| {
            task.task_id()
                .unwrap_or_else(|| TaskId::new(package.as_ref(), task.task()))
                .into_owned()
        })
        .unique()
    {
        task_definitions.add_task_definition_from(&mut turbo_jsons, &task_id);
    }

    task_definitions.build()
}
