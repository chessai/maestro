//! Config and profile types (ADR-007). These parse the TOML shape into typed
//! structures. Profile *resolution / precedence* is daemon policy and is NOT
//! implemented here — this module only parses.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// The whole config file (ADR-007): top-level key, `[defaults]`, and
/// `[profiles.<name>]` tables.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Config {
    /// Active-profile default; overridden by `--profile` / `MAESTRO_PROFILE`.
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Config {
    /// Parse a config from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(e.to_string()))
    }
}

/// Concurrency caps (ADR-003 / ADR-007). Expressed with dotted keys in TOML
/// (`concurrency.machine_cap`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Concurrency {
    #[serde(default = "default_machine_cap")]
    pub machine_cap: u32,
    #[serde(default = "default_advisor_cap")]
    pub advisor_cap: u32,
}

fn default_machine_cap() -> u32 {
    4
}
fn default_advisor_cap() -> u32 {
    2
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency {
            machine_cap: default_machine_cap(),
            advisor_cap: default_advisor_cap(),
        }
    }
}

/// Shim knobs (ADR-005 / ADR-007).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShimConfig {
    #[serde(default = "default_excerpt_cap")]
    pub excerpt_cap_chars: u32,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_hours: u32,
}

fn default_excerpt_cap() -> u32 {
    1500
}
fn default_cache_ttl() -> u32 {
    24
}

impl Default for ShimConfig {
    fn default() -> Self {
        ShimConfig {
            excerpt_cap_chars: default_excerpt_cap(),
            cache_ttl_hours: default_cache_ttl(),
        }
    }
}

/// Downgrade policy when the host can't meet a containment minimum (ADR-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DowngradePolicy {
    /// Run at best available level, shrink budgets, narrow allowlists.
    #[default]
    Tighten,
    /// Refuse to delegate at a lower level.
    Refuse,
}

/// Tighten factors applied on containment downgrade (ADR-004 / ADR-007).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TightenFactors {
    #[serde(default = "default_allowlist_factor")]
    pub allowlist_factor: f64,
    #[serde(default = "default_turn_factor")]
    pub turn_factor: f64,
}

fn default_allowlist_factor() -> f64 {
    0.5
}
fn default_turn_factor() -> f64 {
    0.6
}

impl Default for TightenFactors {
    fn default() -> Self {
        TightenFactors {
            allowlist_factor: default_allowlist_factor(),
            turn_factor: default_turn_factor(),
        }
    }
}

/// Task-lifetime ceiling factors (ADR-003 / ADR-007).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LifetimeFactors {
    /// Lifetime token ceiling = factor × sum(per-tier attempt token budgets).
    #[serde(default = "default_token_factor")]
    pub token_factor: f64,
    /// Lifetime wall-clock ceiling in minutes.
    #[serde(default = "default_wall_clock")]
    pub wall_clock_minutes: i64,
}

fn default_token_factor() -> f64 {
    1.0
}
fn default_wall_clock() -> i64 {
    30
}

impl Default for LifetimeFactors {
    fn default() -> Self {
        LifetimeFactors {
            token_factor: default_token_factor(),
            wall_clock_minutes: default_wall_clock(),
        }
    }
}

/// Streaming credential proxy config (ADR-006 / ADR-004). The proxy is the
/// daemon-local endpoint that injects the provider API key upstream, meters
/// token usage per response, and hard-stops a response mid-stream when a task's
/// token ceiling is exceeded. It is now ON BY DEFAULT: `enabled` defaults to
/// `true`, so live delegation routes the implementer (gated) and the verifier
/// (meter-only) through the proxy unless a profile explicitly disables it with
/// `proxy.enabled = false`. Startup degrades gracefully — if the proxy fails to
/// bind, the daemon logs a warning and backends fall back to direct calls (see
/// `start_proxy`). TOML shape (dotted keys land in the `proxy` table):
/// `proxy.enabled = false`, `proxy.addr = "127.0.0.1:0"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Whether to start the proxy at daemon startup. Default `true`; an omitted
    /// `[proxy]` table (or an omitted `proxy.enabled` key) yields `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The bind address, e.g. `"127.0.0.1:0"` (ephemeral port). Default
    /// `"127.0.0.1:0"`.
    #[serde(default = "default_proxy_addr")]
    pub addr: String,
}

fn default_true() -> bool {
    true
}
fn default_proxy_addr() -> String {
    "127.0.0.1:0".to_string()
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            enabled: default_true(),
            addr: default_proxy_addr(),
        }
    }
}

