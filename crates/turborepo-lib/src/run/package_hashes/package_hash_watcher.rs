use std::{
    collections::{HashMap, HashSet},
    fs::File,
    sync::Arc,
    time::Instant,
};

use futures::future::Either;
use itertools::Itertools;
use notify::{Event, EventKind};
use serde_json::Value;
use tokio::{
    select,
    sync::{broadcast, oneshot, watch},
};
use turbopath::{
    AbsoluteSystemPath, AbsoluteSystemPathBuf, AnchoredSystemPath, AnchoredSystemPathBuf,
};
use turborepo_filewatch::{
    cookies::{CookieError, CookieRegister, CookiedOptionalWatch},
    package_watcher::{PackageManagerState, PackageWatcher},
    NotifyError, OptionalWatch,
};
use turborepo_repository::{
    change_mapper::{ChangeMapper, PackageChanges},
    discovery::{StaticPackageDiscovery, WorkspaceData},
    package_graph::{PackageGraph, PackageName},
    package_json::PackageJson,
};
use turborepo_scm::SCM;

use crate::{
    engine::{self, PackageLookup, TaskDefinitionBuilder, TaskNode},
    run::{
        package_hashes::{LocalPackageHashes, PackageHasher},
        task_id::TaskId,
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

    pub async fn track(&self, tasks: Vec<TaskNode>) -> Result<PackageInputsHashes, TrackError> {
        // in here we add the tasks to the file watcher
        let start = Instant::now();
        self.sub_tx
            .send(SubscriberCommand::Update(tasks))
            .await
            .map_err(|_| TrackError::Send)?;
        let mut packages = self.packages.clone();
        let packages = packages.get_change().await.map_err(|_| TrackError::Recv)?;
        tracing::debug!(
            "calculated {} in {}ms",
            (*packages).0.len(),
            Instant::now().duration_since(start).as_millis()
        );
        Ok(PackageInputsHashes::default())
    }
}

pub enum TrackError {
    Send,
    Recv,
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
        let package_manager_rx = self.package_watcher.watch_manager();

        let (package_graph_tx, mut package_graph_rx) = OptionalWatch::new();
        let package_graph_tx = Arc::new(package_graph_tx);

        let (root_package_json_tx, mut root_package_json_rx) = OptionalWatch::new();
        let root_package_json_tx = Arc::new(root_package_json_tx);

        let (root_turbo_json_tx, root_turbo_json_rx) = OptionalWatch::new();
        let root_turbo_json_tx = Arc::new(root_turbo_json_tx);

        let (package_hasher_tx, mut package_hasher_rx) = OptionalWatch::new();
        let package_hasher_tx = Arc::new(package_hasher_tx);

        let (task_hashes_tx, task_hashes_rx) = OptionalWatch::new();
        let task_hashes_tx = Arc::new(task_hashes_tx);

        let scm = SCM::new(&self.repo_root);

        let update_package_graph_fut = {
            let repo_root = self.repo_root.clone();
            let root_package_json_rx = root_package_json_rx.clone();
            update_package_graph(
                repo_root,
                packages_rx,
                package_manager_rx,
                root_package_json_rx,
                package_graph_tx.clone(),
            )
        };

        let update_package_hasher_fut = {
            let repo_root = self.repo_root.clone();
            // new unknown tasks need to be hashed the first time
            update_package_hasher(
                scm.clone(),
                repo_root,
                root_turbo_json_rx.clone(),
                package_graph_rx.clone(),
                task_hashes_rx.clone(),
                task_hashes_tx.clone(),
                package_hasher_tx.clone(),
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
                            let existing = task_hashes.insert(x.0.clone(), x.1.clone());
                            if let Some(existing) = existing {
                                if existing != x.1 {
                                    // this should hopefully never happen. if calculating hashes for
                                    // a task gives a different result than what we have stored, we
                                    // have done a bad job of tracking the state of the world
                                    tracing::error!(
                                        "hash for task {} changed from {} to {}",
                                        x.0,
                                        existing,
                                        x.1
                                    );
                                }
                            }
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
            _ = update_package_graph_fut => {
                tracing::debug!("closing due to package list watcher stopping");
            }
            _ = update_package_hasher_fut => {
                tracing::debug!("closing due to package hasher stopping");
            }
        }
    }
}

/// When the list of packages changes, or the root package json chagnes, we need
/// to update the package graph so that the change detector can detect the
/// correct changes
#[tracing::instrument(skip_all)]
async fn update_package_graph(
    repo_root: AbsoluteSystemPathBuf,
    mut packages_rx: CookiedOptionalWatch<HashMap<PackageName, WorkspaceData>, ()>,
    mut package_manager_rx: CookiedOptionalWatch<PackageManagerState, ()>,
    mut root_package_json_rx: OptionalWatch<PackageJson>,
    package_graph_tx: Arc<watch::Sender<Option<PackageGraph>>>,
) {
    // we could would use a while let here, but watcher::Ref cannot be held across
    // an await so we use a loop and do the transformation in its own scope to keep
    // the borrow checker happy
    loop {
        let changed = select! {
            out = packages_rx.get_change() => {
                let Ok(packages) = out else {
                    // we will never get another update, so we should stop
                    tracing::debug!("no package list, stopping");
                    break;
                };

                Either::Left(packages.to_owned())
            },
            out = root_package_json_rx.get_change() => {
                let Ok(root_package_json) = out else {
                    // we will never get another update, so we should stop
                    tracing::debug!("no root package json, stopping");
                    break;
                };

                Either::Right(root_package_json.to_owned())
            },
        };

        // we don't actually need `packages` here, since we fetch them again
        // from the package_discovery`
        let (packages, root_package_json) = match changed {
            Either::Left(packages) => (
                packages,
                root_package_json_rx
                    .get_immediate()
                    .unwrap()
                    .unwrap()
                    .to_owned(),
            ),
            Either::Right(root_package_json) => (
                packages_rx
                    .get_immediate()
                    .await
                    .unwrap()
                    .unwrap()
                    .to_owned(),
                root_package_json,
            ),
        };

        let package_manager = match package_manager_rx.get_immediate().await {
            Some(Ok(package_manager)) => package_manager.manager,
            None | Some(Err(_)) => {
                tracing::error!("no package manager, exiting");
                break;
            }
        };

        tracing::debug!("packages changed, rebuilding package graph");

        let package_graph = PackageGraph::builder(&repo_root, root_package_json.clone());

        let res = match package_graph
            .with_package_discovery(StaticPackageDiscovery::new(
                packages.into_values().collect(),
                package_manager,
            ))
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

            if turbo_json_changed {
                // no use in updating the turbo json if there are no listeners
                if let Ok(_) = root_turbo_json_tx.send(None) {
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

                    // we don't really need to exit here, since other watchers will terminate for us
                    _ = root_turbo_json_tx.send(Some(root_turbo_json.clone()));
                }
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

                // we don't really need to exit here, since other watchers will terminate for us
                _ = root_package_json_tx.send(Some(root_package_json));
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

/// When the list of packages changes, or the root package json chagnes, we need
/// to update the package hasher so that it knows the correct globs and task
/// definitions. Additionally, we need to replace all the task ids that we are
/// tracking with the new ones.
#[tracing::instrument(skip_all)]
async fn update_package_hasher(
    scm: SCM,
    repo_root: AbsoluteSystemPathBuf,
    mut root_turbo_json_rx: OptionalWatch<TurboJson>,
    mut package_graph_rx: OptionalWatch<PackageGraph>,
    mut task_hashes_rx: OptionalWatch<HashMap<TaskId<'static>, String>>,
    task_hashes_tx: Arc<watch::Sender<Option<HashMap<TaskId<'static>, String>>>>,
    package_hasher_tx: Arc<watch::Sender<Option<LocalPackageHashes>>>,
) {
    loop {
        let package_hasher = {
            // we don't actually care about task hashes changing, it is down
            // stream data that we need to update, we just need to read it to
            // be able to update
            let (mut root_turbo_json, mut package_graph) = select! {
                root_turbo_json = root_turbo_json_rx.get_change() => {
                    if let Ok(root_turbo_json) = root_turbo_json {
                        (Some((*root_turbo_json).to_owned()), None)
                    } else {
                        tracing::debug!("no root turbo json, exiting");
                        break;
                    }
                }
                package_graph = package_graph_rx.get_change() => {
                    if let Ok(package_graph) = package_graph {
                        (None, Some(package_graph))
                    } else {
                        tracing::debug!("no package graph, exiting");
                        break;
                    }
                }
            };

            if root_turbo_json.is_none() {
                root_turbo_json = match root_turbo_json_rx.get_immediate() {
                    Some(Ok(root_turbo_json)) => Some(root_turbo_json.to_owned()),
                    None | Some(Err(_)) => {
                        tracing::error!(
                            "turbo json changed, but we have no root turbo json, clearing \
                             downstream state"
                        );
                        return;
                    }
                }
            };

            if package_graph.is_none() {
                drop(package_graph);
                package_graph = match package_graph_rx.get_immediate() {
                    Some(Ok(package_graph)) => Some(package_graph),
                    None | Some(Err(_)) => {
                        tracing::error!(
                            "turbo json changed, but we have no package graph, clearing \
                             downstream state"
                        );
                        return;
                    }
                }
            };

            // we validate above that these are all Some
            let (root_turbo_json, package_graph) =
                (root_turbo_json.unwrap(), package_graph.unwrap());

            let task_definitions = match create_task_definitions(
                repo_root.to_owned(),
                root_turbo_json.clone(),
                &*package_graph,
            ) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("unable to create task definitions: {e}");
                    break;
                }
            };

            let package_hasher = LocalPackageHashes::new(
                scm.clone(),
                package_graph
                    .packages()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                task_definitions
                    .into_iter()
                    .map(|(k, v)| (k, v.into()))
                    .collect(),
                repo_root.to_owned(),
            );

            package_hasher
        };

        // here, if the task_hashes_rx is empty, we can just exit
        let task_hashes = match task_hashes_rx.get_immediate() {
            Some(Ok(task_hashes)) => Some(
                task_hashes
                    .keys()
                    .cloned()
                    .map(|id| TaskNode::Task(id))
                    .collect(),
            ),
            None | Some(Err(_)) => {
                tracing::debug!("no task hashes to update");
                None
            }
        };

        let (package_hasher, hashes) = if let Some(task_hashes) = task_hashes {
            // no one is listening (nor ever will be), so we can just exit
            if task_hashes_tx.send(None).is_err() {
                tracing::debug!("no task hashes to update, exiting");
                return;
            }

            let Ok(hashes) = package_hasher
                .calculate_hashes(Default::default(), task_hashes)
                .await
            else {
                // if we can't calculate the hashes, leave the task hasher empty
                return;
            };

            (Some(package_hasher), Some(hashes.hashes))
        } else {
            (None, None)
        };

        // if either of these fail, then the thing that is processing hash updates
        // has stopped, or the thing that is serving the task hashes has stopped,
        // so we can just exit

        if task_hashes_tx.send(hashes).is_err() {
            tracing::debug!("no task hashes to update, exiting");
            return;
        }

        if package_hasher_tx.send(package_hasher).is_err() {
            tracing::debug!("nobody needing a package hasher, exiting");
            return;
        }
    }
}

fn create_task_definitions(
    repo_root: AbsoluteSystemPathBuf,
    root_turbo_json: TurboJson,

    workspaces: &PackageGraph,
) -> Result<HashMap<TaskId<'static>, TaskDefinition>, engine::BuilderError> {
    let mut task_definitions = TaskDefinitionBuilder::new(repo_root.clone(), workspaces, false);

    let mut turbo_jsons = [(PackageName::Root, root_turbo_json.clone())]
        .into_iter()
        .collect();

    for task_id in workspaces
        .packages()
        .cartesian_product(root_turbo_json.pipeline.keys())
        .map(|((package, _), task)| {
            task.task_id()
                .unwrap_or_else(|| TaskId::new(package.as_ref(), task.task()))
                .into_owned()
        })
        .unique()
    {
        task_definitions.add_task_definition_from(&mut turbo_jsons, &task_id)?;
    }

    Ok(task_definitions.build())
}
