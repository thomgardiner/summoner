//! Fleet scheduling and durable report publication.

use crate::config::Config;
use crate::events::EventSink;
use crate::grove::GroveCli;
use crate::order::Order;
use crate::report::{OrderReport, RunReport};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(any(unix, windows))]
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod worker;

pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub(crate) struct Ctx<'a> {
    pub(crate) config: &'a Config,
    pub(crate) grove: GroveCli,
    pub(crate) repo: PathBuf,
    pub(crate) run_dir: PathBuf,
    pub(crate) events: EventSink,
    pub(crate) prior: &'a [OrderReport],
    /// Live spend across all workers and attempts.
    pub(crate) spent: AtomicU64,
}
pub fn run(
    config: &Config,
    selected: Option<&str>,
    paths: &[PathBuf],
    stream: bool,
    allow_unknown_auth: bool,
) -> Result<i32> {
    crate::config::selected_profile(selected);
    let grove = GroveCli::new(config.grove_bin());
    let orders = crate::run_prepare::validated(paths, config)?;
    crate::doctor::require(config, &orders, allow_unknown_auth)?;
    execute(config, grove, orders, stream, Vec::new(), Vec::new())
}
pub fn resume(
    config: &Config,
    _selected: Option<&str>,
    run_id: &str,
    stream: bool,
    allow_unknown_auth: bool,
) -> Result<i32> {
    crate::run_resume::resume(config, run_id, stream, allow_unknown_auth)
}
pub(crate) fn execute(
    config: &Config,
    grove: GroveCli,
    orders: Vec<Order>,
    stream: bool,
    carried: Vec<OrderReport>,
    prior: Vec<OrderReport>,
) -> Result<i32> {
    let grove_version = grove.version()?;
    let selected_profile = crate::config::profile();
    let repo = std::env::current_dir()
        .context("resolving current directory")?
        .canonicalize()
        .context("canonicalizing repository path")?;
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let run_id = format!("{started_at}-{}", std::process::id());
    let run_dir = runs_root().join(&run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("creating run dir {}", run_dir.display()))?;

    let config = crate::run_evidence::write_manifest(
        &run_dir,
        &run_id,
        &repo,
        selected_profile.as_deref(),
        &grove_version,
        config,
        &orders,
    )?;
    let carried_ids: BTreeSet<&str> = carried.iter().map(|report| report.id.as_str()).collect();
    let orders: Vec<Order> = orders
        .into_iter()
        .filter(|order| !carried_ids.contains(order.id.as_str()))
        .collect();

    install_interrupt_handler()?;

    let events = EventSink::new(&run_dir, run_id.clone(), stream)?;
    let workers = config.max_parallel().min(orders.len().max(1));
    events.emit(
        "run_started",
        serde_json::json!({
            "run_id": run_id,
            "run_dir": run_dir.display().to_string(),
            "repo": repo.display().to_string(),
            "workers": workers,
            "orders": orders.iter().map(|o| o.id.clone()).collect::<Vec<_>>(),
            "carried": carried.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        }),
    )?;
    let ctx = Ctx {
        config: &config,
        grove,
        repo: repo.clone(),
        run_dir: run_dir.clone(),
        events,
        prior: &prior,
        spent: AtomicU64::new(
            carried
                .iter()
                .chain(&prior)
                .filter_map(|prior| prior.usage_tokens)
                .fold(0, u64::saturating_add),
        ),
    };
    let started = Instant::now();
    let mut fleet = worker::Fleet::new(orders, &config);
    // Carried orders count as done so their dependents dispatch immediately, and
    // each is a durable terminal record before any worker dispatches.
    for prior in &carried {
        // A carried order's verified commit still anchors its dependents.
        fleet.carry(&prior.id, prior.outcome, prior.candidate_commit.clone());
        ctx.events.emit_terminal("order_carried", prior)?;
    }
    fleet.run(&ctx, workers)?;

    // A dispatch journal failure is fatal: never rank from unrecorded memory.
    ctx.events.check()?;
    let orders = crate::run_journal::terminal_reports(&run_dir.join("events.jsonl"), &run_id)
        .context("projecting order reports from the run journal")?;
    let report = RunReport::assemble(
        run_id,
        repo.display().to_string(),
        started_at,
        started.elapsed().as_secs(),
        orders,
        config.trusted_policy.as_ref().map(|policy| policy.sha256()),
    );
    // Fail closed: record run_finished before publishing report.json or the scorecard.
    ctx.events.emit(
        "run_finished",
        serde_json::json!({
            "run_id": report.run_id,
            "duration_secs": report.duration_secs,
            "summary": report.summary,
            "usage_tokens": report.usage_tokens,
            "exit_code": report.exit_code(),
            "report_path": run_dir.join("report.json").display().to_string(),
        }),
    )?;
    crate::run_evidence::write_once(&run_dir.join("report.json"), &report)
        .context("writing report.json")?;
    crate::scorecard::record(&runs_root(), &report);
    if ctx.events.streaming() {
        // Stream consumers get the complete report as the final NDJSON line;
        // the pretty print would break line-oriented parsers.
        println!(
            "{}",
            serde_json::json!({"event": "report", "report": &report})
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(report.exit_code())
}

pub(crate) fn runs_root() -> PathBuf {
    runs_root_for(
        cfg!(windows),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
        std::env::temp_dir(),
    )
}

fn runs_root_for(
    windows: bool,
    xdg: Option<OsString>,
    local_app_data: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
    temp: PathBuf,
) -> PathBuf {
    let present = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    let root = if windows {
        present(local_app_data).map(PathBuf::from).or_else(|| {
            present(user_profile)
                .or_else(|| present(home))
                .map(PathBuf::from)
                .map(|path| path.join(".cache"))
        })
    } else {
        present(xdg).map(PathBuf::from).or_else(|| {
            present(home)
                .or_else(|| present(user_profile))
                .map(PathBuf::from)
                .map(|path| path.join(".cache"))
        })
    };
    root.unwrap_or(temp).join("summoner").join("runs")
}

#[cfg(unix)]
fn install_interrupt_handler() -> Result<()> {
    extern "C" fn note_interrupt(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    unsafe {
        if libc::signal(
            libc::SIGINT,
            note_interrupt as *const () as libc::sighandler_t,
        ) == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error()).context("installing SIGINT handler");
        }
        if libc::signal(
            libc::SIGTERM,
            note_interrupt as *const () as libc::sighandler_t,
        ) == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error()).context("installing SIGTERM handler");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn install_interrupt_handler() -> Result<()> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };

    unsafe extern "system" fn note_interrupt(event: u32) -> i32 {
        if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
            SHUTDOWN.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    if unsafe { SetConsoleCtrlHandler(Some(note_interrupt), 1) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("installing Windows console interrupt handler");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn install_interrupt_handler() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Outcome, WorkerFailureKind};
    use std::collections::BTreeMap;
    use std::path::Path;

    fn order(id: &str) -> Order {
        Order {
            id: id.into(),
            title: "t".into(),
            brief: "b".into(),
            scope: vec!["src".into()],
            acceptance: Vec::new(),
            verify_profile: None,
            executor: Some("fake".into()),
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

    fn context<'a>(config: &'a Config, dir: &Path, prior: &'a [OrderReport]) -> Ctx<'a> {
        Ctx {
            config,
            grove: GroveCli::new("grove".into()),
            repo: dir.into(),
            run_dir: dir.into(),
            events: EventSink::new(dir, "run".into(), false).unwrap(),
            prior,
            spent: AtomicU64::new(0),
        }
    }

    fn reports(dir: &Path) -> BTreeMap<String, OrderReport> {
        crate::run_journal::terminal_reports(&dir.join("events.jsonl"), "run")
            .unwrap()
            .into_iter()
            .map(|report| (report.id.clone(), report))
            .collect()
    }

    #[test]
    fn windows_run_evidence_prefers_durable_user_storage() {
        let temp = PathBuf::from("temporary");
        assert_eq!(
            runs_root_for(
                true,
                Some("xdg".into()),
                Some("local".into()),
                Some("home".into()),
                Some("profile".into()),
                temp.clone(),
            ),
            PathBuf::from("local/summoner/runs")
        );
        assert_eq!(
            runs_root_for(
                true,
                None,
                None,
                Some("home".into()),
                Some("profile".into()),
                temp,
            ),
            PathBuf::from("profile/.cache/summoner/runs")
        );
    }

    #[test]
    fn unix_run_evidence_honors_xdg_then_home() {
        assert_eq!(
            runs_root_for(
                false,
                Some("xdg".into()),
                Some("local".into()),
                Some("home".into()),
                None,
                PathBuf::from("temporary"),
            ),
            PathBuf::from("xdg/summoner/runs")
        );
    }

    #[test]
    fn panicked_order_does_not_escape_or_stop_unrelated_work() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let mut dependent = order("dependent");
        dependent.after.push("panics".into());
        let fleet = worker::Fleet::new(
            vec![order("panics"), order("independent"), dependent],
            &config,
        );
        fleet
            .dispatch(&context(&config, dir.path(), &[]), 2, |_, order| {
                if order.id == "panics" {
                    panic!("deterministic worker panic");
                }
                let mut report = OrderReport::new(order, "fake".into());
                report.outcome = Outcome::Verified;
                report
            })
            .unwrap();
        let reports = reports(dir.path());
        let panicked = reports.get("panics").unwrap();
        assert_eq!(panicked.outcome, Outcome::Error);
        let failure = panicked.worker_failure.as_ref().unwrap();
        assert_eq!(failure.kind, WorkerFailureKind::Panic);
        assert_eq!(failure.message, "deterministic worker panic");
        assert_eq!(
            reports.get("independent").unwrap().outcome,
            Outcome::Verified
        );
        let dependent = reports.get("dependent").unwrap();
        assert_eq!(dependent.outcome, Outcome::Skipped);
        assert!(
            dependent
                .detail
                .as_deref()
                .is_some_and(|detail| { detail.contains("dependency \"panics\" was error") })
        );
    }

    #[test]
    fn poisoned_scheduler_becomes_structured_order_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let fleet = worker::Fleet::new(vec![order("poisoned"), order("independent")], &config);
        fleet.poison();
        fleet
            .dispatch(&context(&config, dir.path(), &[]), 2, |_, order| {
                let mut report = OrderReport::new(order, "fake".into());
                report.outcome = Outcome::Verified;
                report
            })
            .unwrap();
        let reports = reports(dir.path());
        let poisoned = reports.get("poisoned").unwrap();
        assert_eq!(poisoned.outcome, Outcome::Error);
        let failure = poisoned.worker_failure.as_ref().unwrap();
        assert_eq!(failure.kind, WorkerFailureKind::SchedulerPoisoned);
        assert_eq!(
            reports.get("independent").unwrap().outcome,
            Outcome::Verified
        );
    }
}