/// Advisor filesystem-write config (ADR-006 / ADR-007).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdvisorConfig {
    /// Informational model id; the advisor runs in Claude Code.
    #[serde(default)]
    pub model: Option<String>,
    /// `standard` | `1m`.
    #[serde(default)]
    pub context: Option<String>,
    /// In-repo globs made RW in the advisor mount; empty = fully read-only.
    #[serde(default)]
    pub writable_paths: Vec<String>,
}

/// The credentials file (ADR-007): a `[env]` table mapping env-var names to
/// string values. Unknown top-level keys are ignored. At daemon startup each
/// entry is injected into the process environment if the key is not already set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Credentials {
    /// `[env]` table: env-var name → value.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Credentials {
    /// Parse credentials from a TOML string. Unknown top-level keys are ignored
    /// (serde deny-unknown-fields is intentionally NOT used here).
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(e.to_string()))
    }
}

/// Stall auto-recovery action (ADR-009 Phase 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallAction {
    /// Snapshot the diff, kill the stalled session, and re-attempt same-tier
    /// (fix-in-place if edits were committed, else fresh). Default.
    #[default]
    SnapshotKillRetry,
    /// Emit the `stall_detected` event and leave the task for the advisor /
    /// the coarse watchdog. No automatic kill or retry.
    FlagOnly,
}

/// Daemon-side liveness monitoring config (ADR-009 Phase 2). Controls stall
/// detection + auto-recovery for driven sessions. TOML shape:
/// `monitoring.stall_timeout_seconds = 300`, `monitoring.stall_action = "snapshot_kill_retry"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitoringConfig {
    /// Seconds of zero PTY output (AND no journal state transition) before
    /// flagging a driven session as `suspected_stall`. Must be LESS than the
    /// coarse `watchdog_minutes` (the outer backstop). Default 300 (5 min).
    #[serde(default = "default_stall_timeout")]
    pub stall_timeout_seconds: u64,
    /// What to do on a detected stall. Default `snapshot_kill_retry`.
    #[serde(default)]
    pub stall_action: StallAction,
}

fn default_stall_timeout() -> u64 {
    300
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        MonitoringConfig {
            stall_timeout_seconds: default_stall_timeout(),
            stall_action: StallAction::default(),
        }
    }
}

/// The `[defaults]` table (ADR-007). Every knob has a value here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub concurrency: Concurrency,
    #[serde(default = "default_watchdog")]
    pub watchdog_minutes: u32,
    #[serde(default)]
    pub shim: ShimConfig,
    #[serde(default)]
    pub downgrade_policy: DowngradePolicy,
    #[serde(default)]
    pub tighten: TightenFactors,
    #[serde(default)]
    pub lifetime: LifetimeFactors,
    #[serde(default)]
    pub advisor: AdvisorConfig,
    #[serde(default)]
    pub containment: ContainmentConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub monitoring: MonitoringConfig,
}

fn default_watchdog() -> u32 {
    10
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            concurrency: Concurrency::default(),
            watchdog_minutes: default_watchdog(),
            shim: ShimConfig::default(),
            downgrade_policy: DowngradePolicy::default(),
            tighten: TightenFactors::default(),
            lifetime: LifetimeFactors::default(),
            advisor: AdvisorConfig::default(),
            containment: ContainmentConfig::default(),
            proxy: ProxyConfig::default(),
            monitoring: MonitoringConfig::default(),
        }
    }
}

/// A role model config (ADR-007). A tier may be a bare model string or a table
/// `{ model, kind, turn_budget }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoleModel {
    /// Bare model string, e.g. `"claude-sonnet-4-6"`.
    Bare(String),
    /// Full table form.
    Detailed(RoleModelTable),
}

impl RoleModel {
    /// The model id regardless of which form was used.
    pub fn model(&self) -> &str {
        match self {
            RoleModel::Bare(m) => m,
            RoleModel::Detailed(t) => &t.model,
        }
    }
}

