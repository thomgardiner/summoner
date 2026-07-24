//! Executor backend shape checks.

use crate::config::{ExecutorBackend, PromptRouting};

pub(crate) fn backend_problems(name: &str, backend: &ExecutorBackend) -> Vec<String> {
    let has = |token: &str| backend.argv.iter().any(|arg| arg.contains(token));
    let mut problems = Vec::new();
    if backend.argv.is_empty() {
        problems.push(format!("executor {name:?}: argv is empty"));
        return problems;
    }
    match backend.routing() {
        PromptRouting::Arg if !has("{prompt}") => problems.push(format!(
            "executor {name:?}: prompt routing \"arg\" needs a {{prompt}} placeholder in argv"
        )),
        PromptRouting::File if !has("{prompt_file}") => problems.push(format!(
            "executor {name:?}: prompt routing \"file\" needs a {{prompt_file}} placeholder in argv"
        )),
        _ => {}
    }
    if backend.routing() != PromptRouting::Arg && has("{prompt}") {
        problems.push(format!(
            "executor {name:?}: argv references {{prompt}} but routing is not \"arg\""
        ));
    }
    if backend.routing() != PromptRouting::File && has("{prompt_file}") {
        problems.push(format!(
            "executor {name:?}: argv references {{prompt_file}} but routing is not \"file\""
        ));
    }
    if let Some(timeout) = backend.timeout_secs
        && !(1..=604_800).contains(&timeout)
    {
        problems.push(format!(
            "executor {name:?}: timeout_secs must be between 1 and 604800 (7 days), got {timeout}"
        ));
    }
    // A resume template quietly suppresses the full charter, so it must
    // provably resume the right session and deliver the revision evidence.
    if !backend.resume_argv.is_empty() {
        let has_resume = |token: &str| backend.resume_argv.iter().any(|arg| arg.contains(token));
        if backend.session_marker.is_none() {
            problems.push(format!(
                "executor {name:?}: resume_argv needs a session_marker to capture the \
                 session it resumes"
            ));
        }
        if !has_resume("{session_id}") {
            problems.push(format!(
                "executor {name:?}: resume_argv needs a {{session_id}} placeholder"
            ));
        }
        match backend.routing() {
            PromptRouting::Arg if !has_resume("{prompt}") => problems.push(format!(
                "executor {name:?}: resume_argv needs a {{prompt}} placeholder \
                 (routing \"arg\")"
            )),
            PromptRouting::File if !has_resume("{prompt_file}") => problems.push(format!(
                "executor {name:?}: resume_argv needs a {{prompt_file}} placeholder \
                 (routing \"file\")"
            )),
            _ => {}
        }
    }
    problems
}
