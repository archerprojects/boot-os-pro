// Boot OS Pro — bootospro-helper.rs — build 1.0.2 — Monday 9 November 2026
//
// The privileged helper. Installed at /usr/lib/bootospro/bootospro-helper and
// started by the app through pkexec — once per operation, so one password
// prompt covers the whole write however long it runs.
//
// Protocol (all JSON, one object per line):
//   stdin,  first line : a JobRequest
//   stdin,  later lines: "cancel" — stop the job; EOF (app gone) also cancels
//   stdout             : {"type":"progress","stage","pct","msg"} …
//                        then exactly one {"type":"done","result"}
//                        or {"type":"error","message","cancelled"}

use bootospro_lib::error::BootOsProError;
use bootospro_lib::jobs::{self, JobRequest, Progress};
use std::io::{BufRead, Write};

struct StdoutProgress;

impl Progress for StdoutProgress {
    fn emit(&self, stage: &str, pct: u8, msg: &str) {
        send(serde_json::json!({ "type": "progress", "stage": stage, "pct": pct, "msg": msg }));
    }
}

fn send(v: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn fail(message: String, cancelled: bool) -> ! {
    send(serde_json::json!({ "type": "error", "message": message, "cancelled": cancelled }));
    std::process::exit(1);
}

fn main() {
    if !bootospro_lib::disk::is_root() {
        fail("bootospro-helper must be started by Boot OS Pro through pkexec".into(), false);
    }

    let mut first = String::new();
    if std::io::stdin().lock().read_line(&mut first).unwrap_or(0) == 0 {
        fail("no job received".into(), false);
    }
    let req: JobRequest = match serde_json::from_str(first.trim()) {
        Ok(r) => r,
        Err(e) => fail(format!("invalid job: {e}"), false),
    };

    // Watch stdin for "cancel" for the life of the job. EOF means the app is
    // gone: stop and clean up rather than keep writing unattended.
    std::thread::spawn(|| {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(l) if l.trim() == "cancel" => jobs::request_cancel(),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        jobs::request_cancel();
    });

    match jobs::run(req, &StdoutProgress) {
        Ok(result) => send(serde_json::json!({ "type": "done", "result": result })),
        Err(e) => {
            let cancelled = matches!(e, BootOsProError::Cancelled);
            fail(e.to_string(), cancelled);
        }
    }
}
