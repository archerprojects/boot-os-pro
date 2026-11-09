// Boot OS Pro — helper_client.rs — build 1.0.2 — Monday 9 November 2026
//
// App side of the helper protocol (see src/bin/bootospro-helper.rs). Starts
// the helper through pkexec once per operation, sends the job, relays its
// progress to the frontend as `write_progress`, and forwards Cancel.

use crate::error::{BootOsProError, Result};
use crate::jobs::{Job, JobRequest};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};

const HELPER: &str = "/usr/lib/bootospro/bootospro-helper";

/// stdin of the running helper, kept open for the life of the job so Cancel
/// can reach it. Dropping it (EOF) also tells the helper to stop.
static RUNNING: Mutex<Option<ChildStdin>> = Mutex::new(None);

/// Run one job in the privileged helper and return its JSON result.
pub fn run_job(app: &AppHandle, job: Job) -> Result<serde_json::Value> {
    let req = JobRequest {
        app_version: app.package_info().version.to_string(),
        job,
    };
    let line = serde_json::to_string(&req)
        .map_err(|e| BootOsProError::Other(format!("encode job: {e}")))?;

    let mut child = Command::new("pkexec")
        .arg(HELPER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        if let Ok(mut slot) = RUNNING.lock() {
            *slot = Some(stdin);
        }
    }

    // Drain stderr concurrently — a blocked stderr pipe would stall the helper.
    let stderr_handle = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = e.read_to_string(&mut buf);
            buf
        })
    });

    let mut outcome: Option<Result<serde_json::Value>> = None;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            match v["type"].as_str() {
                Some("progress") => {
                    let _ = app.emit(
                        "write_progress",
                        serde_json::json!({ "stage": v["stage"], "pct": v["pct"], "msg": v["msg"] }),
                    );
                }
                Some("done") => outcome = Some(Ok(v["result"].clone())),
                Some("error") => {
                    outcome = Some(Err(if v["cancelled"].as_bool() == Some(true) {
                        BootOsProError::Cancelled
                    } else {
                        BootOsProError::Other(v["message"].as_str().unwrap_or("").to_string())
                    }))
                }
                _ => {}
            }
        }
    }

    if let Ok(mut slot) = RUNNING.lock() {
        *slot = None;
    }
    let status = child.wait()?;
    let stderr = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();

    outcome.unwrap_or_else(|| {
        // No result line: pkexec refused (126 dismissed, 127 not authorised)
        // or the helper died before reporting.
        Err(match status.code() {
            Some(126) | Some(127) => BootOsProError::Other(
                "Administrator authorisation was not granted — nothing was changed.".into(),
            ),
            _ => BootOsProError::CommandFailed { cmd: "bootospro-helper".into(), stderr },
        })
    })
}

/// Ask the running helper to stop. The job's own result (Cancelled) arrives
/// through `run_job` once it has stopped and cleaned up.
pub fn cancel() {
    if let Ok(mut slot) = RUNNING.lock() {
        if let Some(stdin) = slot.as_mut() {
            let _ = writeln!(stdin, "cancel");
            let _ = stdin.flush();
        }
    }
}
