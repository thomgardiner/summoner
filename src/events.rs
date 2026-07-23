//! The run journal: one JSON object per line, appended as `events.jsonl` and,
//! with `--stream`, mirrored to stdout. It is authoritative — `report.json` is
//! projected from it, not an in-memory vector — and the live integration surface
//! for watching a fleet (a session, an IDE, `tail -f`) without linking summoner.
//!
//! Line shape: an envelope `{"schema_version":1,"run_id":<id>,"seq":<n>,
//! "ts":<secs>,"event":<name>, ...fields}`, `seq` contiguous from zero. Sequence
//! allocation, serialization, the whole-line append, and its flush share one
//! mutex; a record reaches `--stream` stdout only after its line is flushed. Any
//! create, lock, serialize, write, or flush failure is recorded and returned so
//! the run stops and never publishes a report from unrecorded work.
//!
//! Events: run_started, order_carried, order_started, order_dispatched,
//! order_exec_done, order_revised, order_verify, review_started, order_review,
//! order_checkpoint, order_finished, run_finished — plus a stream-only final
//! `report` carrying the ranked report. Checkpoints preserve the full gate result
//! before cleanup; order_carried and order_finished are terminal transitions.

use crate::notify::{Notifier, NotifyPlan};
use crate::report::OrderReport;
use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;

pub struct EventSink {
    journal: Mutex<Journal>,
    run_id: String,
    stream_stdout: bool,
    /// The notification side-channel, present only when a notify command is
    /// configured. Records are handed to a single worker thread so a slow
    /// command never serializes the fleet behind the journal lock.
    notify: Option<NotifyChannel>,
}

/// One background worker draining notify records. Dropping the sender ends the
/// worker's `recv` loop; [`EventSink`]'s `Drop` then joins it so the final
/// `run_finished` notification completes before the process exits.
struct NotifyChannel {
    sender: Sender<(NotifyPlan, String)>,
    worker: JoinHandle<()>,
}

struct Journal {
    file: File,
    seq: u64,
    /// The first serialize/write/flush failure. Once set, every later emit
    /// short-circuits and the run refuses to publish report.json.
    failed: Option<String>,
}

impl EventSink {
    /// The journal is authoritative, so a run that cannot create it must fail
    /// before any work dispatches rather than silently degrade to no evidence.
    pub fn new(run_dir: &Path, run_id: String, stream_stdout: bool) -> Result<Self> {
        let file = File::create(run_dir.join("events.jsonl"))
            .with_context(|| format!("creating run journal in {}", run_dir.display()))?;
        Ok(EventSink {
            journal: Mutex::new(Journal {
                file,
                seq: 0,
                failed: None,
            }),
            run_id,
            stream_stdout,
            notify: None,
        })
    }

    /// Attach the notify side-channel, spawning its worker thread. Off (no
    /// thread) when `notifier` is `None`; a builder so the many test call sites
    /// that never notify keep the simple `new` signature.
    pub fn with_notifier(mut self, notifier: Option<Notifier>) -> Self {
        if let Some(notifier) = notifier {
            let (sender, receiver) = std::sync::mpsc::channel::<(NotifyPlan, String)>();
            if let Ok(worker) = std::thread::Builder::new()
                .name("summoner-notify".into())
                .spawn(move || {
                    while let Ok((plan, line)) = receiver.recv() {
                        notifier.dispatch(&plan, &line);
                    }
                })
            {
                self.notify = Some(NotifyChannel { sender, worker });
            }
        }
        self
    }

