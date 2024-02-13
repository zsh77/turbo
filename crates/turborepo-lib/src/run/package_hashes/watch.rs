use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::{FutureExt, StreamExt};
use tokio::{sync::watch::error::RecvError, time::error::Elapsed};
use turborepo_filewatch::OptionalWatch;
use turborepo_repository::discovery::PackageDiscovery;
use turborepo_telemetry::events::generic::GenericEventBuilder;

use super::PackageHasherBuilder;
use crate::{
    daemon::FileWatching,
    engine::TaskNode,
    run::{package_hashes::PackageHasher, task_id::TaskId, Error},
    task_hash::PackageInputsHashes,
};

/// WatchingPackageHasher is a wrapper around a `PackageHashWatcher` that
/// fields requests for package hashes and returns the latest known hashes
/// for the requested packages.
pub struct WatchingPackageHasher<PD> {
    package_discovery: PD,
    interval: Duration,

    file_watching: FileWatching,
}

#[derive(thiserror::Error, Debug)]
enum WaitError {
    #[error(transparent)]
    Elapsed(#[from] Elapsed),
    #[error(transparent)]
    Unavailable(#[from] RecvError),
}

impl<PD> WatchingPackageHasher<PD> {
    pub fn new(package_discovery: PD, interval: Duration, file_watching: FileWatching) -> Self {
        Self {
            interval,
            package_discovery,
            file_watching,
        }
    }
}

impl<PD: PackageDiscovery + Send + Sync> PackageHasher for WatchingPackageHasher<PD> {
    async fn calculate_hashes(
        &self,
        _run_telemetry: GenericEventBuilder,
        tasks: Vec<TaskNode>,
    ) -> Result<PackageInputsHashes, Error> {
        let data = self
            .file_watching
            .package_hash_watcher
            .track(tasks)
            .await
            .unwrap();
        Ok(data)
    }
}
