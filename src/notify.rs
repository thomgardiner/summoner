//! Terminal-event notifications: fire a user command when the fleet reaches a
//! moment worth looking up from other work — the run finishes, an order lands
//! non-green, or a review starts. It is a thin side-channel over the run
//! journal, never authoritative: a notification that fails or is slow changes
//! nothing about the recorded run.
//!
//! The mechanism is one configured command, run per notable event with the
//! event's JSON line on stdin and `SUMMONER_NOTIFY_TITLE`/`_BODY`/`_EVENT` in
//! the environment. That one seam covers both an OS notifier (`notify-send`,
//! `osascript`, `terminal-notifier`) and a webhook (`curl` reading stdin),
//! without linking an HTTP client. Commands run on a single background worker
//! so they never block the fleet, and each is bounded so a hung command cannot
//! wedge shutdown; [`crate::events::EventSink`] joins the worker on drop, so the
//! last `run_finished` notification always completes before the process exits.

use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A notify command with the moments that trigger it fixed to the ones you
/// actually stop work for: a finished run, a non-green order, a started review.
pub struct Notifier {
    command: Vec<String>,
}

/// What a single notable event should tell you. Sent to the worker by value.
pub struct NotifyPlan {
    event: String,
    title: String,
    body: String,
}

/// How long a single notify command may run before it is killed, so a command
/// that forgets to bound itself (a `curl` with no `--max-time` to a dead host)
/// cannot hang the run's shutdown behind the worker join.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

impl Notifier {
    /// A notifier only when a command is configured; an empty command means the
    /// feature is off and no worker thread is spawned.
    pub fn from_command(command: Vec<String>) -> Option<Self> {
        (!command.is_empty()).then_some(Self { command })
    }

    /// The notification a raw journal event warrants, or `None` when the event
    /// is not one you stop work for. Pure over `(event, fields)` so the policy
    /// is unit-testable without spawning anything. Green orders are silent on
    /// purpose: a fleet that is going fine should not interrupt you.
    pub fn plan(event: &str, fields: &Value) -> Option<NotifyPlan> {
        let id = || fields["id"].as_str().unwrap_or("?");
        let (title, body) = match event {
            "run_finished" => ("Summoner: run finished".to_string(), run_summary(fields)),
            "review_started" => (
                "Summoner: review started".to_string(),
                format!("order {}", id()),
            ),
            "order_finished" => {
                let outcome = fields["outcome"].as_str().unwrap_or("");
                if crate::report::is_green_outcome(outcome) {
                    return None;
                }
                (
                    format!("Summoner: order {outcome}"),
                    format!("order {}", id()),
                )
            }
            _ => return None,
        };
        Some(NotifyPlan {
            event: event.to_string(),
            title,
            body,
        })
    }

    /// Run the command for one event: its JSON line on stdin, the summary in the
    /// environment, output discarded. Best-effort and bounded — every failure is
    /// swallowed because the journal, not this, is the record of the run.
    pub fn dispatch(&self, plan: &NotifyPlan, line: &str) {
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .env("SUMMONER_NOTIFY_TITLE", &plan.title)
            .env("SUMMONER_NOTIFY_BODY", &plan.body)
            .env("SUMMONER_NOTIFY_EVENT", &plan.event)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(line.as_bytes());
            let _ = stdin.write_all(b"\n");
            // Dropping stdin here closes it, so a command reading to EOF returns.
        }
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
    }
}

/// A one-line run summary from the `run_finished` summary map, e.g.
/// "verified 2, rejected 1", ordered as the map is (worst-first is the report's
/// job, not the notification's).
fn run_summary(fields: &Value) -> String {
    let Some(summary) = fields.get("summary").and_then(Value::as_object) else {
        return "run finished".to_string();
    };
    let parts: Vec<String> = summary
        .iter()
        .map(|(outcome, count)| format!("{count} {outcome}"))
        .collect();
    if parts.is_empty() {
        "no orders".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_finished_summarizes_the_outcome_counts() {
        let plan = Notifier::plan(
            "run_finished",
            &json!({"summary": {"verified": 2, "rejected": 1}}),
        )
        .expect("run_finished always notifies");
        assert_eq!(plan.title, "Summoner: run finished");
        // The summary map is sorted (BTreeMap), so counts read alphabetically.
        assert_eq!(plan.body, "1 rejected, 2 verified");
    }

    #[test]
    fn review_started_names_the_order() {
        let plan = Notifier::plan("review_started", &json!({"id": "auth"})).unwrap();
        assert_eq!(plan.title, "Summoner: review started");
        assert_eq!(plan.body, "order auth");
    }

    #[test]
    fn a_non_green_order_notifies_with_its_outcome() {
        let plan = Notifier::plan(
            "order_finished",
            &json!({"id": "api", "outcome": "rejected"}),
        )
        .expect("a non-green order is worth interrupting for");
        assert_eq!(plan.title, "Summoner: order rejected");
        assert_eq!(plan.body, "order api");
    }

    #[test]
    fn green_orders_and_ordinary_events_stay_silent() {
        for outcome in ["verified", "approved"] {
            assert!(
                Notifier::plan("order_finished", &json!({"id": "x", "outcome": outcome})).is_none(),
                "{outcome} should not notify"
            );
        }
        assert!(Notifier::plan("order_started", &json!({"id": "x"})).is_none());
        assert!(Notifier::plan("order_dispatched", &json!({})).is_none());
    }

    #[test]
    fn from_command_is_none_when_unconfigured() {
        assert!(Notifier::from_command(Vec::new()).is_none());
        assert!(Notifier::from_command(vec!["notify-send".into()]).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn dispatch_runs_the_command_with_the_line_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("notified");
        // Record the env summary and the stdin line the command received.
        let notifier = Notifier::from_command(vec![
            "sh".into(),
            "-c".into(),
            format!(
                "printf '%s\\n' \"$SUMMONER_NOTIFY_EVENT:$SUMMONER_NOTIFY_TITLE:$SUMMONER_NOTIFY_BODY\" > {0}; cat >> {0}",
                out.display()
            ),
        ])
        .unwrap();
        let plan = Notifier::plan("run_finished", &json!({"summary": {"verified": 1}})).unwrap();
        notifier.dispatch(&plan, "{\"event\":\"run_finished\"}");
        let recorded = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            recorded,
            "run_finished:Summoner: run finished:1 verified\n{\"event\":\"run_finished\"}\n"
        );
    }
}
