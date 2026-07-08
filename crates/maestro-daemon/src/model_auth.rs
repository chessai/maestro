//! Per-role model authentication check for `maestro doctor` (ADR-004).
//!
//! This module provides a **pure, offline** classifier: it checks credential
//! presence and backend resolvability (+ command-on-PATH for driven CLIs) but
//! never makes a network call. The injected closures `env_has` and `cmd_on_path`
//! make the logic unit-testable without touching the real environment.

use maestro_journal::config::{RoleModel, Roles};
use serde_json::{json, Value};

// ── core classifier ──────────────────────────────────────────────────────────

/// Classify a single role slot.
///
/// Returns `(backend_label, status_string)`. Status strings are stable
/// identifiers:
/// - `"ok"` — everything is in order.
/// - `"missing_credential:<VAR>"` — the named env var is absent/empty.
/// - `"command_not_found:<program>"` — the CLI program is not on PATH.
/// - `"unconfigured:base_url"` — openai_compat with no base_url.
/// - `"unknown_backend"` — unrecognised `kind` value.
pub fn role_auth_status(
    kind: Option<&str>,
    model: &str,
    base_url: Option<&str>,
    command: Option<&str>,
    env_has: &dyn Fn(&str) -> bool,
    cmd_on_path: &dyn Fn(&str) -> bool,
) -> (String, String) {
    // mock short-circuit: any mock role is always "ok".
    if model == "mock" || kind == Some("mock") {
        return ("mock".to_string(), "ok".to_string());
    }

    match kind {
        Some("driven_cli") => {
            // Subscription CLIs need NO API key (ADR-006). Check that the
            // CLI program is on PATH. The effective program is `command` if
            // set, else fall back to `model` (some configs put the CLI name
            // in `model`).
            let program = command.unwrap_or(model);
            if cmd_on_path(program) {
                ("driven_cli".to_string(), "ok".to_string())
            } else {
                (
                    "driven_cli".to_string(),
                    format!("command_not_found:{program}"),
                )
            }
        }

        None | Some("anthropic") => {
            // A real Anthropic model. Requires ANTHROPIC_API_KEY regardless
            // of any base_url override.
            if env_has("ANTHROPIC_API_KEY") {
                ("anthropic".to_string(), "ok".to_string())
            } else {
                (
                    "anthropic".to_string(),
                    "missing_credential:ANTHROPIC_API_KEY".to_string(),
                )
            }
        }

        Some("openai_compat") => {
            // Requires BOTH a base_url AND OPENAI_API_KEY.
            let has_key = env_has("OPENAI_API_KEY");
            let has_url = base_url.is_some();
            if has_key && has_url {
                ("openai_compat".to_string(), "ok".to_string())
            } else if !has_key {
                (
                    "openai_compat".to_string(),
                    "missing_credential:OPENAI_API_KEY".to_string(),
                )
            } else {
                // has_key but no base_url
                ("openai_compat".to_string(), "unconfigured:base_url".to_string())
            }
        }

        Some(other) => (other.to_string(), "unknown_backend".to_string()),
    }
}

// ── helpers for extracting fields from RoleModel ──────────────────────────────

fn extract_fields(rm: &RoleModel) -> (&str, Option<&str>, Option<&str>, Option<&str>) {
    match rm {
        RoleModel::Bare(m) => (m.as_str(), None, None, None),
        RoleModel::Detailed(t) => (
            t.model.as_str(),
            t.kind.as_deref(),
            t.base_url.as_deref(),
            t.command.as_deref(),
        ),
    }
}

// ── JSON builder ──────────────────────────────────────────────────────────────

