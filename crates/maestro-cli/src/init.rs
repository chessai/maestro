//! `maestro init` (ADR-006 / ADR-007): create the config-dir / data-dir /
//! state-dir and, if absent, write a starter `config.toml`; then ship the
//! advisor's client-side lockdown into the TARGET REPO (cwd) — Claude Code deny
//! rules, the project MCP registration, and a CLAUDE.md pointer.
//!
//! Idempotent throughout: the config is never overwritten, and the in-repo
//! files are merged (never clobbered destructively) so running `init` twice
//! adds nothing new.
//!
//! The deny rules here are *client-side defense-in-depth*: they are unverifiable
//! from the daemon and can be misconfigured. The load-bearing control is the
//! read-only filesystem mount built by `maestro advise` (ADR-006).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use maestro_journal::paths;

/// A starter `config.toml` based on the ADR-007 example: a `personal` profile
/// with sonnet/tier models and `default_profile = "personal"`.
const STARTER_CONFIG: &str = r#"default_profile = "personal"

[defaults]
concurrency.machine_cap = 4
concurrency.advisor_cap = 2
watchdog_minutes = 10
shim.excerpt_cap_chars = 1500
shim.cache_ttl_hours = 24
downgrade_policy = "tighten"          # tighten | refuse
tighten.allowlist_factor = 0.5        # applied on containment downgrade
tighten.turn_factor = 0.6
advisor.writable_paths = []           # in-repo globs made RW in the advisor mount; empty = repo fully read-only
lifetime.token_factor = 1.0           # task-lifetime token ceiling factor
lifetime.wall_clock_minutes = 30      # task-lifetime wall-clock ceiling

[profiles.personal]
advisor.model = "claude-fable-5"      # informational; advisor runs in Claude Code
advisor.context = "standard"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "codex", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-8"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 1, tier2 = 2 }
search.backend = "searxng"
search.endpoint = "https://searx.internal:8443"   # unreachable => backend_unavailable
"#;

/// The Claude Code tools whose native mutation is denied for the advisor
/// (ADR-006). Denying the whole tool (no arg pattern) applies in every
/// permission mode, including bypass.
const DENY_TOOLS: &[&str] = &["Edit", "Write", "Bash"];

/// Sentinel marking the maestro-managed section of a CLAUDE.md so we can append
/// it once and detect it on re-runs.
const CLAUDE_MD_MARKER: &str = "<!-- maestro:advisor-role -->";

/// The maestro CLAUDE.md section (fenced by the marker for idempotent append).
const CLAUDE_MD_SECTION: &str = "<!-- maestro:advisor-role -->
## maestro-managed repository

This repository is managed by **maestro**. You are the *advisor*: you plan and
review, but you do **not** edit files directly. The working tree is mounted
**read-only** — attempts to Edit/Write/Bash-mutate it will fail by construction.

- Implementation is **delegated**: use the maestro MCP tools (`delegate`,
  `task_status`, `close_task`, `journal_query`, …) to hand work to subagents.
- **Merges are human / `merge_task`** — verified work sits on a task branch and
  is never auto-merged.
- A writable scratch dir is provided for plans and notes; in-repo writes are
  limited to the opt-in `advisor.writable_paths` allowlist (default: none).
<!-- /maestro:advisor-role -->
";

/// Report of what [`run`] did, for message printing (and testing).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitReport {
    /// Whether a fresh config was written (`false` = left an existing one
    /// intact).
    pub wrote_config: bool,
    /// Per-file outcomes of the in-repo advisor lockdown.
    pub lockdown: LockdownReport,
}

/// What one in-repo lockdown file write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
    /// The file did not exist and was created.
    Created,
    /// The file existed and was updated (deny entries / MCP server / section
    /// merged in).
    Merged,
    /// The file existed and already had everything we would add.
    Unchanged,
}

impl FileOutcome {
    /// A short word for CLI output.
    pub fn label(self) -> &'static str {
        match self {
            FileOutcome::Created => "created",
            FileOutcome::Merged => "merged",
            FileOutcome::Unchanged => "unchanged",
        }
    }
}

/// Per-file outcomes for the three in-repo lockdown artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockdownReport {
    /// `.claude/settings.json`
    pub settings: FileOutcome,
    /// `.mcp.json`
    pub mcp: FileOutcome,
    /// `CLAUDE.md`
    pub claude_md: FileOutcome,
}

impl Default for LockdownReport {
    fn default() -> Self {
        LockdownReport {
            settings: FileOutcome::Unchanged,
            mcp: FileOutcome::Unchanged,
            claude_md: FileOutcome::Unchanged,
        }
    }
}

/// Run `maestro init`: ensure config/data/state dirs + starter config, then ship
/// the advisor lockdown into the current working directory (the target repo).
pub fn run() -> Result<InitReport> {
    let wrote_config = run_config_at(
        &paths::config_path(),
        &paths::data_dir(),
        &paths::state_dir(),
    )?;

    let repo = std::env::current_dir().context("resolving the current directory (target repo)")?;
    let mcp_bin = resolve_mcp_bin();
    let lockdown = write_lockdown(&repo, &mcp_bin)?;

    Ok(InitReport {
        wrote_config,
        lockdown,
    })
}