    /// Append one enveloped record, then mirror it to `--stream` stdout only
    /// after the flush. A serialize/write/flush failure is recorded and returned.
    pub fn emit(&self, event: &str, fields: serde_json::Value) -> Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Decide notability before the journal moves `fields`; the notification
        // then fires off the lock so a slow command never serializes the fleet.
        let plan = self
            .notify
            .as_ref()
            .and_then(|_| Notifier::plan(event, &fields));
        let text = {
            let mut journal = self
                .journal
                .lock()
                .map_err(|_| anyhow!("run journal mutex poisoned"))?;
            if let Some(failure) = &journal.failed {
                bail!("run journal already failed: {failure}");
            }
            match journal.append(&self.run_id, event, ts, fields) {
                Ok(text) => {
                    if self.stream_stdout {
                        // After the flush; println! locks stdout so lines never interleave.
                        println!("{text}");
                    }
                    text
                }
                Err(error) => {
                    journal.failed = Some(format!("{error:#}"));
                    return Err(error);
                }
            }
        };
        if let (Some(notify), Some(plan)) = (&self.notify, plan) {
            // A closed channel (worker gone) is ignored: notifications are a
            // side-channel and must never fail the run.
            let _ = notify.sender.send((plan, text));
        }
        Ok(())
    }

    /// A durable order transition with its full report-shaped evidence.
    pub fn emit_report(&self, event: &str, report: &OrderReport) -> Result<()> {
        self.emit(
            event,
            serde_json::json!({
                "id": report.id,
                "outcome": report.outcome.key(),
                "detail": report.detail,
                "usage_tokens": report.usage_tokens,
                "attempts": report.attempts,
                "session_id": report.session_id,
                "branch": report.branch,
                "report": report,
            }),
        )
    }

    pub fn emit_terminal(&self, event: &str, report: &OrderReport) -> Result<()> {
        self.emit_report(event, report)
    }

    /// Whether the journal has already failed; workers check it before dispatch.
    pub fn failed(&self) -> bool {
        self.journal
            .lock()
            .map(|journal| journal.failed.is_some())
            .unwrap_or(true)
    }

    /// Fail the run if any dispatch emit failed, before report.json is published.
    pub fn check(&self) -> Result<()> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| anyhow!("run journal mutex poisoned"))?;
        match &journal.failed {
            Some(failure) => bail!("run journal failed: {failure}"),
            None => Ok(()),
        }
    }

    pub fn streaming(&self) -> bool {
        self.stream_stdout
    }
}

impl Drop for EventSink {
    /// Close the notify channel and wait for its worker so the final
    /// `run_finished` notification completes before the process exits. Each
    /// command is time-bounded, so a hung one cannot wedge this join.
    fn drop(&mut self) {
        if let Some(NotifyChannel { sender, worker }) = self.notify.take() {
            drop(sender);
            let _ = worker.join();
        }
    }
}

