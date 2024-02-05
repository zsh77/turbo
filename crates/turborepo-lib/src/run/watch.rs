use futures::StreamExt;

use crate::{commands::CommandBase, run::Error, DaemonConnector};

pub struct WatchClient {}

impl WatchClient {
    pub async fn start(base: &CommandBase) -> Result<(), Error> {
        let pid_file = base.daemon_file_root().join_component("turbod.pid");
        let sock_file = base.daemon_file_root().join_component("turbod.sock");

        let connector = DaemonConnector {
            can_start_server: true,
            can_kill_server: true,
            pid_file: pid_file.clone(),
            sock_file: sock_file.clone(),
        };

        let mut client = connector.connect().await?;

        let mut hashes = client.subscribe_package_hashes().await?;
        while let Some(hash) = hashes.next().await {
            let hash = hash.unwrap();
            println!("hash: {:?}", hash);
        }

        Ok(())
    }
}