/// The testable core of the config+dirs step, parameterized over target paths.
/// Returns whether a fresh config was written.
pub fn run_config_at(config_path: &Path, data_dir: &Path, state_dir: &Path) -> Result<bool> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;

    if config_path.exists() {
        return Ok(false);
    }

    std::fs::write(config_path, STARTER_CONFIG)
        .with_context(|| format!("writing starter config {}", config_path.display()))?;
    Ok(true)
}

/// Resolve the `maestro-mcp` binary path for the project MCP registration, in
/// the same order as the daemon-bin resolution (ADR-006):
/// 1. `$MAESTRO_MCP_BIN` if set;
/// 2. a sibling `maestro-mcp` next to the current exe — checking both
///    `current_exe().parent()` and its parent;
/// 3. the bare name `"maestro-mcp"` (resolved on the advisor's `$PATH`).
pub fn resolve_mcp_bin() -> String {
    if let Some(v) = std::env::var_os("MAESTRO_MCP_BIN") {
        if !v.is_empty() {
            return v.to_string_lossy().into_owned();
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let mut candidates: Vec<PathBuf> = vec![dir.join("maestro-mcp")];
            if let Some(up) = dir.parent() {
                candidates.push(up.join("maestro-mcp"));
            }
            for c in candidates {
                if c.is_file() {
                    return c.to_string_lossy().into_owned();
                }
            }
        }
    }
    "maestro-mcp".to_string()
}

/// Write (or idempotently merge) the three in-repo advisor-lockdown files under
/// `repo`: `.claude/settings.json` (deny rules), `.mcp.json` (MCP registration),
/// and `CLAUDE.md` (advisor role pointer).
pub fn write_lockdown(repo: &Path, mcp_bin: &str) -> Result<LockdownReport> {
    let settings = merge_settings(&repo.join(".claude").join("settings.json"))?;
    let mcp = merge_mcp(&repo.join(".mcp.json"), mcp_bin)?;
    let claude_md = merge_claude_md(&repo.join("CLAUDE.md"))?;
    Ok(LockdownReport {
        settings,
        mcp,
        claude_md,
    })
}

