//! Explicit migration of one closed legacy conversation; no model calls.
use super::Cli;
use std::path::Path;
use std::process::ExitCode;

pub(super) fn parse(mut args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut thread_id = None;
    let mut json = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--thread-id" if thread_id.is_none() => {
                thread_id = Some(args.next().ok_or("--thread-id requires a value")?);
            }
            "--json" => json = true,
            _ => return Err(format!("unknown migrate-history option: {arg}")),
        }
    }
    Ok(Cli::MigrateHistory {
        thread_id: thread_id.ok_or("migrate-history requires --thread-id")?,
        json,
    })
}

pub(super) fn run(thread_id: &str, as_json: bool) -> Result<ExitCode, String> {
    let auth = crate::http::auth::codex_auth_path();
    let home = auth.parent().unwrap_or_else(|| Path::new("."));
    let (lock, report) = emp_codex::history::repair::repair_thread_before_native_resume(
        home, thread_id,
    )
    .map_err(|error| {
        if error.reason() == "active_codex_writer" {
            "Close the selected Codex conversation before migrating it; its history was not changed"
                .to_owned()
        } else {
            format!("EMP conversation migration stopped: {}", error.reason())
        }
    })?;
    drop(lock);
    if as_json {
        println!(
            "{}",
            serde_json::json!({"thread_id":thread_id,"threads_repaired":report.threads_repaired,
            "checkpoints_converted":report.checkpoints_converted})
        );
    } else {
        println!(
            "Conversation: {thread_id}\nThreads migrated: {}\nCheckpoints converted: {}",
            report.threads_repaired, report.checkpoints_converted
        );
    }
    Ok(ExitCode::SUCCESS)
}
