use std::{process::Stdio, sync::Arc, time::Duration};

use futures::future::join_all;
use tokio::{
    process::Command,
    sync::{
        mpsc::Sender as MpscSender,
        oneshot::{self, Receiver, Sender},
        Mutex,
    },
};
use turbo_updater::check_for_updates;
use turbopath::AbsoluteSystemPathBuf;

use crate::{commands::link::link, daemon::DaemonConnector, get_version, CommandBase};

#[derive(Debug)]
pub enum DiagnosticMessage {
    NotApplicable(String, String),
    /// internal name of diag and human readable name
    Started(String, String),
    LogLine(String, String),
    Done(String, String),
    Failed(String, String),
    /// a request to suspend terminal output. the renderer
    /// will notify the diagnostic when it is safe to render
    /// and the diagnostic notify in return when it is done
    Suspend(String, Sender<()>, Receiver<()>),
    /// a request to the user with options and a callback to send the response
    Request(String, String, Vec<String>, Sender<String>),
}

pub trait Diagnostic {
    const NAME: &'static str;

    fn execute(&self, chan: MpscSender<DiagnosticMessage>);

    // start

    async fn started(chan: &MpscSender<DiagnosticMessage>, message: String) {
        chan.send(DiagnosticMessage::Started(
            Self::NAME.to_string(),
            message.to_string(),
        ))
        .await
        .ok(); // channel closed, ignore
    }

    // loggings / interaction

    async fn log_line(chan: &MpscSender<DiagnosticMessage>, message: String) {
        chan.send(DiagnosticMessage::LogLine(
            Self::NAME.to_string(),
            message.to_string(),
        ))
        .await
        .ok(); // channel closed, ignore
    }

    async fn request(
        chan: &MpscSender<DiagnosticMessage>,
        message: String,
        options: Vec<String>,
    ) -> Option<Receiver<String>> {
        let (tx, rx) = oneshot::channel();
        chan.send(DiagnosticMessage::Request(
            Self::NAME.to_string(),
            message.to_string(),
            options,
            tx,
        ))
        .await
        .ok()?; // if the channel is closed, we can't request

        Some(rx)
    }

    async fn suspend(chan: &MpscSender<DiagnosticMessage>) -> Option<(Receiver<()>, Sender<()>)> {
        let (stopped_tx, stopped_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();

        chan.send(DiagnosticMessage::Suspend(
            Self::NAME.to_string(),
            stopped_tx,
            resume_rx,
        ))
        .await
        .ok()?; // if the channel is closed, we can't suspend

        Some((stopped_rx, resume_tx))
    }

    // types of exit

    async fn done(chan: MpscSender<DiagnosticMessage>, message: String) {
        chan.send(DiagnosticMessage::Done(
            Self::NAME.to_string(),
            message.to_string(),
        ))
        .await
        .ok(); // channel closed, ignore
    }

    async fn failed(chan: MpscSender<DiagnosticMessage>, message: String) {
        chan.send(DiagnosticMessage::Failed(
            Self::NAME.to_string(),
            message.to_string(),
        ))
        .await
        .ok(); // channel closed, ignore
    }

    async fn not_applicable(chan: MpscSender<DiagnosticMessage>, message: String) {
        chan.send(DiagnosticMessage::NotApplicable(
            Self::NAME.to_string(),
            message.to_string(),
        ))
        .await
        .ok(); // channel closed, ignore
    }
}

pub struct GitDaemonDiagnostic;

impl Diagnostic for GitDaemonDiagnostic {
    const NAME: &'static str = "git.daemon";

