//! `summoner overview`: one pane across every fleet and every Grove repo on the
//! machine, folded from the same event journals a single-run `watch` reads. It
//! answers "what is running anywhere?" without visiting each of a dozen repos.
//!
//! Two sources, both best-effort NDJSON: summoner run journals under the runs
//! root (`runs/*/events.jsonl`, which carry the repo path), and Grove's
//! per-repo coordination stream (`<cache-root>/events/*.jsonl`, keyed by repo
//! slug). Folding is pure so the board is testable; gathering and the redraw
//! loop are the only I/O.

use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One fleet folded to a single line: its repo and order tally.
struct Fleet {
    run_id: String,
    repo: String,
    finished: bool,
    last_ts: u64,
    green: usize,
    running: usize,
    failed: usize,
    total: usize,
}

/// One Grove repo's recent coordination activity, keyed by its events-file slug.
struct GroveRepo {
    slug: String,
    last_ts: u64,
    categories: BTreeMap<String, usize>,
}

pub fn overview(grove_bin: &str, watch: bool) -> Result<i32> {
    if !watch {
        print!("{}", board(grove_bin)?);
        return Ok(0);
    }
    loop {
        // A dumb full redraw, like `watch`: no TUI dependency.
        print!("\x1b[2J\x1b[H{}", board(grove_bin)?);
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn board(grove_bin: &str) -> Result<String> {
    let fleets = gather_fleets()?;
    let grove = gather_grove(grove_bin);
    Ok(render(&fleets, &grove, now()))
}

/// Read every run journal under the runs root and fold each to one [`Fleet`].
fn gather_fleets() -> Result<Vec<Fleet>> {
    let root = crate::run::runs_root();
    let mut fleets = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        // No runs yet is an empty pane, not an error.
        Err(_) => return Ok(fleets),
    };
    for entry in entries.filter_map(Result::ok) {
        let events = entry.path().join("events.jsonl");
        if let Ok(text) = std::fs::read_to_string(&events) {
            let run_id = entry.file_name().to_string_lossy().to_string();
            fleets.push(fold_fleet(run_id, &text));
        }
    }
    Ok(fleets)
}

/// Read Grove's per-repo event files, if the Grove cache root can be resolved.
/// Best-effort: a missing Grove or events dir just leaves the section empty.
fn gather_grove(grove_bin: &str) -> Vec<GroveRepo> {
    let Some(cache_root) = grove_cache_root(grove_bin) else {
        return Vec::new();
    };
    let mut repos = Vec::new();
    let Ok(entries) = std::fs::read_dir(cache_root.join("events")) else {
        return repos;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            let slug = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_default();
            repos.push(fold_grove(slug, &text));
        }
    }
    repos
}

fn grove_cache_root(grove_bin: &str) -> Option<PathBuf> {
    let output = Command::new(grove_bin).arg("config").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let config: Value = serde_json::from_slice(&output.stdout).ok()?;
    config["effective"]["cache_root"]
        .as_str()
        .map(PathBuf::from)
}

/// Fold a run's events into its order tally. An order is running once it starts
/// and until a terminal record (`order_finished`/`order_carried`) records its
/// outcome; green counts the outcomes that landed.
fn fold_fleet(run_id: String, events: &str) -> Fleet {
    let mut repo = String::new();
    let mut finished = false;
    let mut last_ts = 0;
    // order id -> Some(outcome) once terminal, None while running.
    let mut orders: BTreeMap<String, Option<String>> = BTreeMap::new();
    for value in lines(events) {
        let event = value["event"].as_str().unwrap_or("");
        last_ts = last_ts.max(value["ts"].as_u64().unwrap_or(0));
        match event {
            "run_started" => repo = value["repo"].as_str().unwrap_or("").to_string(),
            "run_finished" => finished = true,
            "order_finished" | "order_carried" => {
                if let Some(id) = value["id"].as_str() {
                    orders.insert(
                        id.to_string(),
                        Some(value["outcome"].as_str().unwrap_or("").to_string()),
                    );
                }
            }
            _ if event.starts_with("order_") => {
                if let Some(id) = value["id"].as_str() {
                    orders.entry(id.to_string()).or_insert(None);
                }
            }
            _ => {}
        }
    }
    let total = orders.len();
    let green = orders
        .values()
        .filter(|o| o.as_deref().is_some_and(crate::report::is_green_outcome))
        .count();
    let failed = orders
        .values()
        .filter(|o| {
            o.as_deref()
                .is_some_and(|k| !crate::report::is_green_outcome(k))
        })
        .count();
    let running = orders.values().filter(|o| o.is_none()).count();
    Fleet {
        run_id,
        repo,
        finished,
        last_ts,
        green,
        running,
        failed,
        total,
    }
}

/// Fold a Grove repo's stream into a per-category tally of its events, keyed by
/// the prefix before the dot (`task`, `verify`, `claim`, `worktree`, ...).
fn fold_grove(slug: String, events: &str) -> GroveRepo {
    let mut last_ts = 0;
    let mut categories: BTreeMap<String, usize> = BTreeMap::new();
    for value in lines(events) {
        last_ts = last_ts.max(value["ts"].as_u64().unwrap_or(0));
        if let Some(event) = value["event"].as_str() {
            let category = event.split('.').next().unwrap_or(event);
            *categories.entry(category.to_string()).or_insert(0) += 1;
        }
    }
    GroveRepo {
        slug,
        last_ts,
        categories,
    }
}

