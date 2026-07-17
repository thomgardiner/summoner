//! `summoner watch`: a live terminal board over a run's events.jsonl. Reads
//! the same NDJSON any IDE would, so this is both a dashboard and the
//! reference consumer of the event stream. No TUI dependency: redraw the
//! screen from folded state every poll.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Default, Clone)]
struct OrderRow {
    executor: String,
    phase: String,
    attempt: u64,
    started: u64,
    ended: Option<u64>,
    usage: Option<u64>,
    branch: String,
    detail: String,
}

pub fn watch(run_id: Option<String>) -> Result<i32> {
    let root = crate::run::runs_root();
    let dir = match run_id {
        Some(id) => root.join(id),
        // Run ids sort chronologically (epoch-seconds prefix).
        None => latest_run(&root)?,
    };
    let events = dir.join("events.jsonl");
    if !events.exists() {
        bail!("no events at {}", events.display());
    }
    println!("watching {}", events.display());
    loop {
        let text = std::fs::read_to_string(&events)
            .with_context(|| format!("reading {}", events.display()))?;
        let (board, finished) = render(&text, now());
        // Clear and home: a dumb full redraw beats a TUI dependency.
        print!("\x1b[2J\x1b[H{board}");
        if finished {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn latest_run(root: &std::path::Path) -> Result<PathBuf> {
    let mut runs: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("no runs under {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join("events.jsonl").exists())
        .collect();
    runs.sort();
    runs.pop()
        .with_context(|| format!("no runs with events under {}", root.display()))
}

/// Fold the event stream into a board. Pure so the board is testable; returns
/// (rendered text, run finished).
pub fn render(events: &str, now: u64) -> (String, bool) {
    let mut rows: BTreeMap<String, OrderRow> = BTreeMap::new();
    let mut header = String::from("run: ?");
    let mut footer = String::new();
    let mut finished = false;
    for line in events.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = event["ts"].as_u64().unwrap_or(0);
        let id = event["id"].as_str().unwrap_or_default().to_string();
        match event["event"].as_str().unwrap_or_default() {
            "run_started" => {
                header = format!(
                    "run: {}  repo: {}  workers: {}",
                    event["run_id"].as_str().unwrap_or("?"),
                    event["repo"].as_str().unwrap_or("?"),
                    event["workers"]
                );
                for carried in event["carried"].as_array().into_iter().flatten() {
                    if let Some(id) = carried.as_str() {
                        let row = rows.entry(id.to_string()).or_default();
                        row.phase = "carried".into();
                        row.started = ts;
                        row.ended = Some(ts);
                    }
                }
            }
            "run_finished" => {
                finished = true;
                footer = format!(
                    "finished in {}s  exit {}  tokens {}",
                    event["duration_secs"], event["exit_code"], event["usage_tokens"]
                );
            }
            name => {
                let row = rows.entry(id).or_default();
                match name {
                    "order_started" => {
                        row.executor = event["executor"].as_str().unwrap_or("?").into();
                        row.phase = "starting".into();
                        row.attempt = 1;
                        row.started = ts;
                    }
                    "order_dispatched" => {
                        row.phase = "executing".into();
                        row.branch = event["branch"].as_str().unwrap_or_default().into();
                    }
                    "order_exec_done" => {
                        row.phase = "verifying".into();
                        row.usage = event["usage_tokens"].as_u64().or(row.usage);
                    }
                    "order_verify" => row.phase = "verified".into(),
                    "review_started" => {
                        row.phase =
                            format!("review ({})", event["reviewer"].as_str().unwrap_or("?"));
                    }
                    "order_review" => {
                        row.phase =
                            format!("reviewed: {}", event["verdict"].as_str().unwrap_or("?"));
                    }
                    "order_revised" => {
                        row.attempt = event["attempt"].as_u64().unwrap_or(row.attempt + 1);
                        row.phase =
                            format!("revising ({})", event["reason"].as_str().unwrap_or("retry"));
                    }
                    "order_finished" => {
                        row.phase = event["outcome"].as_str().unwrap_or("?").into();
                        row.ended = Some(ts);
                        row.usage = event["usage_tokens"].as_u64().or(row.usage);
                        row.attempt = event["attempts"].as_u64().unwrap_or(row.attempt.max(1));
                        row.detail = event["detail"].as_str().unwrap_or_default().into();
                        // The attach handles: the branch holds the work, the
                        // session id resumes the executor's context.
                        if row.detail.is_empty()
                            && let Some(session) = event["session_id"].as_str()
                        {
                            row.detail = format!("session {session}");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // A stray empty-id row from unknown events must not render.
    rows.remove("");

    let mut out = format!("{header}\n\n");
    out.push_str(&format!(
        "{:<20} {:<10} {:<3} {:<22} {:<22} {:>8} {:>7}  {}\n",
        "ORDER", "EXECUTOR", "TRY", "PHASE", "BRANCH", "ELAPSED", "TOKENS", "DETAIL"
    ));
    let mut total_tokens = 0u64;
    for (id, row) in &rows {
        // Skipped orders never start; an epoch-sized elapsed is a lie.
        let elapsed = if row.started == 0 {
            "-".to_string()
        } else {
            format!("{}s", row.ended.unwrap_or(now).saturating_sub(row.started))
        };
        total_tokens = total_tokens.saturating_add(row.usage.unwrap_or(0));
        out.push_str(&format!(
            "{:<20} {:<10} {:<3} {:<22} {:<22} {:>8} {:>7}  {}\n",
            truncate(id, 20),
            truncate(&row.executor, 10),
            row.attempt,
            truncate(&row.phase, 22),
            truncate(&row.branch, 22),
            elapsed,
            row.usage.map_or("-".into(), |n| n.to_string()),
            truncate(&row.detail, 48),
        ));
    }
    if footer.is_empty() {
        footer = format!("running…  tokens so far: {total_tokens}");
    }
    out.push_str(&format!("\n{footer}\n"));
    (out, finished)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_fold_into_a_board_with_phases_attempts_and_totals() {
        let events = r#"
{"ts":100,"event":"run_started","run_id":"r-1","repo":"/repo","workers":2}
{"ts":101,"event":"order_started","id":"auth","executor":"codex"}
{"ts":102,"event":"order_dispatched","id":"auth","branch":"grove/smn-auth"}
{"ts":110,"event":"order_exec_done","id":"auth","usage_tokens":500}
{"ts":111,"event":"order_verify","id":"auth","passed":true}
{"ts":112,"event":"review_started","id":"auth","reviewer":"judge"}
{"ts":120,"event":"order_review","id":"auth","verdict":"reject"}
{"ts":121,"event":"order_revised","id":"auth","attempt":2,"reason":"rejected"}
{"ts":150,"event":"order_finished","id":"auth","outcome":"approved","usage_tokens":900,"attempts":2,"session_id":"sess-42"}
{"ts":151,"event":"run_finished","run_id":"r-1","duration_secs":51,"exit_code":0,"usage_tokens":900}
"#;
        let (board, finished) = render(events, 200);
        assert!(finished);
        assert!(board.contains("run: r-1"), "{board}");
        assert!(board.contains("auth"), "{board}");
        assert!(board.contains("approved"), "{board}");
        assert!(board.contains("900"), "{board}");
        // Attempt column reflects the revision; attach handles render.
        let row = board.lines().find(|l| l.starts_with("auth")).unwrap();
        assert!(row.contains(" 2 "), "{row}");
        assert!(row.contains("grove/smn-auth"), "{row}");
        assert!(row.contains("session sess-42"), "{row}");
        // Elapsed uses the finish timestamp, not `now`.
        assert!(row.contains("49s"), "{row}");
        assert!(board.contains("exit 0"), "{board}");

        // An unfinished stream keeps running and measures against `now`.
        let live: String = events
            .lines()
            .filter(|l| !l.contains("run_finished") && !l.contains("order_finished"))
            .collect::<Vec<_>>()
            .join("\n");
        let (board, finished) = render(&live, 161);
        assert!(!finished);
        assert!(board.contains("running…"), "{board}");
        let row = board.lines().find(|l| l.starts_with("auth")).unwrap();
        assert!(row.contains("60s"), "{row}");
    }
}