    fn execute(&self, chan: MpscSender<DiagnosticMessage>) {
        tokio::task::spawn(async move {
            if cfg!(unix) {
                // the git daemon is not implemented on unix
                Self::not_applicable(chan, "Git FS Monitor (not available on unix)".to_string())
                    .await;
                return;
            }

            Self::started(&chan, "Git FS Monitor".to_string()).await;

            let futures: Result<Vec<Vec<u8>>, _> = join_all(
                [
                    &["--version"][..],
                    &["config", "--get", "core.fsmonitor"][..],
                    &["config", "--get", "core.untrackedcache"][..],
                ]
                .into_iter()
                .map(|args| async move {
                    // get the current setting
                    let stdout = Stdio::piped();

                    let command = Command::new("git")
                        .args(args)
                        .stdout(stdout)
                        .spawn()
                        .expect("too many processes"); // this can only fail if we run out of processes on unix

                    command.wait_with_output().await.map(|d| d.stdout)
                }),
            )
            .await
            .into_iter()
            .collect(); // transpose

            Self::log_line(&chan, "Collecting metadata".to_string()).await;

            match futures.as_ref().map(|v| v.as_slice()) {
                Ok([version, fsmonitor, untrackedcache]) => {
                    let version = String::from_utf8_lossy(version);
                    let Some(version) = version.trim().strip_prefix("git version ") else {
                        Self::failed(chan, "Failed to get git version".to_string()).await;
                        return;
                    };

                    let (major, rest) = version.split_once('.').expect("semver");
                    let (minor, _) = rest.split_once('.').expect("semver");

                    let major = major.parse::<u32>().expect("int");
                    let minor = minor.parse::<u32>().expect("int");

                    if major == 2 && minor < 37 || major == 1 {
                        Self::failed(
                            chan,
                            format!(
                                "Git version {} is too old, please upgrade to 2.37 or newer",
                                version
                            ),
                        )
                        .await;
                        return;
                    } else {
                        Self::log_line(
                            &chan,
                            format!("Using supported Git version - {}", version).to_string(),
                        )
                        .await;
                    }

                    let fsmonitor = String::from_utf8_lossy(fsmonitor);
                    let untrackedcache = String::from_utf8_lossy(untrackedcache);

                    if fsmonitor.trim() != "true" || untrackedcache.trim() != "true" {
                        Self::log_line(&chan, "Git FS Monitor not configured".to_string()).await;
                        Self::log_line(&chan, "For more information, see https://turbo.dev/repo/docs/reference/command-line-reference/optimize".to_string()).await;
                        let Some(resp) = Self::request(
                            &chan,
                            "Configure it for this repo now?".to_string(),
                            vec!["Yes".to_string(), "No".to_string()],
                        )
                        .await
                        else {
                            // the sender (terminal) was shut, ignore
                            return;
                        };
                        match resp.await.as_ref().map(|s| s.as_str()) {
                            Ok("Yes") => {
                                Self::log_line(
                                    &chan,
                                    "Setting Git FS Monitor settings".to_string(),
                                )
                                .await;

                                let futures = [
                                    ("core.fsmonitor", fsmonitor),
                                    ("core.untrackedcache", untrackedcache),
                                ]
                                .into_iter()
                                .filter(|(_, value)| value.trim() != "true")
                                .map(|(key, _)| async {
                                    let stdout = Stdio::piped();

                                    let command = Command::new("git")
                                        .args(["config", key, "true"])
                                        .stdout(stdout)
                                        .spawn()
                                        .expect("too many processes"); // this can only fail if we run out of processes on unix

                                    command.wait_with_output().await
                                });

                                let results: Result<Vec<_>, _> =
                                    join_all(futures).await.into_iter().collect();

                                match results {
                                    Ok(_) => {
                                        Self::log_line(
                                            &chan,
                                            "Git FS Monitor settings set".to_string(),
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        Self::failed(
                                            chan,
                                            format!("Failed to set git settings: {}", e),
                                        )
                                        .await;
                                        return;
                                    }
                                }
                            }
                            Ok("No") => {
                                Self::failed(chan, "Git FS Monitor not configured".to_string())
                                    .await;
                                return;
                            }
                            Ok(_) => unreachable!(),
                            Err(_) => {
                                // the sender (terminal) was shut, ignore
                            }
                        }
                    } else {
                        Self::log_line(&chan, "Git FS Monitor settings set".to_string()).await;
                    }
                }
                Ok(_) => unreachable!(), // the vec of futures has exactly 3 elements
                Err(e) => {
                    Self::failed(chan, format!("Failed to get git version: {}", e)).await;
                    return;
                }
            }

            Self::done(chan, "Git FS Monitor Enabled".to_string()).await;
        });
    }
}

pub struct DaemonDiagnostic(pub AbsoluteSystemPathBuf);

impl Diagnostic for DaemonDiagnostic {
    const NAME: &'static str = "turbo.daemon";

    fn execute(&self, chan: MpscSender<DiagnosticMessage>) {
        let daemon_root = self.0.clone();
        tokio::task::spawn(async move {
            Self::started(&chan, "Turbo Daemon".to_string()).await;
            Self::log_line(&chan, "Connecting to daemon...".to_string()).await;
            Self::log_line(&chan, "Getting status...".to_string()).await;

            let connector = DaemonConnector {
                can_kill_server: false,
                can_start_server: true,
                pid_file: daemon_root.join_component("turbod.pid"),
                sock_file: daemon_root.join_component("turbod.sock"),
            };

            let mut client = match connector.connect().await {
                Ok(client) => client,
                Err(e) => {
                    Self::failed(chan, format!("Failed to connect to daemon: {}", e)).await;
                    return;
                }
            };

            match client.status().await {
                Ok(status) => {
                    Self::log_line(&chan, format!("Daemon up for {}ms", status.uptime_msec)).await;
                    Self::done(chan, "Daemon is running".to_string()).await;
                }
                Err(e) => {
                    Self::failed(chan, format!("Failed to get daemon status: {}", e)).await;
                }
            }
        });
    }
}

pub struct LSPDiagnostic(pub AbsoluteSystemPathBuf);
impl Diagnostic for LSPDiagnostic {
    const NAME: &'static str = "turbo.lsp";