fn render(fleets: &[Fleet], grove: &[GroveRepo], now: u64) -> String {
    let mut out = String::new();
    out.push_str("Fleets (summoner runs)\n");
    if fleets.is_empty() {
        out.push_str("  none\n");
    } else {
        // Active first, then most recent.
        let mut fleets: Vec<&Fleet> = fleets.iter().collect();
        fleets.sort_by(|a, b| a.finished.cmp(&b.finished).then(b.last_ts.cmp(&a.last_ts)));
        for fleet in fleets {
            let repo = basename(&fleet.repo);
            let status = if fleet.finished {
                "finished"
            } else {
                "running"
            };
            out.push_str(&format!(
                "  {:<16} {:<16} {:<9} {:>2} orders: {} verified, {} running, {} failed  ({})\n",
                shorten(&fleet.run_id),
                repo,
                status,
                fleet.total,
                fleet.green,
                fleet.running,
                fleet.failed,
                ago(now, fleet.last_ts),
            ));
        }
    }
    out.push_str("\nGrove coordination\n");
    if grove.is_empty() {
        out.push_str("  none\n");
    } else {
        let mut grove: Vec<&GroveRepo> = grove.iter().collect();
        grove.sort_by_key(|repo| std::cmp::Reverse(repo.last_ts));
        for repo in grove {
            let tally: Vec<String> = repo
                .categories
                .iter()
                .map(|(category, count)| format!("{count} {category}"))
                .collect();
            out.push_str(&format!(
                "  {:<16} {:<10} {}\n",
                shorten(&repo.slug),
                ago(now, repo.last_ts),
                tally.join(", "),
            ));
        }
    }
    out
}

/// JSON lines, skipping blanks and anything that does not parse — the stream is
/// a best-effort signal, so one torn line never blanks the pane.
fn lines(events: &str) -> impl Iterator<Item = Value> + '_ {
    events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
}

fn basename(repo: &str) -> String {
    Path::new(repo)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "?".to_string())
}

fn shorten(id: &str) -> String {
    if id.len() > 14 {
        format!("{}…", &id[..13])
    } else {
        id.to_string()
    }
}

fn ago(now: u64, then: u64) -> String {
    if then == 0 {
        return "—".to_string();
    }
    let secs = now.saturating_sub(then);
    if secs < 90 {
        format!("{secs}s ago")
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
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
    fn fold_fleet_tallies_running_green_and_failed_orders() {
        let events = "\
{\"event\":\"run_started\",\"ts\":100,\"repo\":\"/home/t/webapp\"}
{\"event\":\"order_started\",\"ts\":101,\"id\":\"a\"}
{\"event\":\"order_started\",\"ts\":102,\"id\":\"b\"}
{\"event\":\"order_started\",\"ts\":103,\"id\":\"c\"}
{\"event\":\"order_finished\",\"ts\":110,\"id\":\"a\",\"outcome\":\"verified\"}
{\"event\":\"order_finished\",\"ts\":111,\"id\":\"b\",\"outcome\":\"rejected\"}
";
        let fleet = fold_fleet("1699-1".into(), events);
        assert_eq!(fleet.repo, "/home/t/webapp");
        assert!(!fleet.finished);
        assert_eq!(
            (fleet.total, fleet.green, fleet.failed, fleet.running),
            (3, 1, 1, 1)
        );
        assert_eq!(fleet.last_ts, 111);
    }

    #[test]
    fn fold_fleet_marks_a_finished_run() {
        let events = "\
{\"event\":\"run_started\",\"ts\":1,\"repo\":\"/r\"}
{\"event\":\"order_finished\",\"ts\":2,\"id\":\"a\",\"outcome\":\"approved\"}
{\"event\":\"run_finished\",\"ts\":3}
";
        let fleet = fold_fleet("r".into(), events);
        assert!(fleet.finished);
        assert_eq!((fleet.total, fleet.green), (1, 1));
    }

    #[test]
    fn fold_grove_tallies_event_categories_and_tolerates_torn_lines() {
        let events = "\
{\"event\":\"task.begun\",\"ts\":10}
{\"event\":\"verify.completed\",\"ts\":11,\"passed\":true}
{\"event\":\"verify.completed\",\"ts\":12,\"passed\":false}
{ this is not json
{\"event\":\"claim.granted\",\"ts\":13}
";
        let repo = fold_grove("slug".into(), events);
        assert_eq!(repo.last_ts, 13);
        assert_eq!(repo.categories.get("task"), Some(&1));
        assert_eq!(repo.categories.get("verify"), Some(&2));
        assert_eq!(repo.categories.get("claim"), Some(&1));
    }

    #[test]
    fn render_puts_active_fleets_first_and_names_repos_and_grove_activity() {
        let fleets = vec![
            Fleet {
                run_id: "old".into(),
                repo: "/home/t/api".into(),
                finished: true,
                last_ts: 100,
                green: 2,
                running: 0,
                failed: 0,
                total: 2,
            },
            Fleet {
                run_id: "live".into(),
                repo: "/home/t/webapp".into(),
                finished: false,
                last_ts: 200,
                green: 1,
                running: 1,
                failed: 0,
                total: 2,
            },
        ];
        let grove = vec![GroveRepo {
            slug: "abc123".into(),
            last_ts: 190,
            categories: BTreeMap::from([("task".to_string(), 3), ("verify".to_string(), 2)]),
        }];
        let board = render(&fleets, &grove, 200);
        // The running fleet is listed before the finished one.
        let live = board.find("webapp").unwrap();
        let done = board.find("api").unwrap();
        assert!(live < done, "active fleets sort first:\n{board}");
        assert!(board.contains("running"));
        assert!(board.contains("3 task, 2 verify"));
    }

    #[test]
    fn empty_sources_render_a_none_pane() {
        let board = render(&[], &[], 0);
        assert!(board.contains("Fleets (summoner runs)\n  none"));
        assert!(board.contains("Grove coordination\n  none"));
    }
}