/// Load a JSON object from `path`, or a fresh empty object if the file is
/// absent. Errors if present but not a JSON object.
fn load_json_object(path: &Path) -> Result<(Map<String, Value>, bool)> {
    if !path.exists() {
        return Ok((Map::new(), false));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok((Map::new(), false));
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing JSON from {}", path.display()))?;
    match value {
        Value::Object(map) => Ok((map, true)),
        _ => anyhow::bail!("{} is not a JSON object", path.display()),
    }
}

/// Pretty-write a JSON object to `path`, creating parent dirs.
fn write_json_object(path: &Path, obj: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dir {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(obj.clone()))
        .with_context(|| format!("serializing {}", path.display()))?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Merge the advisor deny rules into `.claude/settings.json`:
/// `permissions.deny` gains `Edit`/`Write`/`Bash` without dropping existing
/// entries or clobbering the rest of the settings.
fn merge_settings(path: &Path) -> Result<FileOutcome> {
    let (mut obj, existed) = load_json_object(path)?;

    // permissions object.
    let permissions = obj
        .entry("permissions".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let permissions = permissions
        .as_object_mut()
        .context("`permissions` in .claude/settings.json is not an object")?;

    // deny array.
    let deny = permissions
        .entry("deny".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let deny = deny
        .as_array_mut()
        .context("`permissions.deny` in .claude/settings.json is not an array")?;

    let mut added = false;
    for tool in DENY_TOOLS {
        let present = deny.iter().any(|v| v.as_str() == Some(tool));
        if !present {
            deny.push(Value::String((*tool).to_string()));
            added = true;
        }
    }

    if existed && !added {
        return Ok(FileOutcome::Unchanged);
    }
    write_json_object(path, &obj)?;
    Ok(if existed {
        FileOutcome::Merged
    } else {
        FileOutcome::Created
    })
}

/// Merge the project-scoped maestro MCP server into `.mcp.json` under
/// `mcpServers.maestro`. If a `maestro` server is already registered with the
/// same command, this is a no-op.
fn merge_mcp(path: &Path, mcp_bin: &str) -> Result<FileOutcome> {
    let (mut obj, existed) = load_json_object(path)?;

    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
        .as_object_mut()
        .context("`mcpServers` in .mcp.json is not an object")?;

    let desired = json!({ "command": mcp_bin });
    let already = servers.get("maestro") == Some(&desired);
    if existed && already {
        return Ok(FileOutcome::Unchanged);
    }
    servers.insert("maestro".to_string(), desired);
    write_json_object(path, &obj)?;
    Ok(if existed {
        FileOutcome::Merged
    } else {
        FileOutcome::Created
    })
}

/// Write `CLAUDE.md` if absent, or append the maestro advisor section once if
/// the file exists and does not already contain it.
fn merge_claude_md(path: &Path) -> Result<FileOutcome> {
    if !path.exists() {
        std::fs::write(path, CLAUDE_MD_SECTION)
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(FileOutcome::Created);
    }
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    if existing.contains(CLAUDE_MD_MARKER) {
        return Ok(FileOutcome::Unchanged);
    }
    let mut merged = existing;
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push('\n');
    merged.push_str(CLAUDE_MD_SECTION);
    std::fs::write(path, merged).with_context(|| format!("writing {}", path.display()))?;
    Ok(FileOutcome::Merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_config_creates_dirs_and_writes_config_then_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!("maestro-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let config = tmp.join("config/config.toml");
        let data = tmp.join("data");
        let state = tmp.join("state");

        let first = run_config_at(&config, &data, &state).unwrap();
        assert!(first, "first init should write the config");
        assert!(config.exists());
        assert!(data.is_dir());
        assert!(state.is_dir());
        let written = std::fs::read_to_string(&config).unwrap();
        assert!(written.contains("default_profile = \"personal\""));
        assert!(written.contains("[profiles.personal]"));

        // Idempotent: second run leaves the existing config intact.
        std::fs::write(&config, "user-edited = true\n").unwrap();
        let second = run_config_at(&config, &data, &state).unwrap();
        assert!(!second, "second init must not overwrite");
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "user-edited = true\n"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // AC4: init lockdown writes the three files, and re-running is idempotent
    // (no duplicate deny entries, no duplicated CLAUDE.md section).
    #[test]
    fn lockdown_writes_all_three_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let r1 = write_lockdown(repo, "/abs/maestro-mcp").unwrap();
        assert_eq!(r1.settings, FileOutcome::Created);
        assert_eq!(r1.mcp, FileOutcome::Created);
        assert_eq!(r1.claude_md, FileOutcome::Created);

        // .claude/settings.json deny rules.
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        for tool in DENY_TOOLS {
            assert!(
                deny.iter().any(|v| v.as_str() == Some(tool)),
                "deny must contain {tool}"
            );
        }

        // .mcp.json server registration.
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(repo.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["maestro"]["command"], "/abs/maestro-mcp");

        // CLAUDE.md present with the marker.
        let claude = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
        assert!(claude.contains(CLAUDE_MD_MARKER));

        // Second run: everything unchanged.
        let r2 = write_lockdown(repo, "/abs/maestro-mcp").unwrap();
        assert_eq!(r2.settings, FileOutcome::Unchanged);
        assert_eq!(r2.mcp, FileOutcome::Unchanged);
        assert_eq!(r2.claude_md, FileOutcome::Unchanged);

        // No duplicate deny entries.
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        let edit_count = deny.iter().filter(|v| v.as_str() == Some("Edit")).count();
        assert_eq!(edit_count, 1, "no duplicate deny entries");

        // The CLAUDE.md section appears exactly once.
        let claude = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
        assert_eq!(claude.matches(CLAUDE_MD_MARKER).count(), 1);
    }

    // Merging preserves pre-existing content in .claude/settings.json and
    // .mcp.json, and appends (does not clobber) an existing CLAUDE.md.
    #[test]
    fn lockdown_merges_into_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Pre-existing settings with an unrelated deny entry + other keys.
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(
            repo.join(".claude/settings.json"),
            r#"{ "permissions": { "deny": ["WebFetch"], "allow": ["Read"] }, "model": "x" }"#,
        )
        .unwrap();
        // Pre-existing .mcp.json with another server.
        std::fs::write(
            repo.join(".mcp.json"),
            r#"{ "mcpServers": { "other": { "command": "other-bin" } } }"#,
        )
        .unwrap();
        // Pre-existing CLAUDE.md with user content.
        std::fs::write(repo.join("CLAUDE.md"), "# My project\n\nHouse rules here.\n").unwrap();

        let r = write_lockdown(repo, "maestro-mcp").unwrap();
        assert_eq!(r.settings, FileOutcome::Merged);
        assert_eq!(r.mcp, FileOutcome::Merged);
        assert_eq!(r.claude_md, FileOutcome::Merged);

        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        // Kept the pre-existing entry and other keys.
        assert!(deny.iter().any(|v| v.as_str() == Some("WebFetch")));
        assert!(deny.iter().any(|v| v.as_str() == Some("Edit")));
        assert_eq!(settings["permissions"]["allow"][0], "Read");
        assert_eq!(settings["model"], "x");

        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(repo.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["other"]["command"], "other-bin");
        assert_eq!(mcp["mcpServers"]["maestro"]["command"], "maestro-mcp");

        let claude = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("# My project"), "kept user content");
        assert!(claude.contains(CLAUDE_MD_MARKER), "appended maestro section");
    }

    #[test]
    fn resolve_mcp_bin_honors_env() {
        let prev = std::env::var_os("MAESTRO_MCP_BIN");
        std::env::set_var("MAESTRO_MCP_BIN", "/tmp/custom-maestro-mcp");
        assert_eq!(resolve_mcp_bin(), "/tmp/custom-maestro-mcp");
        match prev {
            Some(v) => std::env::set_var("MAESTRO_MCP_BIN", v),
            None => std::env::remove_var("MAESTRO_MCP_BIN"),
        }
    }
}