/// The table form of a role model (ADR-007).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleModelTable {
    pub model: String,
    /// e.g. `driven_cli`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Backend base-URL override (ADR-008). For `anthropic` it overrides
    /// `$ANTHROPIC_BASE_URL`; for `openai_compat` it is required (reserved).
    #[serde(default)]
    pub base_url: Option<String>,
    /// For a `driven_cli` role (ADR-006 / M3): the CLI program to spawn over a
    /// PTY, e.g. `"claude"` or `"codex"`. Ignored by non-driven backends.
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to `command` for a `driven_cli` role, e.g.
    /// `["--print"]`. Ignored by non-driven backends.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Adapter selector for a `driven_cli` role. `"generic"` (default when
    /// unset) uses the interactive PTY / plan-echo path; `"claude"` uses the
    /// two-phase `--permission-mode plan` / `--permission-mode acceptEdits`
    /// path for the real `claude` CLI.
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub turn_budget: Option<i64>,
    /// Dollar cap passed to `claude --max-budget-usd <amount>` (ADR-006). When
    /// set, the driven role is API-billed (pay-per-token) and the CLI enforces
    /// the ceiling itself; the daemon must NOT strip provider API keys on this
    /// path so the CLI can authenticate per-token. `None` → subscription mode
    /// (keys stripped, flat-rate).
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
}

/// The `roles.*` table for a profile (ADR-007).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Roles {
    #[serde(default)]
    pub tier0: Option<RoleModel>,
    #[serde(default)]
    pub tier1: Option<RoleModel>,
    #[serde(default)]
    pub tier2: Option<RoleModel>,
    #[serde(default)]
    pub verifier_floor: Option<RoleModel>,
    /// The shim extraction model (ADR-005 / ADR-007). Verbatim span-mapping only,
    /// so it defaults to the cheapest configured model when unset — see
    /// [`Roles::shim_model`].
    #[serde(default)]
    pub shim: Option<RoleModel>,
}

impl Roles {
    /// The shim extraction model id (ADR-005). Defaults to `"claude-haiku-4-5"`
    /// (the cheapest configured model) when `roles.shim` is unset — the model's
    /// only job is verbatim span-mapping, which does not need a strong model.
    pub fn shim_model(&self) -> &str {
        self.shim
            .as_ref()
            .map(|r| r.model())
            .unwrap_or("claude-haiku-4-5")
    }
}

/// The OS-sandbox backend + network config for containment (ADR-004 / ADR-007).
/// TOML shape (dotted keys land in the `containment` table):
/// `containment.backend = "auto"`, `containment.network = "deny"`,
/// optionally `containment.devshell_variant` / `containment.podman_image`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainmentConfig {
    /// `auto` | `podman` | `bwrap` | `seatbelt` | `none` (ADR-004). `auto`
    /// prefers podman → bwrap → seatbelt → none.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// `deny` | `allow` network egress policy (default-deny).
    #[serde(default = "default_network")]
    pub network: String,
    /// L2 devShell variant (`devShells.<system>.<variant>`); `None` → default shell.
    #[serde(default)]
    pub devshell_variant: Option<String>,
    /// Podman backend image (required for the podman backend; it does not
    /// inherit the host toolchain).
    #[serde(default)]
    pub podman_image: Option<String>,
}

fn default_backend() -> String {
    "auto".to_string()
}
fn default_network() -> String {
    "deny".to_string()
}

impl Default for ContainmentConfig {
    fn default() -> Self {
        ContainmentConfig {
            backend: default_backend(),
            network: default_network(),
            devshell_variant: None,
            podman_image: None,
        }
    }
}

/// The per-tier containment floor (ADR-004 / ADR-007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContainmentMin {
    #[serde(default)]
    pub tier0: Option<u8>,
    #[serde(default)]
    pub tier1: Option<u8>,
    #[serde(default)]
    pub tier2: Option<u8>,
}