/// Build the `model_auth` JSON object for a doctor report.
///
/// Iterates all five role slots: tier0, tier1, tier2, verifier_floor, shim.
/// Configured slots produce an entry; unconfigured slots are **omitted**, except
/// `shim` which always has an effective default (`"claude-haiku-4-5"` via
/// [`Roles::shim_model`]).
pub fn build_model_auth(
    roles: &Roles,
    env_has: &dyn Fn(&str) -> bool,
    cmd_on_path: &dyn Fn(&str) -> bool,
) -> Value {
    let mut obj = serde_json::Map::new();

    // Helper to insert one entry.
    let mut insert = |slot: &str, rm: &RoleModel| {
        let (model, kind, base_url, command) = extract_fields(rm);
        let (backend, status) = role_auth_status(kind, model, base_url, command, env_has, cmd_on_path);
        obj.insert(
            slot.to_string(),
            json!({ "model": model, "backend": backend, "status": status }),
        );
    };

    if let Some(rm) = &roles.tier0 {
        insert("tier0", rm);
    }
    if let Some(rm) = &roles.tier1 {
        insert("tier1", rm);
    }
    if let Some(rm) = &roles.tier2 {
        insert("tier2", rm);
    }
    if let Some(rm) = &roles.verifier_floor {
        insert("verifier_floor", rm);
    }

    // shim always appears because fetch_extract/search always has an effective
    // model (Roles::shim_model() falls back to "claude-haiku-4-5").
    {
        let shim_model = roles.shim_model();
        let (backend, status) = if let Some(rm) = &roles.shim {
            let (model, kind, base_url, command) = extract_fields(rm);
            role_auth_status(kind, model, base_url, command, env_has, cmd_on_path)
        } else {
            // Default shim: "claude-haiku-4-5" — an Anthropic model.
            role_auth_status(None, shim_model, None, None, env_has, cmd_on_path)
        };
        obj.insert(
            "shim".to_string(),
            json!({ "model": shim_model, "backend": backend, "status": status }),
        );
    }

    Value::Object(obj)
}

// ── real-environment closures (used by doctor()) ──────────────────────────────

