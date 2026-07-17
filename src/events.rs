//! Live run events: one JSON object per line, always appended to the run
//! directory as `events.jsonl` and, with `--stream`, mirrored to stdout. This
//! stream is the integration surface for anything that wants to watch a fleet
//! live — an orchestrating session, an IDE, `tail -f` — without linking
//! against summoner or waiting for the final report.
//!
//! Line shape: `{"ts":<unix secs>,"event":"<name>", ...event fields}`.
//! Events: run_started, order_started, order_dispatched, order_exec_done,
//! order_verify, order_finished, run_finished — and, on stdout in stream mode
//! only, a final `report` event carrying the complete ranked report.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct EventSink {
    file: Option<Mutex<File>>,
    stream_stdout: bool,
}

impl EventSink {
    /// Best-effort, like grove's event log: failing to open the sidecar must
    /// not fail the run it describes.
    pub fn new(run_dir: &Path, stream_stdout: bool) -> Self {
        let file = File::create(run_dir.join("events.jsonl"))
            .ok()
            .map(Mutex::new);
        EventSink {
            file,
            stream_stdout,
        }
    }

    pub fn emit(&self, event: &str, mut fields: serde_json::Value) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut line = serde_json::json!({"ts": ts, "event": event});
        if let (Some(object), Some(extra)) = (line.as_object_mut(), fields.as_object_mut()) {
            object.append(extra);
        }
        let line = line.to_string();
        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "{line}");
        }
        if self.stream_stdout {
            // println! locks stdout per call, so worker threads cannot
            // interleave inside a line.
            println!("{line}");
        }
    }

    pub fn streaming(&self) -> bool {
        self.stream_stdout
    }
}
