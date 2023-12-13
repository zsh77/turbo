use std::{sync::LazyLock, time::Duration};

use console::{style, Style};
use turborepo_ui::*;

use super::CommandBase;
use crate::diagnostics::*;

// can't use LazyCell since DialoguerTheme isn't Sync
static DIALOGUER_THEME: LazyLock<DialoguerTheme> = LazyLock::new(|| DialoguerTheme {
    prompt_prefix: style(">>>".to_string()).bright().for_stderr(),
    active_item_prefix: style("  ❯".to_string()).for_stderr().green(),
    inactive_item_prefix: style("   ".to_string()).for_stderr(),
    success_prefix: style("   ".to_string()).for_stderr(),
    prompt_style: Style::new().for_stderr(),
    ..Default::default()
});

pub async fn run(base: CommandBase) {
    let ui = base.ui;

    println!("\n{}\n", ui.rainbow(">>> TURBOCHARGE"));
    println!(
        "Turborepo does a lot of work behind the scenes to make your monorepo fast,
however there are some things you can do to make it even faster. {}\n",
        BOLD_GREEN.apply_to("Let's go!")
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    DaemonDiagnostic(base.daemon_file_root()).execute(tx.clone());
    LSPDiagnostic(base.daemon_file_root()).execute(tx.clone());
    GitDaemonDiagnostic.execute(tx.clone());
    RemoteCacheDiagnostic::new(base).execute(tx.clone());

    const NUM_TASKS: usize = 4;

    let mut complete = 0;
    let mut failed = 0;
    let mut not_applicable = 0;

    let mut state = None;

    while let Some(message) = rx.recv().await {
        use DiagnosticMessage::*;
        match (message, &state) {
            (Started(id, name), None) => {
                let bar = start_spinner(&name);
                state = Some((id, bar));
            }
            (Started(id1, _), Some((id2, _))) if id1 == *id2 => {}
            (LogLine(id1, line), Some((id2, bar))) if id1 == *id2 => {
                bar.println(format!("    {}", GREY.apply_to(line)));
            }
            (Done(id1, message), Some((id2, bar))) if id1 == *id2 => {
                bar.finish_with_message(BOLD_GREEN.apply_to(message).to_string());
                complete += 1;
                state = None;
            }
            (NotApplicable(_, name), None) => {
                let bar = start_spinner(&name);
                let n_a = GREY.apply_to("n/a").to_string();
                let style = bar.style().tick_strings(&[&n_a, &n_a]);
                bar.set_style(style);
                bar.finish_with_message(format!("{}", BOLD_GREY.apply_to(name)));
                not_applicable += 1;
            }
            (NotApplicable(id1, name), Some((id2, bar))) if id1 == *id2 => {
                let n_a = GREY.apply_to("n/a").to_string();
                let style = bar.style().tick_strings(&[&n_a, &n_a]);
                bar.set_style(style);
                bar.finish_with_message(format!("{}", BOLD_GREY.apply_to(name)));
                not_applicable += 1;
            }
            (Failed(id1, message), Some((id2, bar))) if id1 == *id2 => {
                bar.finish_with_message(BOLD_RED.apply_to(message).to_string());
                failed += 1;
                state = None;
            }
            (Request(id1, prompt, mut options, chan), Some((id2, bar))) if id1 == *id2 => {
                let opt = bar.suspend(|| {
                    dialoguer::Select::with_theme(&*DIALOGUER_THEME)
                        .with_prompt(prompt)
                        .items(&options)
                        .default(0)
                        .interact()
                        .unwrap()
                });

                chan.send(options.swap_remove(opt)).unwrap();
            }
            (Suspend(id1, stopped, resume), Some((id2, bar))) if id1 == *id2 => {
                let bar = bar.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    bar.suspend(|| {
                        resume.blocking_recv().ok(); // sender is dropped, so we
                                                     // can unsuspend
                    });
                });
                stopped.send(()).ok(); // suspender doesn't need to be notified so failing ok
                handle.await.unwrap();
            }
            // any interleaved events will be requeued such that we only process one id at a
            // time. we list them explicitly to support exhaustiveness checking
            (
                event @ (Failed(..) | Done(..) | LogLine(..) | NotApplicable(..) | Started(..)
                | Request(..) | Suspend(..)),
                _,
            ) => {
                tx.send(event).await.unwrap();
            }
        }
        if complete + not_applicable + failed == NUM_TASKS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    if complete + not_applicable == NUM_TASKS {
        println!("\n\n{}", ui.rainbow(">>> FULLY TURBOCHARGED"));
    } else {
        println!("\n\n>>> NOT TURBOCHARGED (yet)");
    }
}