impl Journal {
    /// Allocate the sequence, serialize the enveloped record, write the whole
    /// line, and flush under the caller's lock; returns the line for mirroring.
    fn append(
        &mut self,
        run_id: &str,
        event: &str,
        ts: u64,
        mut fields: serde_json::Value,
    ) -> Result<String> {
        let mut line = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "run_id": run_id,
            "seq": self.seq,
            "ts": ts,
            "event": event,
        });
        if let (Some(object), Some(extra)) = (line.as_object_mut(), fields.as_object_mut()) {
            object.append(extra);
        }
        let text = serde_json::to_string(&line).context("serializing run journal record")?;
        writeln!(self.file, "{text}").context("appending run journal record")?;
        self.file.flush().context("flushing run journal record")?;
        self.seq += 1;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::Order;
    use crate::report::Outcome;
    use crate::run_journal::terminal_reports;
    use std::fs::OpenOptions;
    use std::path::PathBuf;

    fn order(id: &str) -> Order {
        Order {
            id: id.into(),
            title: "t".into(),
            brief: "b".into(),
            scope: vec!["src".into()],
            acceptance: Vec::new(),
            verify_profile: None,
            executor: None,
            reviewer: None,
            timeout_secs: None,
            max_tokens: None,
            base: None,
            branch: None,
            variants: Vec::new(),
            claim_group: None,
            variant_of: None,
            after: Vec::new(),
            source: PathBuf::from(format!("{id}.toml")),
        }
    }

    fn report(id: &str, outcome: Outcome) -> OrderReport {
        let mut report = OrderReport::new(&order(id), "fake".into());
        report.outcome = outcome;
        report
    }

    fn with_file(file: File, run_id: &str) -> EventSink {
        EventSink {
            journal: Mutex::new(Journal {
                file,
                seq: 0,
                failed: None,
            }),
            run_id: run_id.to_string(),
            stream_stdout: false,
            notify: None,
        }
    }

    #[test]
    fn creation_fails_when_the_journal_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-subdir");
        assert!(EventSink::new(&missing, "run".into(), false).is_err());
    }

    #[test]
    fn write_failure_is_recorded_and_stops_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        File::create(&path).unwrap();
        // A read-only file descriptor fails every write; the sink must surface
        // that, remember it, and refuse the run instead of losing evidence.
        let read_only = OpenOptions::new().read(true).open(&path).unwrap();
        let sink = with_file(read_only, "run");
        assert!(sink.emit("run_started", serde_json::json!({})).is_err());
        assert!(sink.failed());
        assert!(sink.check().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_notify_command_fires_for_notable_events_and_the_worker_joins_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let fired = dir.path().join("fired");
        // Each notable event appends its title; a green order and run_started
        // must add nothing.
        let notifier = crate::notify::Notifier::from_command(vec![
            "sh".into(),
            "-c".into(),
            format!(
                "printf '%s\\n' \"$SUMMONER_NOTIFY_TITLE\" >> {}",
                fired.display()
            ),
        ]);
        let sink = EventSink::new(dir.path(), "run".into(), false)
            .unwrap()
            .with_notifier(notifier);
        sink.emit("run_started", serde_json::json!({})).unwrap();
        sink.emit("review_started", serde_json::json!({"id": "auth"}))
            .unwrap();
        sink.emit(
            "order_finished",
            serde_json::json!({"id": "ok", "outcome": "verified"}),
        )
        .unwrap();
        sink.emit(
            "order_finished",
            serde_json::json!({"id": "bad", "outcome": "rejected"}),
        )
        .unwrap();
        sink.emit(
            "run_finished",
            serde_json::json!({"summary": {"verified": 1}}),
        )
        .unwrap();
        // Drop joins the worker, so every dispatched notification has completed.
        drop(sink);
        let fired = std::fs::read_to_string(&fired).unwrap();
        assert_eq!(
            fired,
            "Summoner: review started\nSummoner: order rejected\nSummoner: run finished\n"
        );
    }

    #[test]
    fn concurrent_emits_produce_whole_contiguous_lines() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::new(dir.path(), "run".into(), false).unwrap();
        let sink = &sink;
        let workers = 8;
        let per_worker = 50;
        std::thread::scope(|scope| {
            for worker in 0..workers {
                scope.spawn(move || {
                    for i in 0..per_worker {
                        sink.emit("tick", serde_json::json!({"worker": worker, "i": i}))
                            .unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        let mut seqs = Vec::new();
        for line in text.lines() {
            // Any interleaving of two writers inside a line would break parsing.
            let value: serde_json::Value = serde_json::from_str(line).expect("whole line");
            assert_eq!(value["schema_version"], SCHEMA_VERSION);
            assert_eq!(value["run_id"], "run");
            seqs.push(value["seq"].as_u64().unwrap());
        }
        seqs.sort_unstable();
        assert_eq!(seqs, (0..(workers * per_worker) as u64).collect::<Vec<_>>());
    }

    #[test]
    fn terminal_reports_projects_recorded_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = EventSink::new(dir.path(), "run".into(), false).unwrap();
        sink.emit("run_started", serde_json::json!({"run_id": "run"}))
            .unwrap();
        sink.emit_terminal("order_carried", &report("carried", Outcome::Verified))
            .unwrap();
        sink.emit("order_started", serde_json::json!({"id": "live"}))
            .unwrap();
        sink.emit_terminal("order_finished", &report("live", Outcome::Rejected))
            .unwrap();

        let reports = terminal_reports(&path, "run").unwrap();
        let projected: Vec<(&str, Outcome)> =
            reports.iter().map(|r| (r.id.as_str(), r.outcome)).collect();
        assert_eq!(
            projected,
            vec![("carried", Outcome::Verified), ("live", Outcome::Rejected)]
        );
    }

    #[test]
    fn truncated_final_record_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = EventSink::new(dir.path(), "run".into(), false).unwrap();
        sink.emit("run_started", serde_json::json!({})).unwrap();
        sink.emit_terminal("order_finished", &report("done", Outcome::Verified))
            .unwrap();
        // A crash mid-append can also split one UTF-8 code point. Complete
        // earlier records remain recoverable because the journal is byte-framed.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(
                b"{\"schema_version\":1,\"run_id\":\"run\",\"seq\":2,\"text\":\"\xf0\x9f",
            )
            .unwrap();
        }
        let reports = terminal_reports(&path, "run").unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].id, "done");
    }

    #[test]
    fn corruption_in_a_complete_line_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = EventSink::new(dir.path(), "run".into(), false).unwrap();
        sink.emit("run_started", serde_json::json!({})).unwrap();
        // A newline-terminated garbage line is corruption, not a truncated tail.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "not json").unwrap();
        }
        assert!(terminal_reports(&path, "run").is_err());
    }

    #[test]
    fn sequence_gap_and_run_id_mismatch_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let gap = dir.path().join("gap.jsonl");
        std::fs::write(
            &gap,
            "{\"schema_version\":1,\"run_id\":\"run\",\"seq\":0,\"ts\":1,\"event\":\"a\"}\n\
             {\"schema_version\":1,\"run_id\":\"run\",\"seq\":2,\"ts\":1,\"event\":\"b\"}\n",
        )
        .unwrap();
        assert!(terminal_reports(&gap, "run").is_err());

        let wrong = dir.path().join("wrong.jsonl");
        std::fs::write(
            &wrong,
            "{\"schema_version\":1,\"run_id\":\"other\",\"seq\":0,\"ts\":1,\"event\":\"a\"}\n",
        )
        .unwrap();
        assert!(terminal_reports(&wrong, "run").is_err());
    }
}
