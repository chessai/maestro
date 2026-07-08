//! Deterministic backend for M1 end-to-end testing without an API key.

use serde::Deserialize;

use crate::{
    write_within_worktree, ImplementerBackend, ImplementerError, ImplementerOutcome,
    ImplementerTask,
};

/// The JSON shape the mock expects in `task.spec.instructions`.
#[derive(Debug, Deserialize)]
struct MockPlan {
    writes: Vec<MockWrite>,
}

#[derive(Debug, Deserialize)]
struct MockWrite {
    path: String,
    content: String,
}

/// A backend that replays a fixed set of writes encoded as JSON in the spec's
/// `instructions`. Selected when `task.model == "mock"`.
///
/// The `instructions` must be JSON of the form:
/// ```json
/// { "writes": [ { "path": "src/foo.rs", "content": "..." }, ... ] }
/// ```
/// Anything else is an [`ImplementerError::Protocol`]. Writes go through
/// [`write_within_worktree`], so path-escape rejection is shared with the real
/// backend, but the allowlist is *not* consulted (ADR-002).
#[derive(Debug, Default, Clone, Copy)]
pub struct MockBackend;

impl ImplementerBackend for MockBackend {
    fn run(&self, task: &ImplementerTask) -> Result<ImplementerOutcome, ImplementerError> {
        let plan: MockPlan = serde_json::from_str(&task.spec.instructions).map_err(|e| {
            ImplementerError::Protocol(format!(
                "mock instructions are not a valid writes-plan: {e}"
            ))
        })?;

        let mut files_written = Vec::with_capacity(plan.writes.len());
        for w in &plan.writes {
            write_within_worktree(&task.worktree, &w.path, &w.content)?;
            files_written.push(w.path.clone());
        }

        let notes = format!("mock wrote {} file(s)", files_written.len());
        // M6: report a small DETERMINISTIC nonzero token count so budget
        // enforcement (ADR-003 lifetime ceilings) can be exercised end-to-end.
        // `tokens_in` is the sum of the bytes written across all files (so a
        // writes-plan of known size yields a known token count); `tokens_out`
        // is a fixed 20. A zero-write plan still bills a floor of 100 in so a
        // ladder of attempts accrues a predictable, nonzero total.
        let written_bytes: u64 = plan.writes.iter().map(|w| w.content.len() as u64).sum();
        let tokens_in = 100 + written_bytes;
        let tokens_out = 20;
        Ok(ImplementerOutcome {
            files_written,
            turns: 1,
            tokens_in,
            tokens_out,
            notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::domain::Tier;
    use maestro_journal::spec::{Budget, TaskSpec};
    use tempfile::TempDir;

    fn spec_with_instructions(instructions: &str) -> TaskSpec {
        TaskSpec {
            title: "t".into(),
            tier: Tier::T0,
            base_ref: "main".into(),
            file_allowlist: vec![],
            instructions: instructions.into(),
            acceptance_criteria: vec![],
            check_commands: vec![],
            house_rules_ref: None,
            budget: Budget::default(),
            lifetime_budget: Default::default(),
            containment_min: 0,
        }
    }

    fn task(worktree: &std::path::Path, instructions: &str) -> ImplementerTask {
        ImplementerTask {
            spec: spec_with_instructions(instructions),
            worktree: worktree.to_path_buf(),
            house_rules: String::new(),
            model: "mock".into(),
        }
    }

    #[test]
    fn writes_the_planned_file() {
        let dir = TempDir::new().unwrap();
        let t = task(
            dir.path(),
            r#"{"writes":[{"path":"src/lib.rs","content":"pub fn x(){}"}]}"#,
        );
        let outcome = MockBackend.run(&t).unwrap();
        assert_eq!(outcome.files_written, vec!["src/lib.rs".to_string()]);
        assert_eq!(outcome.turns, 1);
        let got = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
        assert_eq!(got, "pub fn x(){}");
    }

    #[test]
    fn escaping_path_is_rejected_and_nothing_escapes() {
        let dir = TempDir::new().unwrap();
        let t = task(
            dir.path(),
            r#"{"writes":[{"path":"../escape.txt","content":"boom"}]}"#,
        );
        let err = MockBackend.run(&t).unwrap_err();
        assert!(matches!(err, ImplementerError::Io(_)), "got {err:?}");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn out_of_allowlist_in_tree_path_is_written() {
        // Allowlist enforcement is the gate's job, not the backend's.
        let dir = TempDir::new().unwrap();
        let t = task(
            dir.path(),
            r#"{"writes":[{"path":"secrets.rs","content":"const K: u8 = 1;"}]}"#,
        );
        let outcome = MockBackend.run(&t).unwrap();
        assert_eq!(outcome.files_written, vec!["secrets.rs".to_string()]);
        assert!(dir.path().join("secrets.rs").exists());
    }

    #[test]
    fn invalid_json_is_a_protocol_error() {
        let dir = TempDir::new().unwrap();
        let t = task(dir.path(), "not json at all");
        let err = MockBackend.run(&t).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn wrong_shape_json_is_a_protocol_error() {
        let dir = TempDir::new().unwrap();
        let t = task(dir.path(), r#"{"edits":[]}"#);
        let err = MockBackend.run(&t).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }
}