/// Search backend config (ADR-005 / ADR-007). An unset backend means
/// `backend_unavailable` on that host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// A `[profiles.<name>]` table (ADR-007). Every field is optional because a
/// profile overrides `[defaults]`; merging is daemon policy, not done here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub advisor: AdvisorConfig,
    #[serde(default)]
    pub roles: Roles,
    #[serde(default)]
    pub containment_min: ContainmentMin,
    /// Optional per-profile containment backend/network override (ADR-004).
    #[serde(default)]
    pub containment: Option<ContainmentConfig>,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub downgrade_policy: Option<DowngradePolicy>,
    /// Apply tighten factors to all driven-CLI specs.
    #[serde(default)]
    pub codex_tighten: Option<bool>,
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
    #[serde(default)]
    pub tighten: Option<TightenFactors>,
    #[serde(default)]
    pub lifetime: Option<LifetimeFactors>,
    #[serde(default)]
    pub shim: Option<ShimConfig>,
    #[serde(default)]
    pub watchdog_minutes: Option<u32>,
    #[serde(default)]
    pub monitoring: Option<MonitoringConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC5 (ADR-008): a role table with model + kind + base_url parses into
    // RoleModel::Detailed carrying all three fields.
    #[test]
    fn role_table_with_kind_and_base_url_parses() {
        let toml = r#"
[defaults]
[profiles.personal]
roles.tier0 = { model = "qwen", kind = "openai_compat", base_url = "http://localhost:11434/v1" }
"#;
        let cfg = Config::from_toml_str(toml).expect("config parses");
        let p = cfg.profiles.get("personal").expect("personal profile");
        match p.roles.tier0.as_ref().expect("tier0 set") {
            RoleModel::Detailed(t) => {
                assert_eq!(t.model, "qwen");
                assert_eq!(t.kind.as_deref(), Some("openai_compat"));
                assert_eq!(t.base_url.as_deref(), Some("http://localhost:11434/v1"));
            }
            other => panic!("tier0 should be a Detailed table, got {other:?}"),
        }
    }

    // The `containment` table parses on defaults and profiles via dotted keys,
    // and defaults are backend="auto" / network="deny" when unset.
    #[test]
    fn containment_table_parses_with_defaults() {
        let toml = r#"
[defaults]
containment.backend = "bwrap"
containment.network = "allow"

[profiles.p]
containment.backend = "podman"
containment.podman_image = "rust:1"
"#;
        let cfg = Config::from_toml_str(toml).expect("config parses");
        assert_eq!(cfg.defaults.containment.backend, "bwrap");
        assert_eq!(cfg.defaults.containment.network, "allow");
        assert_eq!(cfg.defaults.containment.devshell_variant, None);

        let p = cfg.profiles.get("p").expect("profile p");
        let pc = p.containment.as_ref().expect("profile containment set");
        assert_eq!(pc.backend, "podman");
        // network unset on the profile override → serde default "deny".
        assert_eq!(pc.network, "deny");
        assert_eq!(pc.podman_image.as_deref(), Some("rust:1"));

        // An entirely unset containment table defaults to auto / deny.
        let empty = Config::from_toml_str("[defaults]\n").unwrap();
        assert_eq!(empty.defaults.containment.backend, "auto");
        assert_eq!(empty.defaults.containment.network, "deny");
    }

    // ADR-005: roles.shim defaults to "claude-haiku-4-5" when unset, and an
    // explicit roles.shim (bare or table) overrides it.
    #[test]
    fn shim_model_defaults_to_haiku_and_overrides() {
        // Unset → the Haiku default.
        let unset = Roles::default();
        assert_eq!(unset.shim_model(), "claude-haiku-4-5");

        let toml = r#"
[defaults]
[profiles.personal]
roles.shim = "claude-haiku-4-5-fast"
[profiles.work]
roles.shim = { model = "cheap-local", base_url = "http://localhost:11434/v1" }
"#;
        let cfg = Config::from_toml_str(toml).expect("config parses");
        let personal = &cfg.profiles.get("personal").unwrap().roles;
        assert_eq!(personal.shim_model(), "claude-haiku-4-5-fast");
        let work = &cfg.profiles.get("work").unwrap().roles;
        assert_eq!(work.shim_model(), "cheap-local");
    }

    // Credentials: [env] table parses into a BTreeMap of env-var → value pairs.
    #[test]
    fn credentials_parses_env_table() {
        let toml = r#"
[env]
ANTHROPIC_API_KEY = "sk-ant-test"
OPENAI_API_KEY = "sk-openai-test"
"#;
        let creds = Credentials::from_toml_str(toml).expect("credentials parse");
        assert_eq!(creds.env.get("ANTHROPIC_API_KEY").map(|s| s.as_str()), Some("sk-ant-test"));
        assert_eq!(creds.env.get("OPENAI_API_KEY").map(|s| s.as_str()), Some("sk-openai-test"));
        assert_eq!(creds.env.len(), 2);
    }

    // Credentials: empty string → empty env map (no [env] section).
    #[test]
    fn credentials_empty_input_yields_empty_map() {
        let creds = Credentials::from_toml_str("").expect("empty credentials parse");
        assert!(creds.env.is_empty());

        let creds2 = Credentials::from_toml_str("\n# just a comment\n").expect("comment-only parse");
        assert!(creds2.env.is_empty());
    }

    // Credentials: unknown top-level keys are ignored gracefully.
    #[test]
    fn credentials_ignores_unknown_top_level_keys() {
        let toml = r#"
future_feature = "ignored"
[env]
ANTHROPIC_API_KEY = "sk-ant-test"
[another_unknown_table]
foo = "bar"
"#;
        let creds = Credentials::from_toml_str(toml).expect("credentials with unknown keys parse");
        assert_eq!(creds.env.len(), 1);
        assert_eq!(creds.env.get("ANTHROPIC_API_KEY").map(|s| s.as_str()), Some("sk-ant-test"));
    }

    // Credentials: no [env] section but other keys → empty map.
    #[test]
    fn credentials_no_env_section_yields_empty_map() {
        let toml = r#"
[other]
foo = "bar"
"#;
        let creds = Credentials::from_toml_str(toml).expect("no-env-section parse");
        assert!(creds.env.is_empty());
    }

    // ADR-006/ADR-004: the [proxy] table parses on defaults via dotted keys. The
    // proxy is now ON BY DEFAULT (`enabled` defaults to true), and addr defaults
    // to "127.0.0.1:0". An explicit `proxy.enabled = false` opts out.
    #[test]
    fn proxy_table_parses_with_defaults() {
        // Entirely unset → ENABLED by default, ephemeral-loopback addr.
        let empty = Config::from_toml_str("[defaults]\n").unwrap();
        assert!(empty.defaults.proxy.enabled);
        assert_eq!(empty.defaults.proxy.addr, "127.0.0.1:0");

        // A wholly-omitted config (Config::default) is also enabled.
        assert!(Config::default().defaults.proxy.enabled);

        // Explicit opt-out with a custom addr.
        let toml = r#"
[defaults]
proxy.enabled = false
proxy.addr = "127.0.0.1:8787"
"#;
        let cfg = Config::from_toml_str(toml).expect("config parses");
        assert!(!cfg.defaults.proxy.enabled);
        assert_eq!(cfg.defaults.proxy.addr, "127.0.0.1:8787");

        // Explicitly enabled but addr omitted → serde default addr.
        let toml2 = "[defaults]\nproxy.enabled = true\n";
        let cfg2 = Config::from_toml_str(toml2).expect("config parses");
        assert!(cfg2.defaults.proxy.enabled);
        assert_eq!(cfg2.defaults.proxy.addr, "127.0.0.1:0");

        // enabled omitted but addr set → enabled stays true (default), addr honored.
        let toml3 = "[defaults]\nproxy.addr = \"127.0.0.1:9999\"\n";
        let cfg3 = Config::from_toml_str(toml3).expect("config parses");
        assert!(cfg3.defaults.proxy.enabled);
        assert_eq!(cfg3.defaults.proxy.addr, "127.0.0.1:9999");
    }

    // A role table with max_budget_usd parses with the dollar cap set.
    #[test]
    fn role_table_with_max_budget_usd_parses() {
        let toml = r#"
[defaults]
[profiles.personal]
roles.tier0 = { model = "claude", kind = "driven_cli", command = "claude", adapter = "claude", max_budget_usd = 5.0 }
"#;
        let cfg = Config::from_toml_str(toml).expect("config parses");
        let p = cfg.profiles.get("personal").expect("personal profile");
        match p.roles.tier0.as_ref().expect("tier0 set") {
            RoleModel::Detailed(t) => {
                assert_eq!(t.model, "claude");
                assert_eq!(t.kind.as_deref(), Some("driven_cli"));
                assert_eq!(t.command.as_deref(), Some("claude"));
                assert_eq!(t.adapter.as_deref(), Some("claude"));
                assert_eq!(t.max_budget_usd, Some(5.0));
            }
            other => panic!("tier0 should be a Detailed table, got {other:?}"),
        }
    }

    // A bare-string role stays RoleModel::Bare (⇒ default anthropic backend,
    // no base_url).
    #[test]
    fn bare_role_stays_bare() {
        let toml = r#"
[defaults]
[profiles.personal]
roles.tier0 = "claude-sonnet-4-6"
"#;
        let cfg = Config::from_toml_str(toml).expect("config parses");
        let p = cfg.profiles.get("personal").expect("personal profile");
        assert!(matches!(
            p.roles.tier0.as_ref().unwrap(),
            RoleModel::Bare(m) if m == "claude-sonnet-4-6"
        ));
    }
}