/// Check whether env var `key` is set and non-empty in the process environment.
pub fn real_env_has(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Check whether `program` exists as an executable on `$PATH`.
pub fn real_cmd_on_path(program: &str) -> bool {
    let path_val = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_val).any(|dir| {
        let candidate = dir.join(program);
        is_executable_file(&candidate)
    })
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::config::{RoleModelTable, Roles};

    // ── role_auth_status unit tests ──────────────────────────────────────────

    fn no_env(_k: &str) -> bool { false }
    fn has_env(_k: &str) -> bool { true }
    fn no_cmd(_p: &str) -> bool { false }
    fn has_cmd(_p: &str) -> bool { true }

    #[test]
    fn mock_model_is_always_ok() {
        let (backend, status) = role_auth_status(None, "mock", None, None, &no_env, &no_cmd);
        assert_eq!(backend, "mock");
        assert_eq!(status, "ok");
    }

    #[test]
    fn mock_kind_is_always_ok() {
        let (backend, status) = role_auth_status(Some("mock"), "claude-sonnet-4-6", None, None, &no_env, &no_cmd);
        assert_eq!(backend, "mock");
        assert_eq!(status, "ok");
    }

    #[test]
    fn driven_cli_cmd_present_ok() {
        let (backend, status) = role_auth_status(
            Some("driven_cli"), "codex", None, Some("codex"), &no_env, &has_cmd,
        );
        assert_eq!(backend, "driven_cli");
        assert_eq!(status, "ok");
    }

    #[test]
    fn driven_cli_cmd_absent_error() {
        let (backend, status) = role_auth_status(
            Some("driven_cli"), "codex", None, Some("codex"), &no_env, &no_cmd,
        );
        assert_eq!(backend, "driven_cli");
        assert_eq!(status, "command_not_found:codex");
    }

    #[test]
    fn driven_cli_falls_back_to_model_as_program() {
        // No command set → falls back to model name as the program.
        let (backend, status) = role_auth_status(
            Some("driven_cli"), "claude", None, None, &no_env, &has_cmd,
        );
        assert_eq!(backend, "driven_cli");
        assert_eq!(status, "ok");
    }

    #[test]
    fn driven_cli_model_as_program_absent() {
        let (backend, status) = role_auth_status(
            Some("driven_cli"), "claude", None, None, &no_env, &no_cmd,
        );
        assert_eq!(backend, "driven_cli");
        assert_eq!(status, "command_not_found:claude");
    }

    #[test]
    fn anthropic_kind_none_key_present_ok() {
        let (backend, status) = role_auth_status(
            None, "claude-sonnet-4-6", None, None, &has_env, &no_cmd,
        );
        assert_eq!(backend, "anthropic");
        assert_eq!(status, "ok");
    }

    #[test]
    fn anthropic_kind_explicit_key_present_ok() {
        let (backend, status) = role_auth_status(
            Some("anthropic"), "claude-opus-4-8", None, None, &has_env, &no_cmd,
        );
        assert_eq!(backend, "anthropic");
        assert_eq!(status, "ok");
    }

    #[test]
    fn anthropic_key_missing() {
        let (backend, status) = role_auth_status(
            None, "claude-sonnet-4-6", None, None, &no_env, &no_cmd,
        );
        assert_eq!(backend, "anthropic");
        assert_eq!(status, "missing_credential:ANTHROPIC_API_KEY");
    }

    #[test]
    fn anthropic_with_base_url_still_needs_key() {
        // A base_url override does NOT remove the key requirement.
        let (backend, status) = role_auth_status(
            None, "claude-sonnet-4-6", Some("https://custom.api"), None, &no_env, &no_cmd,
        );
        assert_eq!(backend, "anthropic");
        assert_eq!(status, "missing_credential:ANTHROPIC_API_KEY");
    }

    #[test]
    fn openai_compat_key_and_base_url_ok() {
        let (backend, status) = role_auth_status(
            Some("openai_compat"), "qwen", Some("http://localhost:11434/v1"), None,
            &has_env, &no_cmd,
        );
        assert_eq!(backend, "openai_compat");
        assert_eq!(status, "ok");
    }

    #[test]
    fn openai_compat_missing_key() {
        let (backend, status) = role_auth_status(
            Some("openai_compat"), "qwen", Some("http://localhost:11434/v1"), None,
            &no_env, &no_cmd,
        );
        assert_eq!(backend, "openai_compat");
        assert_eq!(status, "missing_credential:OPENAI_API_KEY");
    }

    #[test]
    fn openai_compat_missing_base_url() {
        // Key present but no base_url → unconfigured.
        let (backend, status) = role_auth_status(
            Some("openai_compat"), "qwen", None, None,
            &has_env, &no_cmd,
        );
        assert_eq!(backend, "openai_compat");
        assert_eq!(status, "unconfigured:base_url");
    }

    #[test]
    fn openai_compat_missing_both_reports_key_first() {
        // Missing key takes priority over missing base_url in the status string.
        let (backend, status) = role_auth_status(
            Some("openai_compat"), "qwen", None, None,
            &no_env, &no_cmd,
        );
        assert_eq!(backend, "openai_compat");
        assert_eq!(status, "missing_credential:OPENAI_API_KEY");
    }

    #[test]
    fn unknown_kind() {
        let (backend, status) = role_auth_status(
            Some("future_backend"), "some-model", None, None, &no_env, &no_cmd,
        );
        assert_eq!(backend, "future_backend");
        assert_eq!(status, "unknown_backend");
    }

    // ── build_model_auth JSON builder tests ─────────────────────────────────

    /// Build a RoleModel::Detailed table for test fixtures.
    fn detailed(
        model: &str,
        kind: Option<&str>,
        base_url: Option<&str>,
        command: Option<&str>,
    ) -> RoleModel {
        RoleModel::Detailed(RoleModelTable {
            model: model.to_string(),
            kind: kind.map(str::to_string),
            base_url: base_url.map(str::to_string),
            command: command.map(str::to_string),
            args: None,
            adapter: None,
            turn_budget: None,
            max_budget_usd: None,
        })
    }

    #[test]
    fn build_model_auth_tier0_anthropic_tier1_driven_cli_shim_default() {
        // tier0 = anthropic (key present), tier1 = driven_cli (cmd present),
        // shim = absent (→ haiku default, anthropic, key present).
        let roles = Roles {
            tier0: Some(RoleModel::Bare("claude-sonnet-4-6".to_string())),
            tier1: Some(detailed("codex", Some("driven_cli"), None, Some("codex"))),
            tier2: None,
            verifier_floor: None,
            shim: None,
        };

        let env_has = |k: &str| k == "ANTHROPIC_API_KEY";
        let cmd_on_path = |p: &str| p == "codex";

        let auth = build_model_auth(&roles, &env_has, &cmd_on_path);

        // tier0
        assert_eq!(auth["tier0"]["model"], "claude-sonnet-4-6");
        assert_eq!(auth["tier0"]["backend"], "anthropic");
        assert_eq!(auth["tier0"]["status"], "ok");

        // tier1
        assert_eq!(auth["tier1"]["model"], "codex");
        assert_eq!(auth["tier1"]["backend"], "driven_cli");
        assert_eq!(auth["tier1"]["status"], "ok");

        // tier2 and verifier_floor are omitted when None.
        assert!(auth.get("tier2").is_none());
        assert!(auth.get("verifier_floor").is_none());

        // shim always present with the haiku default.
        assert_eq!(auth["shim"]["model"], "claude-haiku-4-5");
        assert_eq!(auth["shim"]["backend"], "anthropic");
        assert_eq!(auth["shim"]["status"], "ok");
    }

    #[test]
    fn build_model_auth_shim_present_explicit() {
        // Explicit shim model overrides the default.
        let roles = Roles {
            tier0: None,
            tier1: None,
            tier2: None,
            verifier_floor: None,
            shim: Some(RoleModel::Bare("claude-haiku-4-5-fast".to_string())),
        };

        let auth = build_model_auth(&roles, &has_env, &no_cmd);

        assert_eq!(auth["shim"]["model"], "claude-haiku-4-5-fast");
        assert_eq!(auth["shim"]["backend"], "anthropic");
        assert_eq!(auth["shim"]["status"], "ok");
    }

    #[test]
    fn build_model_auth_missing_key_surfaces_in_all_anthropic_slots() {
        let roles = Roles {
            tier0: Some(RoleModel::Bare("claude-sonnet-4-6".to_string())),
            tier1: None,
            tier2: Some(RoleModel::Bare("claude-opus-4-8".to_string())),
            verifier_floor: Some(RoleModel::Bare("claude-sonnet-4-6".to_string())),
            shim: None,
        };

        let auth = build_model_auth(&roles, &no_env, &no_cmd);

        assert_eq!(auth["tier0"]["status"], "missing_credential:ANTHROPIC_API_KEY");
        assert_eq!(auth["tier2"]["status"], "missing_credential:ANTHROPIC_API_KEY");
        assert_eq!(auth["verifier_floor"]["status"], "missing_credential:ANTHROPIC_API_KEY");
        assert_eq!(auth["shim"]["status"], "missing_credential:ANTHROPIC_API_KEY");
    }

    #[test]
    fn build_model_auth_openai_compat_slot() {
        let roles = Roles {
            tier0: Some(detailed("qwen", Some("openai_compat"), Some("http://localhost:11434/v1"), None)),
            tier1: None,
            tier2: None,
            verifier_floor: None,
            shim: None,
        };

        let env_has = |k: &str| k == "OPENAI_API_KEY";
        let auth = build_model_auth(&roles, &env_has, &no_cmd);

        assert_eq!(auth["tier0"]["model"], "qwen");
        assert_eq!(auth["tier0"]["backend"], "openai_compat");
        assert_eq!(auth["tier0"]["status"], "ok");
    }

    #[test]
    fn build_model_auth_mock_slot() {
        let roles = Roles {
            tier0: Some(detailed("mock", Some("mock"), None, None)),
            tier1: None,
            tier2: None,
            verifier_floor: None,
            shim: None,
        };

        let auth = build_model_auth(&roles, &no_env, &no_cmd);
        assert_eq!(auth["tier0"]["backend"], "mock");
        assert_eq!(auth["tier0"]["status"], "ok");
    }
}