    fn execute(&self, chan: MpscSender<DiagnosticMessage>) {
        let daemon_root = self.0.clone();
        tokio::task::spawn(async move {
            Self::started(&chan, "Turborepo Extension".to_string()).await;
            Self::log_line(&chan, "Checking if it is running...".to_string()).await;
            let lsp_root = daemon_root.join_component("lsp.pid");
            let pidlock = pidlock::Pidlock::new(lsp_root.as_std_path().to_owned());
            match pidlock.get_owner() {
                Ok(Some(pid)) => {
                    Self::done(chan, format!("Turborepo Extension is running ({})", pid)).await;
                }
                Ok(None) => {
                    Self::failed(chan, "Turborepo Extension is not running".to_string()).await;
                }
                Err(e) => {
                    Self::failed(chan, format!("Failed to get LSP status: {}", e)).await;
                }
            }
        });
    }
}

/// a struct that checks and prompts the user to enable remote cache
pub struct RemoteCacheDiagnostic(pub Arc<Mutex<CommandBase>>);
impl RemoteCacheDiagnostic {
    pub fn new(base: CommandBase) -> Self {
        Self(Arc::new(Mutex::new(base)))
    }
}

impl Diagnostic for RemoteCacheDiagnostic {
    const NAME: &'static str = "vercel.auth";

    fn execute(&self, chan: MpscSender<DiagnosticMessage>) {
        let base = self.0.clone();
        // need to spawn local since CommandBase is !Sync
        tokio::task::spawn(async move {
            let result = {
                let base = base.lock().await;
                base.config()
                    .map(|c| (c.team_id().is_some(), c.team_slug().is_some()))
            };

            let Ok((has_team_id, has_team_slug)) = result else {
                Self::failed(chan, "Malformed config file".to_string()).await;
                return;
            };

            Self::started(&chan, "Remote Cache".to_string()).await;
            Self::log_line(&chan, "Checking credentials".to_string()).await;

            if has_team_id || has_team_slug {
                Self::done(chan, "Remote Cache Enabled".to_string()).await;
                return;
            }

            let result = {
                Self::log_line(&chan, "Linking to remote cache".to_string()).await;
                let mut base = base.lock().await;
                let Some((stopped, resume)) = Self::suspend(&chan).await else {
                    // the sender (terminal) was shut, ignore
                    return;
                };
                stopped.await.unwrap();
                Self::log_line(&chan, "Ready to link".to_string()).await;
                let link_res = link(&mut base, false, crate::cli::LinkTarget::RemoteCache).await;
                Self::log_line(&chan, "Linked".to_string()).await;
                resume.send(()).unwrap();
                link_res
            };

            match result {
                Ok(_) => Self::done(chan, "Remote Cache Enabled".to_string()).await,
                Err(e) => {
                    Self::failed(chan, format!("Failed to link: {}", e)).await;
                }
            }
        });
    }
}

pub struct UpdateDiagnostic();

impl Diagnostic for UpdateDiagnostic {
    const NAME: &'static str = "turbo.update";

    fn execute(&self, chan: MpscSender<DiagnosticMessage>) {
        tokio::task::spawn(async move {
            Self::started(&chan, "Update Turborepo To Latest Version".to_string()).await;
            Self::log_line(&chan, "Checking for updates...".to_string()).await;
            let version = check_for_updates(
                "turbo",
                get_version(),
                None,
                Some(Duration::from_secs(0)), // check every time
            )
            .map_err(|e| e.to_string()); // not send

            match version {
                Ok(Some(version)) => {
                    Self::log_line(
                        &chan,
                        format!("Turborepo {} is available", version).to_string(),
                    )
                    .await;

                    let Some(resp) = Self::request(
                        &chan,
                        "Would you like to run the codemod automatically?".to_string(),
                        vec!["Yes".to_string(), "No".to_string()],
                    )
                    .await
                    else {
                        // the sender (terminal) was shut, ignore
                        return;
                    };

                    match resp.await.as_ref().map(|s| s.as_str()) {
                        Ok("Yes") => {
                            Self::log_line(&chan, "Updating Turborepo...".to_string()).await;
                            let mut command = Command::new("npx");
                            let command = command
                                .arg("--yes")
                                .arg("@turbo/codemod@latest")
                                .arg("update")
                                .stdout(Stdio::piped())
                                .spawn()
                                .expect("too many processes"); // this can only fail if we run out of processes on unix

                            match command.wait_with_output().await {
                                Ok(output) if output.status.success() => {
                                    Self::done(
                                        chan,
                                        "Turborepo Updated To Latest Version".to_string(),
                                    )
                                    .await;
                                }
                                _ => {
                                    Self::failed(chan, "Unable to update Turborepo".to_string())
                                        .await
                                }
                            }
                        }
                        Ok("No") => {
                            Self::failed(chan, "Turborepo on old version".to_string()).await
                        }
                        Ok(_) => unreachable!(), // yes and no are the only options
                        Err(_) => {
                            // the sender (terminal) was shut, ignore
                        }
                    }
                }
                // no versions in the registry, just report success
                Ok(None) => {
                    Self::done(chan, "Turborepo Updated To Latest Version".to_string()).await
                }
                Err(message) => {
                    Self::failed(chan, format!("Failed to check for updates: {}", message)).await;
                }
            }
        });
    }
}
