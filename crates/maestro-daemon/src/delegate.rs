//! The one-shot delegation pipeline (M1, ADR-003 / ADR-002 / ADR-006).
//!
//! On `Request::Delegate` the daemon:
//!   1. validates the spec (falsifiable acceptance criteria; resolvable model);
//!   2. creates the task row and emits `created`;
//!   3. spawns a background worker thread that: creates a worktree, records an
//!      implementer session, runs the implementer backend, then runs the
//!      mechanical gate, journaling each lifecycle transition.
//!
//! `delegate` returns the `task_id` immediately; the advisor observes progress
//! via `task_status` / the inbox. A machine-wide concurrency cap gates how many
//! workers run at once; over-cap tasks emit `queued` and start when a slot frees.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use maestro_journal::config::{Config, RoleModel};
use maestro_journal::domain::{
    ContainmentLevel, EventKind, ExitStatus, Independence, Role, SessionKind, Tier,
};
use maestro_journal::paths;
use maestro_journal::report::{ReportBody, Verdict};
use maestro_journal::spec::{CriterionKind, TaskSpec};
use maestro_journal::Journal;
use maestro_implementer::{
    AnthropicBackend, AnthropicVerifier, ImplementerBackend, ImplementerError, ImplementerTask,
    MockBackend, MockVerifier, VerifierBackend, VerifyTask,
};
use maestro_driver::{
    AnthropicPlanChecker, DrivenConfig, DrivenSession, EndReason, KillKind, MockPlanChecker,
    PlanChecker,
};

use crate::gate::{self, GateOutcome};
use crate::resolve::{resolve, ResolvedProfile};
use crate::verify_checkout::ThrowawayCheckoutRunner;
use crate::worktree;

/// Shared, process-wide delegation state: the single journal writer behind a
/// mutex, plus the machine-concurrency semaphore. Cloned (via `Arc`) into every
/// worker thread.
pub struct DelegationState {
    /// The single SQLite connection, shared read+write under a mutex.
    pub journal: Arc<Mutex<Journal>>,
    /// Machine concurrency limiter (ADR-003 `concurrency.machine_cap`).
    slots: Mutex<Slots>,
    slot_cv: Condvar,
    /// The daemon's `--profile` flag, so workers resolve tier→model identically.
    profile_flag: Option<String>,
    /// The host capability probe (ADR-004), cached once at startup — it is cheap
    /// but a subprocess call, so we do not re-probe per delegation.
    caps: maestro_sandbox::Capabilities,
    /// Live driven (PTY) sessions keyed by `task_id` (ADR-006). A driven worker
    /// registers its [`maestro_driver::SessionHandle`] before joining the driver
    /// and removes it after; `KillTask` looks the handle up to fire the
    /// break-glass kill path. Empty for tasks not running a driven session.
    live_sessions: Mutex<HashMap<String, maestro_driver::SessionHandle>>,
    /// The streaming credential proxy's shared token ledger (ADR-006 / ADR-004),
    /// present only when the proxy is enabled (OPT-IN; default `None`). When set,
    /// `run_pipeline` registers each task's token ceiling here so the proxy can
    /// meter usage + hard-stop. The implementer backend is routed through the
    /// proxy in `run_attempt` (via `proxy_addr` below) when this is `Some`.
    proxy_ledger: Option<Arc<maestro_proxy::Ledger>>,
    /// The bound address of the streaming credential proxy (ADR-006), present
    /// only when the proxy is enabled (now default-ON; `None` when it failed to
    /// bind or was disabled). When set, the one-shot implementer backend is built
    /// targeting `http://{addr}` INSTEAD of the role's upstream base_url, so its
    /// Anthropic calls route through the proxy (which injects the key + meters +
    /// hard-stops on the ceiling). The non-mock VERIFIER is also routed here, but
    /// METER-ONLY (via `X-Maestro-Meter` vs the implementer's gated
    /// `X-Maestro-Task`): the proxy meters its usage into the ledger so total task
    /// spend is accurate, but never gates or hard-stops it (ADR-002: verification
    /// is never skipped). The shim is never routed. `None` on the proxy-off path
    /// (all backends go direct).
    proxy_addr: Option<String>,
}

struct Slots {
    in_use: u32,
    cap: u32,
}

impl DelegationState {
    /// Build shared state from the opened journal and the resolved machine cap.
    /// `proxy_ledger` is `Some` only when the streaming credential proxy is
    /// enabled (OPT-IN); it is `None` on the default path.
    pub fn new(
        journal: Journal,
        machine_cap: u32,
        profile_flag: Option<String>,
        proxy_ledger: Option<Arc<maestro_proxy::Ledger>>,
        proxy_addr: Option<String>,
    ) -> Arc<Self> {
        Arc::new(DelegationState {
            journal: Arc::new(Mutex::new(journal)),
            slots: Mutex::new(Slots {
                in_use: 0,
                cap: machine_cap.max(1),
            }),
            slot_cv: Condvar::new(),
            profile_flag,
            caps: maestro_sandbox::probe(),
            live_sessions: Mutex::new(HashMap::new()),
            proxy_ledger,
            proxy_addr,
        })
    }

    /// Register a live driven session's handle under its `task_id` (ADR-006), so
    /// `KillTask` can reach it. Called by the driven worker before it joins.
    pub fn register_session(&self, task_id: &str, handle: maestro_driver::SessionHandle) {
        self.live_sessions
            .lock()
            .expect("live_sessions mutex poisoned")
            .insert(task_id.to_string(), handle);
    }

    /// Remove a task's live driven session handle (called after the driver
    /// returns, whatever the outcome).
    pub fn unregister_session(&self, task_id: &str) {
        self.live_sessions
            .lock()
            .expect("live_sessions mutex poisoned")
            .remove(task_id);
    }

    /// Fetch a clone of a task's live driven session handle, if one is running.
    pub fn session_handle(&self, task_id: &str) -> Option<maestro_driver::SessionHandle> {
        self.live_sessions
            .lock()
            .expect("live_sessions mutex poisoned")
            .get(task_id)
            .cloned()
    }

    /// Try to take a concurrency slot without blocking. Returns `true` on
    /// success (caller must later call [`Self::release_slot`]).
    fn try_take_slot(&self) -> bool {
        let mut slots = self.slots.lock().expect("slots mutex poisoned");
        if slots.in_use < slots.cap {
            slots.in_use += 1;
            true
        } else {
            false
        }
    }

    /// Block until a slot is free, then take it.
    fn take_slot_blocking(&self) {
        let mut slots = self.slots.lock().expect("slots mutex poisoned");
        while slots.in_use >= slots.cap {
            slots = self.slot_cv.wait(slots).expect("slot cv wait");
        }
        slots.in_use += 1;
    }

    /// Release a previously-taken slot and wake one waiter.
    fn release_slot(&self) {
        let mut slots = self.slots.lock().expect("slots mutex poisoned");
        slots.in_use = slots.in_use.saturating_sub(1);
        self.slot_cv.notify_one();
    }
}

/// A validation failure detected before (or at) task creation.
pub enum DelegateError {
    /// The spec failed validation; `reason` is a human message. Mapped to a
    /// terminal `failed(spec_rejected)` if a task was created, else returned raw.
    SpecRejected(String),
    /// The tier's model could not be resolved from the active profile.
    ModelUnavailable(String),
    /// An internal fault (journal write, config load).
    Internal(String),
}

impl DelegateError {
    /// The failure-taxonomy kind string for the journal payload.
    pub fn kind_str(&self) -> &'static str {
        match self {
            DelegateError::SpecRejected(_) => "spec_rejected",
            DelegateError::ModelUnavailable(_) => "model_unavailable",
            DelegateError::Internal(_) => "internal_error",
        }
    }

    /// The human message, safe to surface to the caller.
    pub fn message_public(&self) -> &str {
        match self {
            DelegateError::SpecRejected(m)
            | DelegateError::ModelUnavailable(m)
            | DelegateError::Internal(m) => m,
        }
    }
}

/// Record a terminal task for a pre-spawn delegation rejection so the lifecycle
/// is observable via `task_status` / the inbox. Creates a task row with a
/// synthetic minimal spec (we may not have a resolvable model, so the row is
/// best-effort), emits `created` then `failed(<kind>)`, and returns the task id.
///
/// Returns `None` if even the task row could not be written, in which case the
/// caller surfaces a plain error response.
pub fn record_rejected_task(
    state: &DelegationState,
    advisor_session_id: &str,
    err: &DelegateError,
) -> Option<String> {
    // A placeholder spec/tier so the row is well-formed; the failure payload
    // carries the real reason.
    let spec_json = serde_json::json!({ "title": "(rejected)" }).to_string();
    let task_id = {
        let journal = state.journal.lock().expect("journal mutex poisoned");
        journal
            .create_task(
                advisor_session_id,
                Tier::T0,
                "unknown",
                ContainmentLevel::L0,
                &spec_json,
                "HEAD",
                None,
                None,
                None,
            )
            .ok()?
    };
    emit(state, &task_id, EventKind::Created, None);
    let payload = failure_payload(err.kind_str(), err.message_public(), None);
    emit(state, &task_id, EventKind::Failed, Some(&payload));
    Some(task_id)
}

/// Load the on-disk config (defaults on any read/parse failure — the daemon must
/// keep serving). Mirrors the daemon's `load_config`.
fn load_config() -> Config {
    let path = maestro_journal::paths::config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => Config::from_toml_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

/// Resolve the active profile using the daemon flag + `MAESTRO_PROFILE`.
fn resolved_profile(profile_flag: Option<&str>) -> Result<ResolvedProfile, String> {
    let config = load_config();
    let env = std::env::var("MAESTRO_PROFILE").ok();
    resolve(profile_flag, env.as_deref(), &config).resolved
}

/// A fully resolved role for a tier: the model plus its backend selectors
/// (`kind` + `base_url`), carried from config through to the worker (ADR-008).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRole {
    pub model: String,
    pub kind: Option<String>,
    pub base_url: Option<String>,
    /// For a `driven_cli` role (ADR-006 / M3): the CLI program to spawn.
    pub command: Option<String>,
    /// Arguments passed to `command`.
    pub args: Option<Vec<String>>,
    /// Adapter selector for a `driven_cli` role: `"generic"` (default / unset)
    /// or `"claude"` for the two-phase permission-mode adapter.
    pub adapter: Option<String>,
    /// Dollar cap for `claude --max-budget-usd <amount>` (ADR-006). When set,
    /// the role is API-billed; provider API keys are NOT stripped so the CLI
    /// can authenticate per-token. `None` → subscription (keys stripped).
    pub max_budget_usd: Option<f64>,
}

/// The configured role for a tier off a resolved profile, if present. A bare
/// model string carries no `kind`/`base_url` (⇒ default anthropic backend).
fn role_for_tier(rp: &ResolvedProfile, tier: Tier) -> Option<ResolvedRole> {
    let role = match tier {
        Tier::T0 => rp.roles.tier0.as_ref(),
        Tier::T1 => rp.roles.tier1.as_ref(),
        Tier::T2 => rp.roles.tier2.as_ref(),
    }?;
    Some(match role {
        RoleModel::Bare(m) => ResolvedRole {
            model: m.clone(),
            kind: None,
            base_url: None,
            command: None,
            args: None,
            adapter: None,
            max_budget_usd: None,
        },
        RoleModel::Detailed(t) => ResolvedRole {
            model: t.model.clone(),
            kind: t.kind.clone(),
            base_url: t.base_url.clone(),
            command: t.command.clone(),
            args: t.args.clone(),
            adapter: t.adapter.clone(),
            max_budget_usd: t.max_budget_usd,
        },
    })
}

/// Select the implementer backend for a resolved role (ADR-008). Backend choice
/// is a profile decision, never hardcoded: it is driven by the role's `kind`
/// (and the `mock` model as a convenience). Unavailable/unknown kinds fail loud
/// as `model_unavailable` — never a silent fallback to the API backend.
pub fn select_backend(
    model: &str,
    kind: Option<&str>,
    base_url: Option<String>,
) -> Result<Box<dyn ImplementerBackend>, DelegateError> {
    // A bare `mock` model or an explicit `mock` kind → the deterministic backend.
    if model == "mock" || kind == Some("mock") {
        return Ok(Box::new(MockBackend));
    }
    match kind {
        // Proxy routing (ADR-006) is applied by the CALLER (`run_attempt`), which
        // — when the proxy is enabled — passes `base_url = http://{proxy_addr}`
        // here for the non-mock anthropic path and sets the task's
        // `X-Maestro-Task` header, so the proxy injects the key upstream, meters
        // usage into the ledger, and hard-stops on the ceiling. This function just
        // honors the `base_url` it is given; on the proxy-off path that is the
        // role's upstream base_url and the live path is byte-for-byte unchanged.
        None | Some("anthropic") => Ok(Box::new(AnthropicBackend::new(base_url))),
        Some("driven_cli") => Err(DelegateError::ModelUnavailable(
            "driven_cli backend is not available until M3".into(),
        )),
        Some("openai_compat") => Err(DelegateError::ModelUnavailable(
            "openai_compat backend is not yet implemented (ADR-008)".into(),
        )),
        Some(other) => Err(DelegateError::ModelUnavailable(format!(
            "unknown backend kind: {other}"
        ))),
    }
}

/// The containment floor for a tier off a resolved profile (0 if unset). The
/// task's `containment_min` can only *raise* this (ADR-003 / ADR-004).
fn containment_floor(rp: &ResolvedProfile, tier: Tier, spec_min: u8) -> u8 {
    let profile_min = match tier {
        Tier::T0 => rp.containment_min.tier0,
        Tier::T1 => rp.containment_min.tier1,
        Tier::T2 => rp.containment_min.tier2,
    }
    .unwrap_or(0);
    profile_min.max(spec_min)
}

/// The tightened changed-file-count cap for a task (ADR-004 / ADR-007), or
/// `None` when no tightening is active. This is the ONLY tightening lever with
/// real teeth in the current architecture: when tightening is active the gate
/// additionally rejects any diff that changes MORE files than
/// `ceil(allowlist_factor × allowlist_len)`, bounding blast radius on weakly-
/// contained / codex tasks. Pure given its inputs.
///
/// Tightening is ACTIVE iff the containment recipe was downgraded, OR the
/// profile's `codex_tighten` is set AND the role is a `driven_cli` role. With an
/// empty allowlist there is nothing to narrow (an empty allowlist already fails
/// any change), so the cap is `None`. Otherwise the cap is never below 1.
fn tighten_file_cap(
    rp: &ResolvedProfile,
    recipe: &ContainmentRecipe,
    role: &ResolvedRole,
    spec: &TaskSpec,
) -> Option<usize> {
    let active =
        recipe.downgraded || (rp.codex_tighten && role.kind.as_deref() == Some("driven_cli"));
    if !active {
        return None;
    }
    if spec.file_allowlist.is_empty() {
        return None;
    }
    let factor = rp.tighten.allowlist_factor.clamp(0.0, 1.0);
    let cap = (factor * spec.file_allowlist.len() as f64).ceil() as usize;
    Some(cap.max(1))
}

/// The default per-attempt turn budget when a spec sets none (ADR-006 / M3):
/// mirrors the implementer's `DEFAULT_TURN_BUDGET`.
const DEFAULT_TURN_BUDGET: u32 = 25;

/// The per-attempt TURN budget the structured `claude` driven adapter enforces
/// (ADR-004 / ADR-007). Pure given its inputs. The base budget is
/// `spec.budget.turns` (default [`DEFAULT_TURN_BUDGET`] when unset). When
/// tightening is ACTIVE — the SAME condition as [`tighten_file_cap`]: the
/// containment recipe was downgraded, OR `codex_tighten` is set AND the role is
/// a `driven_cli` role — the budget is shrunk to
/// `floor(turns × tighten.turn_factor)`, never below 1. When tightening is not
/// active the raw base budget is returned. Never 0.
fn effective_turn_cap(
    rp: &ResolvedProfile,
    recipe: &ContainmentRecipe,
    role: &ResolvedRole,
    spec: &TaskSpec,
) -> u32 {
    let turns = spec
        .budget
        .turns
        .map(|t| t.max(1) as u32)
        .unwrap_or(DEFAULT_TURN_BUDGET);
    let active =
        recipe.downgraded || (rp.codex_tighten && role.kind.as_deref() == Some("driven_cli"));
    if !active {
        return turns;
    }
    let factor = rp.tighten.turn_factor.clamp(0.0, 1.0);
    let tightened = ((turns as f64) * factor).floor() as u32;
    tightened.max(1)
}

/// The fully-resolved containment recipe for a task (ADR-004). Computed ONCE at
/// delegation time and reused for the whole task — escalation may raise the
/// model but never lowers containment, so the effective level is fixed here.
/// The per-attempt [`maestro_sandbox::SandboxSpec`] is built from this recipe
/// plus the attempt's worktree (the workspace differs per attempt).
#[derive(Debug, Clone)]
pub struct ContainmentRecipe {
    /// The effective (post-downgrade) level to run every surface at.
    pub level: maestro_sandbox::Level,
    /// The resolved OS-sandbox backend.
    pub backend: maestro_sandbox::Backend,
    /// Network egress policy from `containment.network`.
    pub network: maestro_sandbox::NetworkPolicy,
    /// L2 flake dir (the repo path).
    pub flake_dir: PathBuf,
    /// L2 devShell variant.
    pub devshell_variant: Option<String>,
    /// Podman image (required by the podman backend).
    pub podman_image: Option<String>,
    /// The requested floor before downgrade (for the downgrade event payload).
    pub requested: maestro_sandbox::Level,
    /// Whether the host forced a downgrade below `requested`.
    pub downgraded: bool,
}

impl ContainmentRecipe {
    /// Resolve the effective containment recipe (ADR-004): map the requested
    /// floor to a `sandbox::Level`, resolve the backend from config + caps, then
    /// compute the effective level + downgrade flag. Pure given its inputs.
    pub fn resolve(
        requested_floor: u8,
        containment: &maestro_journal::config::ContainmentConfig,
        caps: &maestro_sandbox::Capabilities,
        repo: &Path,
    ) -> Self {
        // A floor >2 is clamped to L2 (the max level); u8→Level is total here.
        let requested = maestro_sandbox::Level::from_u8(requested_floor.min(2))
            .unwrap_or(maestro_sandbox::Level::L0);
        let backend = maestro_sandbox::resolve_backend(&containment.backend, caps);
        let (level, downgraded) =
            maestro_sandbox::resolve_effective(requested, caps, backend);
        let network = if containment.network == "allow" {
            maestro_sandbox::NetworkPolicy::Allow
        } else {
            maestro_sandbox::NetworkPolicy::Deny
        };
        ContainmentRecipe {
            level,
            backend,
            network,
            flake_dir: repo.to_path_buf(),
            devshell_variant: containment.devshell_variant.clone(),
            podman_image: containment.podman_image.clone(),
            requested,
            downgraded,
        }
    }

    /// Build the per-attempt [`maestro_sandbox::SandboxSpec`] for a worktree.
    pub fn spec_for(&self, workspace: &Path) -> maestro_sandbox::SandboxSpec {
        maestro_sandbox::SandboxSpec {
            level: self.level,
            backend: self.backend,
            workspace: workspace.to_path_buf(),
            network: self.network,
            flake_dir: Some(self.flake_dir.clone()),
            devshell_variant: self.devshell_variant.clone(),
            podman_image: self.podman_image.clone(),
        }
    }

    /// The `ContainmentDowngraded` event payload: `{ requested, actual, backend }`.
    fn downgrade_payload(&self) -> String {
        serde_json::json!({
            "requested": self.requested.as_u8(),
            "actual": self.level.as_u8(),
            "backend": self.backend.to_string(),
        })
        .to_string()
    }
}

/// The verifier role off a resolved profile: `roles.verifier_floor` if set.
/// A bare model carries no kind/base_url.
fn verifier_role(rp: &ResolvedProfile) -> Option<ResolvedRole> {
    rp.roles.verifier_floor.as_ref().map(|role| match role {
        RoleModel::Bare(m) => ResolvedRole {
            model: m.clone(),
            kind: None,
            base_url: None,
            command: None,
            args: None,
            adapter: None,
            max_budget_usd: None,
        },
        RoleModel::Detailed(t) => ResolvedRole {
            model: t.model.clone(),
            kind: t.kind.clone(),
            base_url: t.base_url.clone(),
            command: t.command.clone(),
            args: t.args.clone(),
            adapter: t.adapter.clone(),
            max_budget_usd: t.max_budget_usd,
        },
    })
}

/// The provider prefix of a model id, for independence classification. We take
/// the segment before the first `-` (e.g. `claude-opus-4-8` → `claude`,
/// `gpt-5` → `gpt`, `mock` → `mock`). Good enough for the ADR-002 hierarchy.
fn provider_prefix(model: &str) -> &str {
    model.split('-').next().unwrap_or(model)
}

/// Classify verifier independence vs the implementer model (ADR-002):
/// same model → `FreshContextOnly`; same provider, different model →
/// `CrossModel`; else `CrossProvider`.
fn classify_independence(implementer_model: &str, verifier_model: &str) -> Independence {
    if implementer_model == verifier_model {
        Independence::FreshContextOnly
    } else if provider_prefix(implementer_model) == provider_prefix(verifier_model) {
        Independence::CrossModel
    } else {
        Independence::CrossProvider
    }
}

/// Choose the verifier for an attempt at `impl_tier` running `impl_model`
/// (ADR-002). Prefer `roles.verifier_floor`; else fall back to a configured tier
/// model that DIFFERS from the implementer. Returns the verifier role and its
/// classified independence, or `None` when no usable verifier exists at all
/// (→ the task fails `model_unavailable`; verification is never skipped).
fn select_verifier(rp: &ResolvedProfile, impl_model: &str) -> Option<(ResolvedRole, Independence)> {
    // 1. verifier_floor if configured (used even if it equals the implementer —
    //    that is FreshContextOnly, still allowed as a last resort).
    if let Some(role) = verifier_role(rp) {
        let indep = classify_independence(impl_model, &role.model);
        return Some((role, indep));
    }
    // 2. Fall back to a tier model DIFFERENT from the implementer.
    for tier in [Tier::T0, Tier::T1, Tier::T2] {
        if let Some(role) = role_for_tier(rp, tier) {
            if role.model != impl_model {
                let indep = classify_independence(impl_model, &role.model);
                return Some((role, indep));
            }
        }
    }
    // 3. No differing model; as a last resort, any configured tier model at all
    //    (fresh context only). If none configured, no verifier exists.
    for tier in [Tier::T0, Tier::T1, Tier::T2] {
        if let Some(role) = role_for_tier(rp, tier) {
            let indep = classify_independence(impl_model, &role.model);
            return Some((role, indep));
        }
    }
    None
}

/// Whether a resolved verifier role is the mock backend (never routed / no net).
fn verifier_role_is_mock(role: &ResolvedRole) -> bool {
    role.model == "mock" || role.kind.as_deref() == Some("mock")
}

/// Select the verifier backend for a resolved verifier role: `mock` → the
/// deterministic [`MockVerifier`], else the [`AnthropicVerifier`] built with
/// `effective_base_url` (the proxy address when the proxy is enabled and this is
/// a non-mock verifier — see `run_verifier` — else the role's own base_url).
fn select_verifier_backend(
    role: &ResolvedRole,
    effective_base_url: Option<String>,
) -> Box<dyn VerifierBackend> {
    if verifier_role_is_mock(role) {
        Box::new(MockVerifier)
    } else {
        Box::new(AnthropicVerifier::new(effective_base_url))
    }
}

/// The highest configured tier at or above `from`, i.e. the escalation ladder's
/// "top". A tier is "configured" iff `role_for_tier` resolves a model for it.
/// The ladder always includes `from` itself (its model was resolved at delegate
/// time), so the result is never below `from`.
fn top_tier(rp: &ResolvedProfile, from: Tier) -> Tier {
    let mut top = from;
    for tier in [Tier::T0, Tier::T1, Tier::T2] {
        if tier > from && role_for_tier(rp, tier).is_some() {
            top = tier;
        }
    }
    top
}

/// The next configured tier strictly above `tier`, if any (skips gaps).
fn next_configured_tier(rp: &ResolvedProfile, tier: Tier) -> Option<Tier> {
    [Tier::T1, Tier::T2]
        .into_iter()
        .find(|&candidate| candidate > tier && role_for_tier(rp, candidate).is_some())
}

/// The number of CONFIGURED tiers in the escalation ladder from `start` up to
/// (and including) `top_tier(rp, start)` (ADR-007). `start` always counts (its
/// model was resolved at delegate time); each configured tier strictly above
/// `start` and ≤ the ladder top is also counted (gaps are skipped). Never < 1.
fn ladder_tier_count(rp: &ResolvedProfile, start: Tier) -> i64 {
    let top = top_tier(rp, start);
    // `start` itself always counts, then walk configured tiers upward to `top`.
    let mut count = 1i64;
    let mut tier = start;
    while tier < top {
        match next_configured_tier(rp, tier) {
            Some(next) if next <= top => {
                count += 1;
                tier = next;
            }
            _ => break,
        }
    }
    count
}

/// Compute the task-lifetime token ceiling (ADR-003 / ADR-007). Pure given its
/// inputs. Precedence:
///   1. An explicit `spec.lifetime_budget.tokens` ALWAYS wins (unchanged).
///   2. Else, derive from `lifetime.token_factor` ONLY when a per-attempt token
///      budget (`spec.budget.tokens`) is set AND `token_factor > 0`:
///      `factor × per_attempt × num_tiers`, rounded, clamped up to at least a
///      single attempt's budget (`per_attempt`) so a ceiling can never be below
///      one attempt (which would guarantee immediate `budget_exhausted`).
///      `num_tiers` is the configured escalation ladder length from `spec.tier`
///      up to its top (v1: every tier shares `spec.budget.tokens`, so the sum
///      over the ladder is `per_attempt × num_tiers`).
///   3. Else → `None` (no token ceiling; preserves specs with no per-attempt
///      token budget).
fn derive_token_ceiling(spec: &TaskSpec, rp: &ResolvedProfile) -> Option<i64> {
    if let Some(v) = spec.lifetime_budget.tokens {
        return Some(v);
    }
    let per_attempt = spec.budget.tokens?;
    let factor = rp.lifetime.token_factor;
    if factor <= 0.0 {
        return None;
    }
    let num_tiers = ladder_tier_count(rp, spec.tier);
    let derived = (factor * (per_attempt as f64) * (num_tiers as f64)).round() as i64;
    Some(derived.max(per_attempt))
}

/// Validate a spec's acceptance criteria (ADR-003): every criterion must be a
/// command or a falsifiable invariant with a non-empty `check`. Adjective-only
/// or empty checks are rejected.
fn validate_spec(spec: &TaskSpec) -> Result<(), String> {
    if spec.acceptance_criteria.is_empty() {
        return Err("spec has no acceptance criteria".to_string());
    }
    for c in &spec.acceptance_criteria {
        if c.check.trim().is_empty() {
            return Err(format!(
                "acceptance criterion {} has an empty check (adjective-free, falsifiable checks required)",
                c.id
            ));
        }
        // Both Command and Invariant are acceptable *kinds*; the falsifiability
        // guard is the non-empty check above. (M1 does not statically parse an
        // invariant's semantics — only that it is a concrete statement.)
        match c.kind {
            CriterionKind::Command | CriterionKind::Invariant => {}
        }
    }
    Ok(())
}

/// Whether a spec's `check_commands` look BUILD-ONLY — they run at least one
/// command but none of them execute a test suite (operating-lesson L6). A
/// build-only gate compiles the deliverable but never runs its tests, so a task
/// can be "verified" while shipping test-file defects. This is a pure, heuristic
/// detector used only to WARN (never to reject): the advisor stays in control.
///
/// Returns `false` when there are no check commands at all (nothing to warn
/// about — an empty gate is a separate, pre-existing concern) or when ANY command
/// contains a recognizable test-runner token. The match is case-insensitive and
/// substring-based; it errs toward NOT warning (no false alarms) — e.g. a wrapper
/// script or a Makefile target like `make check` that internally runs tests is
/// assumed to test. Recognized tokens cover the common runners across ecosystems.
fn check_commands_look_build_only(check_commands: &[String]) -> bool {
    if check_commands.is_empty() {
        return false;
    }
    // Test-runner signals. Substring, lowercased. Kept deliberately broad so a
    // command that plausibly runs tests suppresses the warning (avoid false
    // positives that would train operators to ignore it).
    const TEST_TOKENS: &[&str] = &[
        "test",     // cargo test, go test, npm test, dotnet test, ctest, `make test`
        "nextest",  // cargo nextest
        "pytest",   // python
        "unittest", // python -m unittest
        "jest",     // js
        "vitest",   // js
        "mocha",    // js
        "rspec",    // ruby
        "phpunit",  // php
        "check",    // `make check`, `cargo check` is build-ish but `make check` tests;
                    // erring toward not-warning is the intended bias
        "spec",     // *_spec targets
        "junit",    // java
        "gradle",   // gradle test tasks (broad; suppresses to avoid false alarm)
    ];
    // Build-only iff NO command contains any test token.
    !check_commands.iter().any(|cmd| {
        let lc = cmd.to_lowercase();
        TEST_TOKENS.iter().any(|tok| lc.contains(tok))
    })
}

/// Append a task event under the shared journal lock. Errors are logged, not
/// propagated, so the worker's own control flow stays linear.
fn emit(state: &DelegationState, task_id: &str, kind: EventKind, payload: Option<&str>) {
    let journal = state.journal.lock().expect("journal mutex poisoned");
    if let Err(e) = journal.append_event(task_id, kind, payload) {
        tracing::warn!(task = task_id, ?kind, error = %e, "append_event failed");
    }
}

/// Record a session's outcome under the shared journal lock (ADR-008 metering
/// write-back). A `None` session id (insert failed earlier) is a no-op. Errors
/// are logged, not propagated, keeping the worker's control flow linear.
fn finish_session(
    state: &DelegationState,
    session_id: Option<&str>,
    exit_status: ExitStatus,
    turns: Option<i64>,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
) {
    let Some(session_id) = session_id else {
        return;
    };
    let journal = state.journal.lock().expect("journal mutex poisoned");
    if let Err(e) = journal.finish_session(session_id, exit_status, turns, tokens_in, tokens_out) {
        tracing::warn!(session = session_id, error = %e, "finish_session failed");
    }
}

/// Entry point for `Request::Delegate`. Validates the spec, resolves the model,
/// creates the task row, emits `created`, and spawns the background worker.
/// Returns the new `task_id` on success.
pub fn delegate(
    state: &Arc<DelegationState>,
    advisor_session_id: &str,
    repo_path: &str,
    spec: TaskSpec,
) -> Result<String, DelegateError> {
    // 1. Validate spec BEFORE any task row exists.
    if let Err(reason) = validate_spec(&spec) {
        return Err(DelegateError::SpecRejected(reason));
    }

    // Resolve tier → model and containment floor.
    let rp = resolved_profile(state.profile_flag.as_deref())
        .map_err(DelegateError::ModelUnavailable)?;
    let tier = spec.tier;
    let role = role_for_tier(&rp, tier).ok_or_else(|| {
        DelegateError::ModelUnavailable(format!(
            "no model configured for tier {} in the active profile",
            tier.as_int()
        ))
    })?;
    let model = role.model.clone();

    // Resolve backend availability EARLY — before any task row / worktree —
    // so an unavailable/unknown kind fails loud (`model_unavailable`) without
    // spawning a worktree, mirroring the model-missing path above (ADR-008).
    // The backend value itself is rebuilt in the worker; here we only validate.
    // A `driven_cli` role uses the PTY path (not `select_backend`); it validates
    // its `command` in the worker (`internal_error` if unset), so skip here.
    if role.kind.as_deref() != Some("driven_cli") {
        select_backend(&model, role.kind.as_deref(), role.base_url.clone())?;
    }

    // Resolve the EFFECTIVE containment recipe once (ADR-004): requested floor →
    // host caps + backend → (effective level, downgraded?). Reused for the whole
    // task so escalation never lowers containment.
    let floor = containment_floor(&rp, tier, spec.containment_min);
    let repo = PathBuf::from(repo_path);
    let recipe = ContainmentRecipe::resolve(floor, &rp.containment, &state.caps, &repo);
    // Record the ACTUAL effective level in `tasks.containment_level` (ADR-004
    // downgrade-and-tighten), not the requested floor.
    let containment = ContainmentLevel::try_from(recipe.level.as_u8())
        .map_err(|e| DelegateError::Internal(format!("invalid containment level: {e}")))?;

    let spec_json = serde_json::to_string(&spec)
        .map_err(|e| DelegateError::Internal(format!("serializing spec: {e}")))?;
    // Resolve a possibly-symbolic base_ref (e.g. "HEAD") to a concrete local
    // branch UP FRONT, so the natural spec value "just works" end-to-end: the
    // task branches off it AND `merge_task` can later fast-forward it (which
    // requires a local branch). The RESOLVED value is persisted in the task row
    // + used throughout the pipeline; `spec.base_ref` in the stored spec JSON is
    // left as-is (the spec is immutable). A SHA/tag/detached HEAD resolves to
    // itself (branching off it is valid; it is simply not merge_task-able).
    let base_ref = worktree::resolve_base_ref(&repo, &spec.base_ref);

    // Pin `base_ref` to the concrete commit SHA it points at NOW (operating-lesson
    // L3). Every attempt's worktree is cut from this exact commit, and the
    // scope/allowlist diff is taken against it — NOT the live `base_ref` tip. If a
    // sibling task merges into `base_ref` while this task runs, the base branch
    // advances but this task's scope check still diffs against the commit it was
    // cut from, so the newly-merged files never appear as out-of-allowlist
    // deletions → no spurious `scope_violation`. The MERGE TARGET stays the live
    // symbolic `base_ref` (so `merge_task` can still fast-forward it); only the
    // scope diff uses the pinned SHA. A ref that cannot be peeled to a commit
    // (bogus / empty repo) leaves the pin absent → the pipeline falls back to the
    // symbolic `base_ref`, preserving the prior behavior.
    let pinned_base = worktree::resolve_to_sha(&repo, &base_ref);

    // The workspace path is fixed by convention: <state>/worktrees/<task_id>.
    // We do not know the task_id until create_task mints it, so record the row
    // first, then note the intended workspace via the worker.
    let task_id = {
        let journal = state.journal.lock().expect("journal mutex poisoned");
        journal
            .create_task(
                advisor_session_id,
                tier,
                &model,
                containment,
                &spec_json,
                &base_ref,
                None,
                Some(repo_path),
                None,
            )
            .map_err(|e| DelegateError::Internal(format!("create_task: {e}")))?
    };

    // Warn (never reject) when the spec's check_commands look build-only — they
    // compile the deliverable but run no tests, so a "verified" verdict can hide
    // test-file defects (operating-lesson L6). Surface it BOTH to the operator
    // (tracing) and to the advisor (inlined in the `created` event payload so it
    // rides the inbox without a new event kind). The advisor stays in control.
    let created_payload = if check_commands_look_build_only(&spec.check_commands) {
        tracing::warn!(
            task = %task_id,
            check_commands = ?spec.check_commands,
            "spec check_commands look build-only (no test runner detected); a build-only \
             gate can report 'verified' while shipping test defects (L6)"
        );
        Some(
            serde_json::json!({
                "warning": "check_commands_build_only",
                "message": "check_commands compile but run no tests; the gate can report \
                            'verified' while test-file defects slip through. Add a \
                            test-running command (e.g. `cargo test`/`cargo nextest run`) \
                            to the acceptance gate.",
            })
            .to_string(),
        )
    } else {
        None
    };
    emit(state, &task_id, EventKind::Created, created_payload.as_deref());

    // 3. Spawn the background worker.
    let state = Arc::clone(state);
    let tid = task_id.clone();
    std::thread::spawn(move || {
        run_worker(state, tid, repo, base_ref, pinned_base, spec, role, recipe);
    });

    Ok(task_id)
}

/// The background worker: acquire a slot (queueing if at cap), create the
/// worktree, run the implementer, then the gate, journaling throughout.
#[allow(clippy::too_many_arguments)]
fn run_worker(
    state: Arc<DelegationState>,
    task_id: String,
    repo: PathBuf,
    base_ref: String,
    pinned_base: Option<String>,
    spec: TaskSpec,
    role: ResolvedRole,
    recipe: ContainmentRecipe,
) {
    // Emit the containment-downgraded event BEFORE the first `spawned` (ADR-004),
    // so the journal records the cap before any surface runs. Queueing may still
    // delay the spawn, but the downgrade is task-scoped, not attempt-scoped.
    if recipe.downgraded {
        emit(
            &state,
            &task_id,
            EventKind::ContainmentDowngraded,
            Some(&recipe.downgrade_payload()),
        );
        // Light-touch tighten (ADR-004): shrink the per-attempt turn budget by the
        // profile's `tighten.turn_factor`. Compute + log the tightened cap here.
        // For the STRUCTURED `claude` driven adapter this cap IS enforced: the
        // adapter hard-stops the execute phase once the observed turn count
        // exceeds it (`effective_turn_cap` computes the same value per attempt).
        // The generic driven path still carries a watchdog, not a turn cap.
        if let Ok(rp) = resolved_profile(state.profile_flag.as_deref()) {
            let requested_turns = spec
                .budget
                .turns
                .map(|t| t.max(1) as u32)
                .unwrap_or(DEFAULT_TURN_BUDGET);
            let turn_cap = effective_turn_cap(&rp, &recipe, &role, &spec);
            tracing::info!(
                task = %task_id,
                requested_turns,
                turn_cap,
                turn_factor = rp.tighten.turn_factor,
                "containment downgraded: structured claude adapter will enforce a tightened turn cap"
            );
        }
        // The allowlist cap (ADR-004) IS enforced at the mechanical gate: log the
        // computed changed-file cap so the tightening stays observable.
        if let Ok(rp) = resolved_profile(state.profile_flag.as_deref()) {
            if let Some(cap) = tighten_file_cap(&rp, &recipe, &role, &spec) {
                tracing::info!(
                    task = %task_id,
                    allowlist_len = spec.file_allowlist.len(),
                    allowlist_factor = rp.tighten.allowlist_factor,
                    changed_file_cap = cap,
                    "containment downgraded: gate will enforce a tightened changed-file cap"
                );
            }
        }
    }

    // Concurrency: take a slot; if none free, emit `queued` and block.
    if !state.try_take_slot() {
        emit(&state, &task_id, EventKind::Queued, None);
        state.take_slot_blocking();
    }
    // Ensure the slot is released no matter how the body exits.
    struct SlotGuard<'a>(&'a DelegationState);
    impl Drop for SlotGuard<'_> {
        fn drop(&mut self) {
            self.0.release_slot();
        }
    }
    let _slot = SlotGuard(&state);

    if let Err(fail) = run_pipeline(
        &state,
        &task_id,
        &repo,
        &base_ref,
        pinned_base.as_deref(),
        &spec,
        &role,
        &recipe,
    ) {
        // Any pipeline error is journaled as a terminal `failed`.
        let payload = failure_payload(fail.kind(), fail.message(), None);
        emit(&state, &task_id, EventKind::Failed, Some(&payload));
    }
}

/// A worker-internal failure with its taxonomy kind + message.
struct WorkerFailure {
    kind: &'static str,
    message: String,
}

impl WorkerFailure {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn message(&self) -> &str {
        &self.message
    }
}

fn fail(kind: &'static str, message: impl Into<String>) -> WorkerFailure {
    WorkerFailure {
        kind,
        message: message.into(),
    }
}

/// Build the `failed`-event JSON payload mirroring the failure taxonomy.
fn failure_payload(kind: &str, message: &str, extra: Option<serde_json::Value>) -> String {
    let mut obj = serde_json::json!({ "kind": kind, "message": message });
    if let (Some(map), Some(serde_json::Value::Object(extra))) = (obj.as_object_mut(), extra) {
        for (k, v) in extra {
            map.insert(k, v);
        }
    }
    obj.to_string()
}

/// Check the task-lifetime ceilings (ADR-003) at an attempt boundary. If a
/// ceiling is hit, emits a TERMINAL `failed(budget_exhausted)` and returns
/// `true` (the caller stops the whole task — a budget stop is not escalation
/// fuel). Returns `false` when the task is still within its budget.
///
/// Enforced at attempt boundaries (before each attempt and after each attempt's
/// implementer+verifier have written their metered usage). NOTE: a true
/// per-response / mid-stream token hard-stop needs the daemon API proxy
/// (ADR-006) and is deferred; M6 enforces via reported usage at these boundaries.
fn budget_exhausted(
    state: &DelegationState,
    task_id: &str,
    token_ceiling: Option<i64>,
    wall_clock_ceiling_minutes: Option<i64>,
) -> bool {
    // --- lifetime tokens: SUM over all sessions vs the ceiling ---------------
    if let Some(ceiling) = token_ceiling {
        let (tin, tout) = {
            let journal = state.journal.lock().expect("journal mutex poisoned");
            journal.task_token_totals(task_id).unwrap_or((0, 0))
        };
        if tin + tout >= ceiling {
            let payload = failure_payload(
                "budget_exhausted",
                "task hit its lifetime token ceiling",
                Some(serde_json::json!({
                    "reason": "lifetime_tokens",
                    "tokens_in": tin,
                    "tokens_out": tout,
                    "ceiling": ceiling,
                })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            return true;
        }
    }

    // --- lifetime wall-clock: minutes since the first `spawned` event --------
    if let Some(minutes) = wall_clock_ceiling_minutes {
        let first_spawn = {
            let journal = state.journal.lock().expect("journal mutex poisoned");
            journal.first_spawn_ts(task_id).unwrap_or(None)
        };
        // Only meaningful once a spawn has happened; before that, no elapsed clock.
        if let Some(ts) = first_spawn {
            if let Ok(spawned_at) =
                time::OffsetDateTime::parse(&ts, &time::format_description::well_known::Rfc3339)
            {
                let elapsed = time::OffsetDateTime::now_utc() - spawned_at;
                let elapsed_minutes = elapsed.whole_minutes();
                if elapsed_minutes >= minutes {
                    let payload = failure_payload(
                        "budget_exhausted",
                        "task hit its lifetime wall-clock ceiling",
                        Some(serde_json::json!({
                            "reason": "lifetime_wall_clock",
                            "elapsed_minutes": elapsed_minutes,
                            "ceiling_minutes": minutes,
                        })),
                    );
                    emit(state, task_id, EventKind::Failed, Some(&payload));
                    return true;
                }
            }
        }
    }

    false
}

/// The mechanical GATE's `checks_failed` details, carried into the NEXT attempt
/// so it can FIX-IN-PLACE instead of re-implementing from scratch (L15). When a
/// `checks_failed` is the terminal outcome of an attempt, the pipeline keeps the
/// worker's near-complete edits in the SAME worktree and injects this failing
/// command + its captured output into the next attempt's worker context, so the
/// worker fixes the specific error (e.g. a single trivial borrow-check error)
/// rather than rewriting everything and hitting a *different* trivial error.
#[derive(Debug, Clone)]
struct CheckFailure {
    /// The failing `check_command` (the first one the gate rejected).
    command: String,
    /// A bounded digest of that command's combined stdout+stderr.
    output_digest: String,
}

/// The outcome of one implementer+gate+verify attempt, as it feeds the
/// escalation control loop.
enum AttemptOutcome {
    /// The verifier passed: task is DONE (branch committed, journaled).
    Passed,
    /// A verification failure (checks_failed OR verify_failed) — escalation fuel.
    /// Carries the failed diff + report (if a verifier ran) for the next attempt.
    /// `check_failure` is `Some` ONLY when the mechanical gate's `check_commands`
    /// rejected the edits (a `checks_failed`, no verifier ran): the next attempt
    /// then REUSES this worktree (worker edits intact) and injects the failing
    /// command+output so the worker fixes-in-place rather than rewriting (L15).
    /// It is `None` for a model `verify_failed` (fresh worktree next attempt).
    VerificationFailed {
        diff: String,
        report: Option<ReportBody>,
        check_failure: Option<CheckFailure>,
    },
    /// Terminal, NOT escalation fuel: scope violation (already journaled).
    ScopeViolation,
}

/// The escalation control loop (ADR-003). Runs implementer→gate→verify attempts,
/// escalating the tier on repeated verification failures and blocking at the top.
///
/// Ladder: from `spec.tier` up to the top configured tier. Two verification
/// failures at a non-top tier → `escalated`, advance a tier. One failure at the
/// top → `blocked`. Bounded: 2 attempts per non-top tier, 1 at the top.
#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    state: &DelegationState,
    task_id: &str,
    repo: &Path,
    base_ref: &str,
    pinned_base: Option<&str>,
    spec: &TaskSpec,
    initial_role: &ResolvedRole,
    recipe: &ContainmentRecipe,
) -> Result<(), WorkerFailure> {
    // The commit-ish every attempt cuts its worktree from AND diffs against for
    // the scope/allowlist check (operating-lesson L3). Prefer the pinned SHA (the
    // concrete commit `base_ref` pointed at when the task spawned); fall back to
    // the symbolic `base_ref` when pinning was not possible. Distinct from the
    // MERGE target (`base_ref`), which stays the live ref so `merge_task` can
    // fast-forward it.
    let scope_base = pinned_base.unwrap_or(base_ref);
    // Resolve the profile once for verifier selection + the escalation ladder.
    let rp = resolved_profile(state.profile_flag.as_deref())
        .map_err(|e| fail("internal_error", format!("resolving profile: {e}")))?;

    // Resolve the task-lifetime ceilings ONCE (ADR-003 "Task-lifetime ceilings").
    // The per-attempt `budget` re-derives per tier; these bound the whole ladder.
    // - token_ceiling (ADR-007): an explicit `spec.lifetime_budget.tokens` always
    //   wins; else, when a per-attempt token budget is set and the profile's
    //   `lifetime.token_factor > 0`, derive a default ceiling of
    //   `token_factor × per_attempt × ladder_tier_count` (the sum of per-tier
    //   attempt budgets up the ladder, since v1 every tier shares
    //   `spec.budget.tokens`), clamped up to at least a single attempt's budget.
    //   With no per-attempt token budget (or factor ≤ 0) there is no ceiling —
    //   preserving specs that set no token budget. See `derive_token_ceiling`.
    // - wall_clock_ceiling_minutes: the spec if set, else the profile/config
    //   `lifetime.wall_clock_minutes` (default 30, large enough that fast tests
    //   never trip it).
    let token_ceiling: Option<i64> = derive_token_ceiling(spec, &rp);
    let wall_clock_ceiling_minutes: Option<i64> = spec
        .lifetime_budget
        .wall_clock_minutes
        .or(Some(rp.lifetime.wall_clock_minutes));

    // Proxy ledger registration (ADR-006 / ADR-004): when the streaming
    // credential proxy is enabled, tell its ledger this task's token ceiling so
    // it can meter usage and hard-stop once the ceiling is crossed. Registered
    // HERE — at pipeline start, before the first attempt runs — so the ceiling is
    // populated before the implementer's first proxied request. A `None` ceiling
    // is registered too (⇒ `over_budget` is well-defined = never over). The
    // implementer backend is routed through the proxy in `run_attempt`.
    if let Some(ledger) = state.proxy_ledger.as_ref() {
        ledger.register(task_id, token_ceiling);
    }

    let start_tier = spec.tier;
    let top = top_tier(&rp, start_tier);

    let mut tier = start_tier;
    let mut role = initial_role.clone();
    // Per-tier verification-failure counter; reset on each escalation.
    let mut tier_failures = 0u32;
    // Whole-task, monotonically increasing verifier report attempt number.
    let mut attempt: i64 = 0;
    // All prior verifier reports + the last failed diff, carried into escalated
    // attempts (never transcripts) (ADR-002 / ADR-003).
    let mut prior_reports: Vec<ReportBody> = Vec::new();
    let mut last_failed_diff: Option<String> = None;
    // FIX-IN-PLACE carryover (L15): when the previous attempt failed the
    // mechanical gate's `check_commands` (a `checks_failed`), this holds the
    // failing command + its output. When `Some`, the NEXT `run_attempt` REUSES
    // the previous worktree (worker edits intact, no reset to base_ref) and
    // injects the failing check into the worker's context so it fixes the
    // specific error instead of rewriting. Cleared after each attempt consumes
    // it; only ever set from a `checks_failed` outcome (never verify_failed).
    let mut pending_check_fix: Option<CheckFailure> = None;

    loop {
        // BEFORE each attempt: enforce the lifetime ceilings (ADR-003). A task
        // that keeps failing verification is cut off here rather than running
        // the full ladder. A budget stop is TERMINAL, not escalation fuel.
        if budget_exhausted(state, task_id, token_ceiling, wall_clock_ceiling_minutes) {
            return Ok(());
        }

        attempt += 1;
        // Consume any fix-in-place carryover from the prior attempt's
        // `checks_failed` (L15): when present, `run_attempt` reuses the existing
        // worktree and injects the failing check into the worker's context.
        let check_fix = pending_check_fix.take();
        let outcome = run_attempt(
            state,
            task_id,
            repo,
            scope_base,
            spec,
            &role,
            attempt,
            &rp,
            recipe,
            &prior_reports,
            last_failed_diff.as_deref(),
            check_fix.as_ref(),
        )?;

        match outcome {
            AttemptOutcome::Passed => return Ok(()),
            AttemptOutcome::ScopeViolation => return Ok(()),
            AttemptOutcome::VerificationFailed { diff, report, check_failure } => {
                // AFTER a failed attempt: the implementer + verifier sessions
                // have written their metered tokens, so the accumulated total is
                // now current. If a ceiling is hit, stop terminally BEFORE
                // considering escalation (a budget stop is not escalation fuel).
                // Only checked on the failure path: a Passed attempt already
                // committed + emitted `verify_passed` and must not be overwritten.
                if budget_exhausted(state, task_id, token_ceiling, wall_clock_ceiling_minutes) {
                    return Ok(());
                }
                tier_failures += 1;
                if let Some(r) = report {
                    prior_reports.push(r);
                }
                last_failed_diff = Some(diff);
                // FIX-IN-PLACE carryover (L15): a `checks_failed` (the mechanical
                // gate's `check_commands` rejected the edits — `check_failure` is
                // Some) means the worker's near-complete code is intact in the
                // worktree and needs a targeted fix, not a rewrite. Carry the
                // failing command+output so the NEXT attempt reuses that same
                // worktree and injects it. A `verify_failed` (check_failure None)
                // carries nothing → the next attempt gets a fresh worktree.
                //
                // When the loop is about to ESCALATE or STOP (blocked) this
                // carryover is dropped below, because those paths reset the flow:
                // an escalated (bigger) model or a blocked/abandoned task does not
                // fix-in-place on the same worktree. Only a same-tier retry reuses.
                pending_check_fix = check_failure;

                if tier == top {
                    // One failure after reaching the top → blocked (resting).
                    // Preserve the failed attempt's work in the journal so a
                    // superseded/abandoned task (whose worktree is later torn
                    // down) is not silently lost — same `partial_diff` shape as
                    // the kill/wedge snapshots.
                    let blocked_payload = serde_json::json!({
                        "partial_diff": last_failed_diff.as_deref().map(cap_diff),
                    })
                    .to_string();
                    emit(state, task_id, EventKind::Blocked, Some(&blocked_payload));
                    return Ok(());
                }
                if tier_failures >= 2 {
                    // Escalate: raise to the next configured tier (ADR-003).
                    let Some(next) = next_configured_tier(&rp, tier) else {
                        // No higher configured tier despite tier != top: treat
                        // as blocked (defensive; top_tier should preclude this).
                        let blocked_payload = serde_json::json!({
                            "partial_diff": last_failed_diff.as_deref().map(cap_diff),
                        })
                        .to_string();
                        emit(state, task_id, EventKind::Blocked, Some(&blocked_payload));
                        return Ok(());
                    };
                    let next_role = role_for_tier(&rp, next).ok_or_else(|| {
                        fail(
                            "model_unavailable",
                            format!("no model configured for tier {} on escalation", next.as_int()),
                        )
                    })?;
                    let payload = serde_json::json!({
                        "from_tier": tier.as_int(),
                        "to_tier": next.as_int(),
                        "reason": "verification_failed",
                    })
                    .to_string();
                    emit(state, task_id, EventKind::Escalated, Some(&payload));
                    tier = next;
                    role = next_role;
                    tier_failures = 0;
                    // On escalation the fix-in-place carryover is DROPPED (L15):
                    // the escalated (bigger) model starts on a FRESH worktree off
                    // base_ref with the prior verifier reports + last failed diff
                    // (ADR-003), not the smaller model's half-fixed worktree. Fix-
                    // in-place is a SAME-TIER retry optimization; escalation resets
                    // the approach.
                    pending_check_fix = None;
                    // Budgets re-derive from the new tier; containment is never
                    // lowered (ADR-003) — M2 re-derives model here.
                }
                // else: 1st failure at a non-top tier → retry same tier.
            }
        }
    }
}

/// Run a single attempt: fresh worktree, implementer, mechanical gate, and (on
/// checks_passed) the model verifier. Journals each transition. Returns the
/// attempt outcome; an `Err` is an infrastructure failure the caller journals as
/// terminal `failed`.
#[allow(clippy::too_many_arguments)]
fn run_attempt(
    state: &DelegationState,
    task_id: &str,
    repo: &Path,
    // The pinned base commit-ish (operating-lesson L3): every worktree is cut
    // from this exact commit and the scope/verifier diff is taken against it, so
    // the base branch advancing mid-task cannot cause a spurious scope violation.
    scope_base: &str,
    spec: &TaskSpec,
    role: &ResolvedRole,
    attempt: i64,
    rp: &ResolvedProfile,
    recipe: &ContainmentRecipe,
    prior_reports: &[ReportBody],
    last_failed_diff: Option<&str>,
    // FIX-IN-PLACE carryover (L15): `Some` iff the PREVIOUS attempt failed the
    // mechanical gate's `check_commands`. When set, this attempt REUSES the
    // existing worktree (worker edits intact — no reset to base_ref) and injects
    // the failing command + output into the worker's context so it fixes the
    // specific error rather than re-implementing. `None` → fresh worktree.
    check_fix: Option<&CheckFailure>,
) -> Result<AttemptOutcome, WorkerFailure> {
    let model = role.model.as_str();

    // A `driven_cli` role runs a PTY session (ADR-006 / M3) instead of the
    // one-shot API backend; the two share the mechanical gate + verifier tail.
    if role.kind.as_deref() == Some("driven_cli") {
        return run_driven_attempt(
            state,
            task_id,
            repo,
            scope_base,
            spec,
            role,
            attempt,
            rp,
            recipe,
            prior_reports,
            check_fix,
        );
    }

    // Select the backend up front (ADR-008). A kind that has become unavailable
    // still fails loud.
    //
    // Proxy routing (ADR-006): when the streaming credential proxy is enabled, the
    // IMPLEMENTER's Anthropic backend is built targeting the daemon-local proxy
    // (`http://{addr}`) INSTEAD of the role's upstream base_url, so the proxy
    // injects the key upstream, meters usage into the ledger, and hard-stops on
    // the ceiling. `select_backend`'s `mock` short-circuit still wins (a `mock`
    // model/kind is never routed through the proxy), so we only override the
    // base_url for the non-mock anthropic path. On the proxy-off path
    // (`proxy_addr` is `None`) the role's base_url is used unchanged — the live
    // path is byte-for-byte identical. The verifier + shim are never routed here.
    let is_mock = model == "mock" || role.kind.as_deref() == Some("mock");
    let backend_base_url = match state.proxy_addr.as_deref() {
        Some(addr) if !is_mock => Some(format!("http://{addr}")),
        _ => role.base_url.clone(),
    };
    let backend =
        select_backend(model, role.kind.as_deref(), backend_base_url).map_err(|e| {
            fail("model_unavailable", e.message_public().to_string())
        })?;

    // --- worktree: fix-in-place REUSE vs fresh per-attempt cut (L15 / L3) ---
    // Normally each attempt cuts a FRESH worktree off the PINNED base (L3),
    // discarding the prior attempt's edits. But when the prior attempt failed the
    // mechanical gate's `check_commands` (`check_fix` is Some), the worker's
    // near-complete code is intact and needs a targeted fix, not a rewrite — so
    // we REUSE the existing worktree (its edits preserved) rather than resetting
    // to base_ref. A missing/absent worktree (e.g. the first attempt, or one that
    // was torn down) falls back to a fresh cut, so this is always safe.
    let worktree_path = if check_fix.is_some() && worktree::reuse(repo, task_id) {
        worktree::worktree_path(task_id)
    } else {
        worktree::create(repo, scope_base, task_id)
            .map_err(|e| fail("internal_error", format!("worktree create: {e}")))?
    };

    let workspace = worktree_path.to_string_lossy().to_string();
    emit(state, task_id, EventKind::Spawned, None);
    let session_id = {
        let journal = state.journal.lock().expect("journal mutex poisoned");
        match journal.insert_session(
            Some(task_id),
            None,
            Role::Implementer,
            model,
            SessionKind::OneShotApi,
            Some(&workspace),
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(task = task_id, error = %e, "insert_session failed");
                None
            }
        }
    };

    // --- resolve house rules; escalated attempts prepend prior reports --
    let mut house_rules = match &spec.house_rules_ref {
        Some(rel) if !rel.trim().is_empty() => {
            let path = repo.join(rel);
            std::fs::read_to_string(&path).map_err(|e| {
                fail(
                    "internal_error",
                    format!("reading house_rules_ref {}: {e}", path.display()),
                )
            })?
        }
        _ => String::new(),
    };
    // FIX-IN-PLACE context injection (L15): when this attempt is reusing the
    // prior attempt's worktree after a `checks_failed`, tell the worker its code
    // is already present and it must FIX the specific failing check — not rewrite.
    // Prepended AHEAD of any prior-verifier-report preamble; both may be present.
    if let Some(cf) = check_fix {
        house_rules = format!("{}{}", check_fix_preamble(cf), house_rules);
    }
    if !prior_reports.is_empty() {
        // Prepend a summary of ALL prior reports + the last failed diff. Never
        // transcripts (ADR-002 / ADR-003).
        let mut preamble = String::new();
        preamble.push_str(
            "## Prior attempt failed verification\n\
             This task was re-run at a higher tier after verification failures. \
             The summaries below are prior verifier reports (not transcripts). \
             Address every blocker.\n\n",
        );
        for (i, r) in prior_reports.iter().enumerate() {
            preamble.push_str(&format!("### Prior verifier report {}\n", i + 1));
            preamble.push_str(&format!("verdict: {:?}\n", r.verdict));
            for f in &r.findings {
                preamble.push_str(&format!(
                    "- {:?} [{}]: {}\n",
                    f.severity,
                    f.criterion_id.as_deref().unwrap_or("-"),
                    f.evidence,
                ));
            }
            preamble.push('\n');
        }
        if let Some(diff) = last_failed_diff {
            preamble.push_str("### Last failed diff\n```diff\n");
            preamble.push_str(diff);
            preamble.push_str("\n```\n\n");
        }
        preamble.push_str(&house_rules);
        house_rules = preamble;
    }

    // --- run the implementer backend ------------------------------------
    emit(state, task_id, EventKind::Iterating, None);
    let task = ImplementerTask {
        spec: spec.clone(),
        worktree: worktree_path.clone(),
        house_rules,
        model: model.to_string(),
        // Route metered usage to this task in the proxy's ledger (ADR-006). Only
        // set when the proxy is enabled AND this is the non-mock anthropic path
        // (the backend base_url was overridden to the proxy above); `None`
        // otherwise, so no `X-Maestro-Task` header is sent on the default path.
        task_header: match state.proxy_addr.as_deref() {
            Some(_) if !is_mock => Some(task_id.to_string()),
            _ => None,
        },
    };
    match backend.run(&task) {
        Ok(outcome) => {
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Ok,
                Some(outcome.turns as i64),
                Some(outcome.tokens_in as i64),
                Some(outcome.tokens_out as i64),
            );
        }
        Err(ImplementerError::Unavailable(m)) => {
            finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
            worktree::remove(repo, &worktree_path);
            return Err(fail("model_unavailable", m));
        }
        // A Budget stop — the implementer's own turn-budget exhaustion OR the
        // proxy's pre-forward token-ceiling rejection (ADR-006) — is a terminal
        // `budget_exhausted`, NOT an `internal_error`. Special-cased BEFORE the
        // catch-all `other` arm. The worktree is removed on this path too.
        Err(ImplementerError::Budget(m)) => {
            finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
            worktree::remove(repo, &worktree_path);
            return Err(fail("budget_exhausted", m));
        }
        Err(other) => {
            finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
            worktree::remove(repo, &worktree_path);
            return Err(fail("internal_error", other.to_string()));
        }
    }
    emit(state, task_id, EventKind::ImplFinished, None);

    run_gate_and_verify(
        state,
        task_id,
        scope_base,
        spec,
        role,
        attempt,
        rp,
        recipe,
        prior_reports,
        &worktree_path,
    )
}

/// The watchdog duration for a driven session (ADR-006 / ADR-007): the profile's
/// `watchdog_minutes`, overridable by `MAESTRO_WATCHDOG_SECONDS` (tests set a
/// short value so the wedge path is fast).
fn watchdog_duration(rp: &ResolvedProfile) -> Duration {
    if let Ok(secs) = std::env::var("MAESTRO_WATCHDOG_SECONDS") {
        if let Ok(secs) = secs.trim().parse::<u64>() {
            return Duration::from_secs(secs.max(1));
        }
    }
    Duration::from_secs(u64::from(rp.watchdog_minutes.max(1)) * 60)
}

/// The PLAN-PHASE wall-clock ceiling for the structured `claude` adapter
/// (operating-lesson L4). The plan phase is turn-uncapped and only covered by the
/// IDLE watchdog, so an active-but-looping plan (one that keeps emitting output)
/// could run unbounded. This bounds it by absolute wall-clock, independent of
/// activity. Overridable by `MAESTRO_PLAN_CEILING_SECONDS` (tests set a short
/// value); otherwise a generous multiple of the watchdog — a plan may span a few
/// active turns but should never approach the task-lifetime ceiling — with a
/// 5-minute floor.
fn plan_ceiling_duration(rp: &ResolvedProfile) -> Duration {
    if let Ok(secs) = std::env::var("MAESTRO_PLAN_CEILING_SECONDS") {
        if let Ok(secs) = secs.trim().parse::<u64>() {
            return Duration::from_secs(secs.max(1));
        }
    }
    let watchdog = watchdog_duration(rp);
    // 5× the watchdog, floored at 5 minutes.
    (watchdog * 5).max(Duration::from_secs(5 * 60))
}

/// Build the task prompt handed to a driven CLI over the PTY (ADR-003 plan-echo):
/// title + instructions + allowlist, then the plan-echo instruction.
fn driven_prompt(spec: &TaskSpec) -> String {
    let allowlist = if spec.file_allowlist.is_empty() {
        "(none specified)".to_string()
    } else {
        spec.file_allowlist
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // NOTE: the prompt must NOT contain the bare plan marker token, because the
    // PTY echoes the prompt back and the plan extractor keys on the FIRST marker
    // occurrence. We spell the marker as PLAN<colon> in prose to avoid that.
    format!(
        "# Task\n## Title\n{title}\n\n## Instructions\n{instructions}\n\n\
         ## File allowlist — HARD BOUNDARY\n\
         You may ONLY create or modify files matching these paths/globs:\n{allowlist}\n\n\
         Any file OUTSIDE this list that you create or modify WILL BE DROPPED (it is \
         excluded from the commit), which typically breaks the build — so do NOT \
         split, extract, move, or restructure code into new files outside the \
         allowlist. If completing the task genuinely requires touching files outside \
         this list (e.g. a refactor), STOP and report that you need a wider allowlist \
         instead of working around it.\n\n\
         First output a single line beginning with the word PLAN followed by a \
         colon, summarizing your plan, then implement.\n",
        title = spec.title,
        instructions = spec.instructions,
        allowlist = allowlist,
    )
}

/// The env-var names to strip from a driven CLI's inherited environment
/// (ADR-006). When `max_budget_usd` is `Some` the role is API-billed
/// (pay-per-token): the CLI needs the provider API key to authenticate and to
/// have the dollar ceiling enforced — return an EMPTY strip list so the keys
/// flow through to the child. When `None` the role is subscription-backed
/// (flat-rate, unmetered): strip the standard provider API keys so a
/// subscription-authenticated CLI (claude, codex) doesn't accidentally bill
/// per-token via a key that happens to be in the daemon's environment.
pub(crate) fn driven_env_remove(max_budget_usd: Option<f64>) -> Vec<String> {
    if max_budget_usd.is_some() {
        // API-billed: retain all provider keys.
        vec![]
    } else {
        // Subscription: strip provider API keys.
        vec![
            "ANTHROPIC_API_KEY".into(),
            "ANTHROPIC_AUTH_TOKEN".into(),
            "OPENAI_API_KEY".into(),
            "CODEX_API_KEY".into(),
        ]
    }
}

/// Run a single DRIVEN (PTY) attempt (ADR-006 / M3). Spawns the configured CLI
/// over a PTY in a fresh worktree, runs the plan-echo gate, registers the live
/// session for the kill path, joins to completion, then maps the [`EndReason`]:
///
/// - `Completed` → mechanical gate + verifier tail (the CLI edited the worktree);
/// - `PlanRejected` → terminal `failed(plan_rejected)`, zero edits (not fuel);
/// - `TurnBudgetExceeded` → terminal `failed(turn_budget_exceeded)` with the cap
///   + a partial-diff snapshot (the structured adapter hard-stopped mid-session);
/// - `Wedged` → terminal `failed(session_wedged)` with a partial-diff snapshot;
/// - `Killed(Human|Advisor)` → `interrupted` + terminal `failed(interrupted_*)`
///   with a partial-diff snapshot;
/// - `Failed(msg)` → terminal `failed(internal_error)`.
#[allow(clippy::too_many_arguments)]
fn run_driven_attempt(
    state: &DelegationState,
    task_id: &str,
    repo: &Path,
    // The pinned base commit-ish (operating-lesson L3): the worktree is cut from
    // it and every diff (gate, verifier, forensic snapshot) is taken against it.
    scope_base: &str,
    spec: &TaskSpec,
    role: &ResolvedRole,
    attempt: i64,
    rp: &ResolvedProfile,
    recipe: &ContainmentRecipe,
    prior_reports: &[ReportBody],
    // FIX-IN-PLACE carryover (L15): `Some` iff the PREVIOUS attempt failed the
    // mechanical gate's `check_commands`. When set, reuse the existing worktree
    // (CLI edits intact) and inject the failing check into the driven prompt.
    check_fix: Option<&CheckFailure>,
) -> Result<AttemptOutcome, WorkerFailure> {
    let model = role.model.as_str();

    // The driven CLI program is required for a `driven_cli` role.
    let program = role
        .command
        .clone()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| {
            fail(
                "internal_error",
                "driven_cli role has no `command` configured",
            )
        })?;
    let args = role.args.clone().unwrap_or_default();

    // --- worktree: fix-in-place REUSE vs fresh per-attempt cut (L15 / L3) ---
    // Normally cut a FRESH worktree off the PINNED base each attempt (L3). On a
    // fix-in-place retry after a `checks_failed` (`check_fix` is Some) REUSE the
    // existing worktree so the CLI's near-complete edits are intact; a missing
    // worktree falls back to a fresh cut, so this is always safe.
    let worktree_path = if check_fix.is_some() && worktree::reuse(repo, task_id) {
        worktree::worktree_path(task_id)
    } else {
        worktree::create(repo, scope_base, task_id)
            .map_err(|e| fail("internal_error", format!("worktree create: {e}")))?
    };
    let workspace = worktree_path.to_string_lossy().to_string();

    // Wrap the driven CLI under the task's containment recipe (ADR-004): the CLI
    // runs contained. At L0 / Backend::None `wrap` is identity, so M3 behavior is
    // unchanged. A `wrap` error (e.g. podman needs an image) is an internal fault.
    let spec_sandbox = recipe.spec_for(&worktree_path);
    let wrapped = maestro_sandbox::wrap(&spec_sandbox, &program, &args)
        .map_err(|e| fail("internal_error", format!("sandbox wrap (driven): {e}")))?;
    let program = wrapped.program;
    let args = wrapped.args;

    // Per-session PTY log under data_dir (ADR-006 `sessions.log_path`).
    let log_dir = paths::data_dir().join("driven-logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{task_id}-{attempt}.log"));

    emit(state, task_id, EventKind::Spawned, None);
    let session_id = {
        let journal = state.journal.lock().expect("journal mutex poisoned");
        match journal.insert_session(
            Some(task_id),
            None,
            Role::Implementer,
            model,
            SessionKind::DrivenPty,
            Some(&workspace),
        ) {
            Ok(id) => {
                let _ = journal.set_session_log_path(&id, &log_path.to_string_lossy());
                Some(id)
            }
            Err(e) => {
                tracing::warn!(task = task_id, error = %e, "insert_session failed");
                None
            }
        }
    };

    // Choose the plan checker: `mock` model → deterministic; else Anthropic.
    let checker: Arc<dyn PlanChecker + Send + Sync> = if model == "mock" {
        Arc::new(MockPlanChecker)
    } else {
        Arc::new(AnthropicPlanChecker::new(role.base_url.clone()))
    };

    // The per-attempt turn cap the structured `claude` adapter enforces (ADR-004
    // / ADR-007): the tightened turn budget when tightening is active, else the
    // raw budget. The generic driven path ignores it.
    let turn_cap = effective_turn_cap(rp, recipe, role, spec);

    let watchdog = watchdog_duration(rp);
    // L4: bound the (turn-uncapped) plan phase by wall-clock. Only the structured
    // `claude` adapter reads it; the generic path ignores it.
    let plan_ceiling = plan_ceiling_duration(rp);

    // API-billed: when max_budget_usd is set, keep provider API keys so the
    // CLI can authenticate per-token and self-enforce the cap. Subscription
    // (None): strip the keys so the flat-rate CLI uses its subscription.
    if let Some(cap) = role.max_budget_usd {
        tracing::info!(
            task = %task_id,
            attempt,
            max_budget_usd = cap,
            "driven role is API-billed: provider API keys retained, CLI will enforce dollar cap"
        );
    }

    // FIX-IN-PLACE context injection (L15): on a reuse after `checks_failed`,
    // prepend the failing-check preamble to the driven prompt so the CLI fixes
    // the specific error in its already-present edits rather than rewriting.
    let prompt = match check_fix {
        Some(cf) => format!("{}{}", check_fix_preamble(cf), driven_prompt(spec)),
        None => driven_prompt(spec),
    };

    let config = DrivenConfig {
        program,
        args,
        cwd: worktree_path.clone(),
        prompt,
        log_path: log_path.clone(),
        watchdog,
        plan_marker: "PLAN:".to_string(),
        // Give the plan echo a little slack over the watchdog.
        plan_timeout: watchdog,
        turn_cap: Some(turn_cap),
        max_budget_usd: role.max_budget_usd,
        // L4: plan-phase wall-clock ceiling (structured `claude` adapter only).
        plan_ceiling: Some(plan_ceiling),
        // Strip provider API keys for subscription-authenticated CLIs (flat-
        // rate, unmetered). When max_budget_usd is set the role is API-billed
        // (pay-per-token): retain the keys so the CLI can authenticate and
        // self-enforce the dollar ceiling (ADR-006 `metered: true`). The
        // daemon's own in-process calls (plan checker, shim extraction,
        // one-shot API backends) are NOT affected — only the child's
        // inherited env is modified.
        env_remove: driven_env_remove(role.max_budget_usd),
    };

    emit(state, task_id, EventKind::Iterating, None);

    // Branch on the adapter: "claude" → two-phase permission-mode adapter;
    // else → generic interactive PTY / plan-echo path.
    let spawn_result = if role.adapter.as_deref() == Some("claude") {
        maestro_driver::run_claude_driven(config, spec.clone(), checker)
    } else {
        DrivenSession::spawn(config, spec.clone(), checker)
    };

    let (handle, join) = match spawn_result {
        Ok(pair) => pair,
        Err(e) => {
            finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
            worktree::remove(repo, &worktree_path);
            return Err(fail("internal_error", format!("driven spawn: {e}")));
        }
    };

    // Register the live session for the break-glass kill path, then join.
    state.register_session(task_id, handle);
    let result = join.join();
    state.unregister_session(task_id);

    let result = match result {
        Ok(r) => r,
        Err(_) => {
            finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
            worktree::remove(repo, &worktree_path);
            return Err(fail("internal_error", "driven session thread panicked"));
        }
    };

    let turns = Some(result.turns as i64);
    // The structured `claude` adapter reports real token usage from its
    // stream-json `result` events (ADR-006); the generic driven path leaves
    // these `None`. When present, driven sessions become metered in the journal.
    let tokens_in = result.tokens_in.map(|t| t as i64);
    let tokens_out = result.tokens_out.map(|t| t as i64);
    if let Some(cost) = result.cost_usd {
        tracing::info!(task = %task_id, attempt, cost_usd = cost, "driven session reported cost");
    }

    // A capped partial-diff snapshot for kill/wedge forensics (ADR-006). Taken
    // against the pinned base (L3) — the commit the worktree was cut from.
    let snapshot = || cap_diff(&worktree::snapshot_diff(&worktree_path, scope_base));

    match result.reason {
        EndReason::Completed => {
            // The structured adapter reports tokens (metered); the generic path
            // reports None (ADR-006 `metered: false`).
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Ok,
                turns,
                tokens_in,
                tokens_out,
            );
            emit(state, task_id, EventKind::ImplFinished, None);
            run_gate_and_verify(
                state,
                task_id,
                scope_base,
                spec,
                role,
                attempt,
                rp,
                recipe,
                prior_reports,
                &worktree_path,
            )
        }
        EndReason::PlanRejected { reason } => {
            // Terminal, ZERO edits (driver killed before edits) — NOT escalation
            // fuel; treated like a scope violation (ADR-003). Do not commit.
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Error,
                turns,
                tokens_in,
                tokens_out,
            );
            let payload = failure_payload(
                "plan_rejected",
                "driven CLI plan-echo rejected the plan before any edits",
                Some(serde_json::json!({ "reason": reason })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            worktree::remove(repo, &worktree_path);
            Ok(AttemptOutcome::ScopeViolation)
        }
        EndReason::Wedged => {
            let partial_diff = snapshot();
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Wedged,
                turns,
                tokens_in,
                tokens_out,
            );
            let payload = failure_payload(
                "session_wedged",
                "driven CLI produced no output past the watchdog timeout",
                Some(serde_json::json!({ "partial_diff": partial_diff })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            worktree::remove(repo, &worktree_path);
            Ok(AttemptOutcome::ScopeViolation)
        }
        EndReason::TurnBudgetExceeded => {
            // The execute phase exceeded the per-attempt turn cap and was
            // hard-stopped mid-session (ADR-004 / ADR-007). TERMINAL — a budget
            // stop is not escalation fuel; treated like a scope violation, same
            // as Wedged. The worktree may hold partial edits: snapshot them for
            // forensics, then remove the worktree.
            let partial_diff = snapshot();
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Error,
                turns,
                tokens_in,
                tokens_out,
            );
            let message = format!(
                "driven CLI exceeded its per-attempt turn cap of {turn_cap} and was hard-stopped"
            );
            let payload = failure_payload(
                "turn_budget_exceeded",
                &message,
                Some(serde_json::json!({
                    "reason": "turn_budget_exceeded",
                    "cap": turn_cap,
                    "partial_diff": partial_diff,
                })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            worktree::remove(repo, &worktree_path);
            Ok(AttemptOutcome::ScopeViolation)
        }
        EndReason::Killed(kind) => {
            let partial_diff = snapshot();
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Killed,
                turns,
                tokens_in,
                tokens_out,
            );
            let (fail_kind, kind_label) = match kind {
                KillKind::Human => ("interrupted_human", "human"),
                KillKind::Advisor => ("interrupted_advisor", "advisor"),
            };
            // First the `interrupted` event (payload: reason=kill + snapshot),
            // then the terminal `failed(interrupted_*)` (ADR-006).
            let interrupted_payload = serde_json::json!({
                "reason": "kill",
                "kill_kind": kind_label,
                "partial_diff": partial_diff,
            })
            .to_string();
            emit(state, task_id, EventKind::Interrupted, Some(&interrupted_payload));
            let payload = failure_payload(
                fail_kind,
                "driven CLI session killed",
                Some(serde_json::json!({ "partial_diff": partial_diff })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            worktree::remove(repo, &worktree_path);
            Ok(AttemptOutcome::ScopeViolation)
        }
        EndReason::Failed(msg) => {
            finish_session(
                state,
                session_id.as_deref(),
                ExitStatus::Error,
                turns,
                tokens_in,
                tokens_out,
            );
            let payload = failure_payload("internal_error", &msg, None);
            emit(state, task_id, EventKind::Failed, Some(&payload));
            worktree::remove(repo, &worktree_path);
            Ok(AttemptOutcome::ScopeViolation)
        }
    }
}

/// Build the FIX-IN-PLACE preamble (L15) prepended to the worker's house rules
/// when an attempt reuses the prior attempt's worktree after a `checks_failed`.
/// It tells the worker (a) its previous edits are already present in the
/// worktree, (b) which `check_command` failed, and (c) that it must make the
/// SMALLEST targeted change to fix that specific error rather than rewriting.
/// Pure given its input.
fn check_fix_preamble(cf: &CheckFailure) -> String {
    format!(
        "## Fix the failing check — do NOT rewrite\n\
         Your previous edits for this task ARE ALREADY PRESENT in this worktree. \
         They compiled/ran far enough to reach the acceptance gate, but ONE check \
         command failed. Do NOT start over or rewrite working code — read the \
         error below and make the SMALLEST change that fixes exactly that error, \
         then stop. Rewriting from scratch typically just trades this error for a \
         different trivial one.\n\n\
         ### Failing check command\n```\n{command}\n```\n\n\
         ### Its output (stdout+stderr, truncated)\n```\n{output}\n```\n\n",
        command = cf.command,
        output = cf.output_digest,
    )
}

/// Cap a partial-diff snapshot to a forensically-useful size for the terminal
/// event payload (ADR-006). Longer diffs are truncated with a marker.
fn cap_diff(diff: &str) -> String {
    const CAP: usize = 4000;
    if diff.len() <= CAP {
        diff.to_string()
    } else {
        let mut s = diff[..CAP].to_string();
        s.push_str("\n…[truncated]");
        s
    }
}

/// The post-implementer mechanical gate (ADR-002) + model verifier tail, shared
/// by the one-shot API path and the driven-CLI path (whose CLI edited the same
/// worktree). Diffs `worktree_path` vs `base_ref`, enforces the allowlist +
/// check commands, and on pass runs the verifier and commits the branch.
#[allow(clippy::too_many_arguments)]
fn run_gate_and_verify(
    state: &DelegationState,
    task_id: &str,
    // The pinned base commit-ish (operating-lesson L3): the scope/allowlist diff,
    // the checks-failed diff, and the verifier's structural diff are ALL taken
    // against this exact commit — the one the worktree was cut from — never the
    // live `base_ref` tip. This makes the scope check immune to the base branch
    // advancing while the task runs.
    scope_base: &str,
    spec: &TaskSpec,
    role: &ResolvedRole,
    attempt: i64,
    rp: &ResolvedProfile,
    recipe: &ContainmentRecipe,
    prior_reports: &[ReportBody],
    worktree_path: &Path,
) -> Result<AttemptOutcome, WorkerFailure> {
    let model = role.model.as_str();

    // --- mechanical gate (ADR-002) --------------------------------------
    // The gate is a verification surface: it runs under the SAME containment
    // recipe as the task (ADR-004 "verification surfaces inherit the task
    // recipe"). At L0 the wrap is identity, so behavior is unchanged from M3.
    let sandbox = recipe.spec_for(worktree_path);
    // Tightened blast-radius cap (ADR-004 / ADR-007): when tightening is active
    // (containment downgrade OR codex_tighten on a driven_cli role), the gate
    // additionally rejects a diff that changes more files than the tightened cap.
    let max_changed_files = tighten_file_cap(rp, recipe, role, spec);
    emit(state, task_id, EventKind::ChecksStarted, None);
    let gate_outcome = gate::run(
        worktree_path,
        scope_base,
        &spec.file_allowlist,
        max_changed_files,
        &spec.check_commands,
        &sandbox,
    )
    .map_err(|e| fail("internal_error", format!("gate: {e}")))?;

    match gate_outcome {
        GateOutcome::ScopeViolation { offending } => {
            // TERMINAL — not escalation fuel (ADR-003).
            let payload = failure_payload(
                "scope_violation",
                "changed paths fell outside the file allowlist",
                Some(serde_json::json!({ "offending_paths": offending })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            Ok(AttemptOutcome::ScopeViolation)
        }
        GateOutcome::TightenedScopeExceeded { changed, cap, files } => {
            // TERMINAL — the diff stayed in-allowlist but exceeded the tightened
            // changed-file cap (containment downgrade / codex_tighten). Still a
            // scope violation (reuse the kind), NOT escalation fuel (ADR-004).
            let message = format!(
                "changed {changed} files, exceeding the tightened allowlist cap of {cap} \
                 (containment downgrade / codex_tighten)"
            );
            let payload = failure_payload(
                "scope_violation",
                &message,
                Some(serde_json::json!({
                    "reason": "tightened_allowlist",
                    "changed": changed,
                    "cap": cap,
                    "offending_paths": files,
                })),
            );
            emit(state, task_id, EventKind::Failed, Some(&payload));
            Ok(AttemptOutcome::ScopeViolation)
        }
        GateOutcome::ChecksFailed {
            command,
            output_digest,
            changed,
        } => {
            let payload = serde_json::json!({
                "command": command,
                "output_digest": output_digest,
            })
            .to_string();
            emit(state, task_id, EventKind::ChecksFailed, Some(&payload));
            // DURABILITY (L15 / CLAUDE.md cautionary case): commit the worker's
            // in-allowlist edits to the task branch NOW, before this attempt
            // returns. A same-tier fix-in-place retry REUSES this worktree, and in
            // the live loss-loop the worker's own `git reset` on that retry wiped
            // the UNCOMMITTED near-complete edits back to base — unrecoverable.
            // Committing here makes the retry resume from a REAL commit regardless
            // of any later working-tree reset: `reset --hard HEAD` now RESTORES the
            // edits instead of discarding them. We commit EXACTLY the gate's
            // in-allowlist `changed` set (captured before the check commands ran,
            // so never `target/` or other artifact/out-of-scope paths), via the
            // same `commit_paths` the checks-PASSED path uses. Best-effort: a
            // commit error is logged, not fatal.
            //
            // On ESCALATION the carryover is dropped and `run_attempt` cuts a FRESH
            // worktree off `base_ref` (`worktree::create` force-deletes the task
            // branch first), so this fix-commit never survives a tier bump — a
            // bigger model always re-approaches from base (ADR-003 / decision
            // table). Only a same-tier `checks_failed` retry resumes from it.
            if let Err(e) = worktree::commit_paths(
                worktree_path,
                &changed,
                &format!("maestro: fix-in-place checkpoint — {}", spec.title),
            ) {
                tracing::warn!(
                    task = task_id,
                    error = %e,
                    "checks_failed: committing in-allowlist edits for fix-in-place durability failed"
                );
            }
            // Counts as a verification failure (ADR-003). No verifier ran, so no
            // report; carry the diff for the next attempt's context. AND carry the
            // failing command+output as a fix-in-place hint (L15): the next
            // same-tier attempt reuses THIS worktree (edits committed above) and
            // injects this so the worker fixes the specific error, not rewrites.
            //
            // Scope the carried diff to the in-allowlist `changed` set (adversarial
            // review, Finding 1): a plain `diff` here re-`add -A`s and would fold any
            // artifacts a check command wrote into the escalation context. `diff_paths`
            // keeps `last_failed_diff` artifact-free.
            let diff = worktree::diff_paths(worktree_path, scope_base, &changed).unwrap_or_default();
            Ok(AttemptOutcome::VerificationFailed {
                diff,
                report: None,
                check_failure: Some(CheckFailure {
                    command,
                    output_digest,
                }),
            })
        }
        GateOutcome::Passed { changed } => {
            emit(state, task_id, EventKind::ChecksPassed, None);
            // --- model verifier (ADR-002) -------------------------------
            let gate_output = "mechanical gate: allowlist ok; check commands ok".to_string();
            // Restrict the verifier's structural input to the implementer's
            // in-allowlist changed files, captured at the scope-check step BEFORE
            // the check commands ran (ADR-002). This keeps post-build artifacts
            // (e.g. `target/`) that a check command created out of the diff even
            // when the repo has no `.gitignore` — a plain `add -A` diff would
            // otherwise stage them.
            let diff = worktree::diff_paths(worktree_path, scope_base, &changed)
                .map_err(|e| fail("internal_error", format!("diff for verifier: {e}")))?;

            let report = run_verifier(
                state,
                task_id,
                spec,
                rp,
                model,
                attempt,
                &diff,
                &gate_output,
                prior_reports,
                worktree_path,
                recipe,
            )?;

            match report.verdict {
                Verdict::Pass => {
                    // Commit ONLY the implementer's in-allowlist changed files;
                    // leave for human merge — NEVER auto-merge. Restricting to the
                    // gate's pre-build `changed` set keeps build artifacts (e.g.
                    // `target/`) that a check command created out of the committed
                    // branch, even without a `.gitignore`. Commit BEFORE emitting
                    // `verify_passed` so an observer that sees that terminal state
                    // (and may immediately `merge_task`) always finds the branch
                    // already committed — no observe-before-commit race.
                    worktree::commit_paths(
                        worktree_path,
                        &changed,
                        &format!("maestro: {}", spec.title),
                    )
                    .map_err(|e| fail("internal_error", format!("commit: {e}")))?;
                    emit(state, task_id, EventKind::VerifyPassed, None);
                    Ok(AttemptOutcome::Passed)
                }
                Verdict::Fail => {
                    emit(state, task_id, EventKind::VerifyFailed, None);
                    // A MODEL verify_failed (not a mechanical checks_failed): no
                    // fix-in-place hint — the next attempt gets a fresh worktree
                    // and re-implements with the verifier's report as context.
                    Ok(AttemptOutcome::VerificationFailed {
                        diff,
                        report: Some(report),
                        check_failure: None,
                    })
                }
            }
        }
    }
}

/// Run the model verifier for an attempt (ADR-002): select a verifier distinct
/// from the implementer, emit `verify_started`, insert a verifier session, run
/// `verify` (retrying once on a crash/invalid report), persist the report, and
/// finish the session with its tokens. Verifier budget is NOT charged to the
/// implementer.
///
/// Failure modes: no usable verifier → `model_unavailable`; a `verify` error
/// (crash / invalid) → retry once with a fresh session, second failure →
/// `internal_error`; the verifier model `Unavailable` → `model_unavailable`.
#[allow(clippy::too_many_arguments)]
fn run_verifier(
    state: &DelegationState,
    task_id: &str,
    spec: &TaskSpec,
    rp: &ResolvedProfile,
    impl_model: &str,
    attempt: i64,
    diff: &str,
    gate_output: &str,
    prior_reports: &[ReportBody],
    worktree_path: &Path,
    recipe: &ContainmentRecipe,
) -> Result<ReportBody, WorkerFailure> {
    let (verifier_role, independence) = select_verifier(rp, impl_model).ok_or_else(|| {
        fail(
            "model_unavailable",
            "no eligible verifier model configured (verification never skipped, ADR-002)",
        )
    })?;

    // Proxy routing (ADR-006): when the streaming credential proxy is enabled AND
    // the verifier model is non-mock, route the verifier's Anthropic calls through
    // the daemon-local proxy (`http://{addr}`) META-ONLY: the proxy injects the
    // key upstream and METERS the verifier's usage into the per-task ledger (so
    // the ledger reflects TOTAL task spend, making the implementer's pre-forward
    // gate accurate), but NEVER gates or hard-stops the verifier — it must always
    // run (ADR-002 "verification never skipped"). The `X-Maestro-Meter` header
    // (vs the implementer's gated `X-Maestro-Task`) carries the meter-only
    // semantics. The MOCK verifier is never routed (it makes no network call). On
    // the proxy-off path (`proxy_addr` is `None`) the verifier uses its role's
    // base_url unchanged and sends no header — the direct path is untouched.
    let verifier_is_mock = verifier_role_is_mock(&verifier_role);
    let (verifier_base_url, meter_header) = match state.proxy_addr.as_deref() {
        Some(addr) if !verifier_is_mock => {
            (Some(format!("http://{addr}")), Some(task_id.to_string()))
        }
        _ => (verifier_role.base_url.clone(), None),
    };
    let backend = select_verifier_backend(&verifier_role, verifier_base_url);

    // The verifier MAY run bounded commands in a THROWAWAY COPY of the
    // implementer's worktree, severed from the repo (ADR-002). Built here (lazily
    // copied on first use); dropped — and its tempdir removed — when this
    // function returns. The MockVerifier ignores it; the AnthropicVerifier drives
    // it via the `run_command` tool and the daemon's records populate
    // `commands_run`.
    let runner = ThrowawayCheckoutRunner::new(worktree_path, recipe.clone());

    // Up to two tries: a crash / invalid report retries once with a fresh
    // session (ADR-002).
    let mut last_err: Option<ImplementerError> = None;
    for try_idx in 0..2 {
        emit(state, task_id, EventKind::VerifyStarted, None);
        let session_id = {
            let journal = state.journal.lock().expect("journal mutex poisoned");
            journal
                .insert_session(
                    Some(task_id),
                    None,
                    Role::Verifier,
                    &verifier_role.model,
                    SessionKind::OneShotApi,
                    None,
                )
                .ok()
        };

        let vtask = VerifyTask {
            spec: spec.clone(),
            diff: diff.to_string(),
            gate_output: gate_output.to_string(),
            model: verifier_role.model.clone(),
            // The AnthropicVerifier was already built with the effective base_url
            // (proxy or role), so `base_url` here is redundant for it; keep the
            // role's value for provenance. `meter_header` carries the meter-only
            // routing tag when the proxy is enabled (non-mock).
            base_url: verifier_role.base_url.clone(),
            prior_reports: prior_reports.to_vec(),
            meter_header: meter_header.clone(),
        };

        match backend.verify(&vtask, &runner) {
            Ok(out) => {
                // Persist the report; finish the session with its tokens. The
                // verifier's tokens are its own — not the implementer's.
                if let Some(sid) = session_id.as_deref() {
                    let report_json = serde_json::to_string(&out.report).unwrap_or_default();
                    let journal = state.journal.lock().expect("journal mutex poisoned");
                    if let Err(e) = journal.insert_verifier_report(
                        task_id,
                        sid,
                        attempt,
                        independence,
                        &report_json,
                    ) {
                        tracing::warn!(task = task_id, error = %e, "insert_verifier_report failed");
                    }
                    let _ = journal.finish_session(
                        sid,
                        ExitStatus::Ok,
                        Some(out.turns as i64),
                        Some(out.tokens_in as i64),
                        Some(out.tokens_out as i64),
                    );
                }
                return Ok(out.report);
            }
            Err(ImplementerError::Unavailable(m)) => {
                finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
                // No eligible/usable verifier → model_unavailable (never retried
                // as a crash: unavailability is not a transient crash).
                return Err(fail("model_unavailable", m));
            }
            Err(other) => {
                // Crash / invalid report / budget exhaustion mid-report: retry
                // once with a fresh session (ADR-002).
                finish_session(state, session_id.as_deref(), ExitStatus::Error, None, None, None);
                tracing::warn!(task = task_id, try_idx, error = %other, "verifier failed; retrying");
                last_err = Some(other);
            }
        }
    }

    // Second failure → internal_error on the task (ADR-002).
    Err(fail(
        "internal_error",
        format!(
            "verifier failed twice: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".into())
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::spec::{AcceptanceCriterion, Budget};

    fn spec(criteria: Vec<AcceptanceCriterion>) -> TaskSpec {
        TaskSpec {
            title: "t".into(),
            tier: Tier::T0,
            base_ref: "HEAD".into(),
            file_allowlist: vec![],
            instructions: "{}".into(),
            acceptance_criteria: criteria,
            check_commands: vec![],
            house_rules_ref: None,
            budget: Budget::default(),
            lifetime_budget: Default::default(),
            containment_min: 0,
        }
    }

    fn crit(id: &str, check: &str, kind: CriterionKind) -> AcceptanceCriterion {
        AcceptanceCriterion {
            id: id.into(),
            check: check.into(),
            kind,
        }
    }

    // L6: build-only check_commands are flagged; anything that runs tests is not.
    #[test]
    fn build_only_check_commands_are_flagged() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        // No commands → nothing to warn about (empty gate is a separate concern).
        assert!(!check_commands_look_build_only(&[]));

        // Pure build/lint gates → build-only.
        assert!(check_commands_look_build_only(&s(&["cargo build"])));
        assert!(check_commands_look_build_only(&s(&["cargo build --release", "cargo clippy"])));
        assert!(check_commands_look_build_only(&s(&["nix build .#pkg"])));

        // Any test-running command suppresses the warning (case-insensitive).
        assert!(!check_commands_look_build_only(&s(&["cargo test"])));
        assert!(!check_commands_look_build_only(&s(&["cargo build", "cargo test --all"])));
        assert!(!check_commands_look_build_only(&s(&["cargo nextest run"])));
        assert!(!check_commands_look_build_only(&s(&["CARGO_TEST=1 make TEST"])));
        assert!(!check_commands_look_build_only(&s(&["pytest -q"])));
        assert!(!check_commands_look_build_only(&s(&["npm test"])));
        assert!(!check_commands_look_build_only(&s(&["make check"]))); // biased to not-warn
    }

    #[test]
    fn empty_criteria_rejected() {
        assert!(validate_spec(&spec(vec![])).is_err());
    }

    #[test]
    fn empty_check_rejected() {
        let s = spec(vec![crit("AC1", "   ", CriterionKind::Invariant)]);
        assert!(validate_spec(&s).is_err());
    }

    #[test]
    fn concrete_command_and_invariant_accepted() {
        let s = spec(vec![
            crit("AC1", "cargo test", CriterionKind::Command),
            crit("AC2", "file X exists", CriterionKind::Invariant),
        ]);
        assert!(validate_spec(&s).is_ok());
    }

    // AC4: the pure effective-resolution + downgrade logic. Requested L2 on caps
    // without nix → capped to L1 (bwrap present) with downgraded=true; the daemon
    // would record L1 and emit ContainmentDowngraded.
    #[test]
    fn recipe_resolve_downgrades_l2_without_nix() {
        use maestro_journal::config::ContainmentConfig;
        // Hand-built caps: bwrap present, NO nix.
        let mut caps = maestro_sandbox::probe();
        caps.nix_flakes = false;
        caps.bwrap = true;
        caps.container_runtime = None;
        caps.recompute_max_level();

        let cc = ContainmentConfig {
            backend: "bwrap".into(),
            network: "deny".into(),
            devshell_variant: None,
            podman_image: None,
        };
        let repo = std::path::Path::new("/repo");
        let recipe = ContainmentRecipe::resolve(2, &cc, &caps, repo);
        assert_eq!(recipe.requested, maestro_sandbox::Level::L2);
        assert_eq!(recipe.level, maestro_sandbox::Level::L1, "AC4: capped to L1 (no nix)");
        assert!(recipe.downgraded, "AC4: downgrade flag set");
        assert_eq!(recipe.backend, maestro_sandbox::Backend::Bwrap);

        // Payload shape: { requested, actual, backend }.
        let payload: serde_json::Value =
            serde_json::from_str(&recipe.downgrade_payload()).unwrap();
        assert_eq!(payload["requested"], 2);
        assert_eq!(payload["actual"], 1);
        assert_eq!(payload["backend"], "bwrap");
    }

    // AC4: requested L1 with NO usable backend → capped to L0, downgraded.
    #[test]
    fn recipe_resolve_l1_no_backend_to_l0() {
        use maestro_journal::config::ContainmentConfig;
        let mut caps = maestro_sandbox::probe();
        caps.nix_flakes = false;
        caps.bwrap = false;
        caps.seatbelt = false;
        caps.container_runtime = None;
        caps.recompute_max_level();

        let cc = ContainmentConfig {
            backend: "none".into(),
            network: "deny".into(),
            devshell_variant: None,
            podman_image: None,
        };
        let recipe = ContainmentRecipe::resolve(1, &cc, &caps, std::path::Path::new("/r"));
        assert_eq!(recipe.level, maestro_sandbox::Level::L0);
        assert!(recipe.downgraded);
    }

    // AC4: floor 0 → L0, never downgraded (the M1/M3 default path).
    #[test]
    fn recipe_resolve_floor_zero_is_l0_no_downgrade() {
        use maestro_journal::config::ContainmentConfig;
        let caps = maestro_sandbox::probe();
        let cc = ContainmentConfig::default();
        let recipe = ContainmentRecipe::resolve(0, &cc, &caps, std::path::Path::new("/r"));
        assert_eq!(recipe.level, maestro_sandbox::Level::L0);
        assert!(!recipe.downgraded);
    }

    // AC5 (driven): at effective L1 with Backend::Bwrap, the recipe's SandboxSpec
    // wraps the driven CLI so DrivenConfig.program == "bwrap".
    #[test]
    fn recipe_wraps_driven_cli_under_bwrap() {
        let recipe = ContainmentRecipe {
            level: maestro_sandbox::Level::L1,
            backend: maestro_sandbox::Backend::Bwrap,
            network: maestro_sandbox::NetworkPolicy::Deny,
            flake_dir: PathBuf::from("/repo"),
            devshell_variant: None,
            podman_image: None,
            requested: maestro_sandbox::Level::L1,
            downgraded: false,
        };
        let ws = PathBuf::from("/w");
        let spec = recipe.spec_for(&ws);
        let args = vec!["--print".to_string()];
        let wrapped = maestro_sandbox::wrap(&spec, "codex", &args).unwrap();
        assert_eq!(wrapped.program, "bwrap", "AC5: driven CLI wrapped under bwrap");
        let n = wrapped.args.len();
        assert_eq!(
            &wrapped.args[n - 2..],
            &["codex".to_string(), "--print".to_string()][..],
            "AC5: argv ends with the driven CLI + its args"
        );
    }

    #[test]
    fn containment_floor_takes_max_of_profile_and_spec() {
        let rp = ResolvedProfile::merge(&Default::default(), None);
        // Defaults have no per-tier containment_min → 0; spec raises to 2.
        assert_eq!(containment_floor(&rp, Tier::T0, 2), 2);
        assert_eq!(containment_floor(&rp, Tier::T0, 0), 0);
    }

    // AC6: backend selection by role kind (ADR-008).
    #[test]
    fn select_backend_by_kind() {
        // mock model / mock kind → Ok.
        assert!(select_backend("mock", None, None).is_ok());
        assert!(select_backend("anything", Some("mock"), None).is_ok());

        // anthropic (default / explicit) → Ok, WITHOUT needing an API key
        // (non-eager construction; we never call `.run()`).
        assert!(select_backend("claude-sonnet-4-6", None, None).is_ok());
        assert!(select_backend("claude-sonnet-4-6", Some("anthropic"), None).is_ok());
        // base_url override is accepted for anthropic.
        assert!(select_backend(
            "claude-sonnet-4-6",
            Some("anthropic"),
            Some("http://localhost:8080".into())
        )
        .is_ok());

        // Deferred / unknown kinds fail loud as model_unavailable.
        for kind in ["driven_cli", "openai_compat", "bogus"] {
            let err = select_backend("x", Some(kind), None)
                .err()
                .unwrap_or_else(|| panic!("kind {kind} must be unavailable"));
            assert_eq!(err.kind_str(), "model_unavailable", "kind {kind}");
        }
    }

    // ADR-007: ladder_tier_count counts configured tiers from `start` up to the
    // ladder top, skipping gaps, always ≥ 1.
    #[test]
    fn ladder_tier_count_counts_configured_tiers() {
        // tier0 + tier2 configured, gap at tier1.
        let toml = r#"
[defaults]
[profiles.p]
roles.tier0 = "mock"
roles.tier2 = "mock2"
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        let rp = ResolvedProfile::merge(&cfg.defaults, cfg.profiles.get("p"));
        // Start T0 → ladder is [T0, T2] → 2 (gap at T1 skipped).
        assert_eq!(ladder_tier_count(&rp, Tier::T0), 2);
        // Start T2 → ladder is just [T2] → 1.
        assert_eq!(ladder_tier_count(&rp, Tier::T2), 1);

        // Only tier0 configured → count 1 regardless of start.
        let toml_one = r#"
[defaults]
[profiles.p]
roles.tier0 = "mock"
"#;
        let cfg1 = Config::from_toml_str(toml_one).unwrap();
        let rp1 = ResolvedProfile::merge(&cfg1.defaults, cfg1.profiles.get("p"));
        assert_eq!(ladder_tier_count(&rp1, Tier::T0), 1);
    }

    // ADR-007: derive_token_ceiling precedence — explicit spec wins; else derive
    // from token_factor when a per-attempt token budget is set; else None.
    #[test]
    fn derive_token_ceiling_rules() {
        // A profile with tier0 + tier1 configured → 2-tier ladder from T0.
        let toml = r#"
[defaults]
[profiles.p]
roles.tier0 = "mock"
roles.tier1 = "mock1"
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        let rp = ResolvedProfile::merge(&cfg.defaults, cfg.profiles.get("p"));
        assert_eq!(ladder_tier_count(&rp, Tier::T0), 2);
        assert_eq!(rp.lifetime.token_factor, 1.0);

        let base = spec(vec![crit("AC1", "cargo test", CriterionKind::Command)]);

        // per_attempt = 1000, factor 1.0, 2-tier ladder → derived 2000.
        let mut s = base.clone();
        s.budget.tokens = Some(1000);
        assert_eq!(derive_token_ceiling(&s, &rp), Some(2000));

        // Explicit lifetime_budget.tokens wins even with a per-attempt budget set.
        let mut s2 = s.clone();
        s2.lifetime_budget.tokens = Some(500);
        assert_eq!(derive_token_ceiling(&s2, &rp), Some(500));

        // No per-attempt token budget → no ceiling.
        let s3 = base.clone();
        assert_eq!(derive_token_ceiling(&s3, &rp), None);

        // factor ≤ 0 → no derived ceiling (still None with no explicit spec value).
        let mut rp0 = rp.clone();
        rp0.lifetime.token_factor = 0.0;
        assert_eq!(derive_token_ceiling(&s, &rp0), None);

        // Clamp: derived below a single attempt is raised to per_attempt.
        // factor 0.4 × 1000 × 2 = 800 → still ≥ 1000? no: 800 < 1000 → clamp 1000.
        let mut rp_small = rp.clone();
        rp_small.lifetime.token_factor = 0.4;
        assert_eq!(derive_token_ceiling(&s, &rp_small), Some(1000));
    }

    // The role for a tier carries kind + base_url from the Detailed table.
    #[test]
    fn role_for_tier_carries_kind_and_base_url() {
        let toml = r#"
[defaults]
[profiles.p]
roles.tier0 = "mock"
roles.tier1 = { model = "qwen", kind = "openai_compat", base_url = "http://localhost:11434/v1" }
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        let rp = ResolvedProfile::merge(&cfg.defaults, cfg.profiles.get("p"));

        // Bare string → no kind/base_url.
        let t0 = role_for_tier(&rp, Tier::T0).unwrap();
        assert_eq!(t0.model, "mock");
        assert_eq!(t0.kind, None);
        assert_eq!(t0.base_url, None);

        // Detailed table → kind + base_url carried through.
        let t1 = role_for_tier(&rp, Tier::T1).unwrap();
        assert_eq!(t1.model, "qwen");
        assert_eq!(t1.kind.as_deref(), Some("openai_compat"));
        assert_eq!(t1.base_url.as_deref(), Some("http://localhost:11434/v1"));
    }

    /// A `ContainmentRecipe` fixture with the given `downgraded` flag. Only the
    /// `downgraded` field matters to `tighten_file_cap`.
    fn recipe_with_downgraded(downgraded: bool) -> ContainmentRecipe {
        ContainmentRecipe {
            level: maestro_sandbox::Level::L0,
            backend: maestro_sandbox::Backend::None,
            network: maestro_sandbox::NetworkPolicy::Deny,
            flake_dir: PathBuf::from("/repo"),
            devshell_variant: None,
            podman_image: None,
            requested: maestro_sandbox::Level::L0,
            downgraded,
        }
    }

    /// A `ResolvedRole` fixture with the given backend `kind`.
    fn role_with_kind(kind: Option<&str>) -> ResolvedRole {
        ResolvedRole {
            model: "mock".into(),
            kind: kind.map(str::to_string),
            base_url: None,
            command: None,
            args: None,
            adapter: None,
            max_budget_usd: None,
        }
    }

    /// A spec fixture carrying an `allowlist` of `n` distinct globs.
    fn spec_with_allowlist(n: usize) -> TaskSpec {
        let mut s = spec(vec![crit("AC1", "cargo test", CriterionKind::Command)]);
        s.file_allowlist = (0..n).map(|i| format!("src/f{i}.rs")).collect();
        s
    }

    // ADR-004 / ADR-007: tighten_file_cap — the enforced changed-file cap.
    #[test]
    fn tighten_file_cap_rules() {
        // Base profile with default tighten factors (allowlist_factor 0.5).
        let rp = ResolvedProfile::merge(&Default::default(), None);
        assert_eq!(rp.tighten.allowlist_factor, 0.5);
        assert!(!rp.codex_tighten);

        let anthropic = role_with_kind(None);
        let driven = role_with_kind(Some("driven_cli"));

        // Downgraded recipe + allowlist of 4 + factor 0.5 → Some(2).
        assert_eq!(
            tighten_file_cap(&rp, &recipe_with_downgraded(true), &anthropic, &spec_with_allowlist(4)),
            Some(2)
        );

        // codex_tighten + driven_cli role + allowlist 4 + factor 0.5 → Some(2).
        let mut rp_codex = rp.clone();
        rp_codex.codex_tighten = true;
        assert_eq!(
            tighten_file_cap(&rp_codex, &recipe_with_downgraded(false), &driven, &spec_with_allowlist(4)),
            Some(2)
        );

        // codex_tighten + NON-driven role → None (not active).
        assert_eq!(
            tighten_file_cap(&rp_codex, &recipe_with_downgraded(false), &anthropic, &spec_with_allowlist(4)),
            None
        );

        // Neither downgraded nor codex_tighten → None.
        assert_eq!(
            tighten_file_cap(&rp, &recipe_with_downgraded(false), &driven, &spec_with_allowlist(4)),
            None
        );

        // Active but empty allowlist → None (nothing to narrow).
        assert_eq!(
            tighten_file_cap(&rp, &recipe_with_downgraded(true), &anthropic, &spec_with_allowlist(0)),
            None
        );

        // factor 0.5 with allowlist len 1 → ceil(0.5)=1 (min-1 clamp holds).
        assert_eq!(
            tighten_file_cap(&rp, &recipe_with_downgraded(true), &anthropic, &spec_with_allowlist(1)),
            Some(1)
        );

        // factor 0.25 len 3 → ceil(0.75) = 1.
        let mut rp_quarter = rp.clone();
        rp_quarter.tighten.allowlist_factor = 0.25;
        assert_eq!(
            tighten_file_cap(&rp_quarter, &recipe_with_downgraded(true), &anthropic, &spec_with_allowlist(3)),
            Some(1)
        );
    }

    /// A spec fixture carrying an explicit per-attempt turn budget.
    fn spec_with_turns(turns: Option<i64>) -> TaskSpec {
        let mut s = spec(vec![crit("AC1", "cargo test", CriterionKind::Command)]);
        s.budget.turns = turns;
        s
    }

    // driven_env_remove: Some → empty (API-billed, keep keys); None → 4-key strip list.
    #[test]
    fn driven_env_remove_api_billed_vs_subscription() {
        // API-billed (max_budget_usd set): no keys stripped.
        let api_billed = driven_env_remove(Some(5.0));
        assert!(
            api_billed.is_empty(),
            "API-billed role must NOT strip provider keys; got: {api_billed:?}"
        );

        // Subscription (max_budget_usd not set): strip the standard 4 keys.
        let subscription = driven_env_remove(None);
        assert_eq!(
            subscription,
            vec![
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "OPENAI_API_KEY",
                "CODEX_API_KEY",
            ],
            "subscription role must strip provider API keys"
        );
    }

    // ADR-004 / ADR-007: effective_turn_cap — the enforced per-attempt turn cap
    // for the structured claude driven adapter.
    #[test]
    fn effective_turn_cap_rules() {
        // Default tighten factor is 0.6.
        let rp = ResolvedProfile::merge(&Default::default(), None);
        assert_eq!(rp.tighten.turn_factor, 0.6);
        assert!(!rp.codex_tighten);

        let anthropic = role_with_kind(None);
        let driven = role_with_kind(Some("driven_cli"));

        // NOT tightening → raw budget (30 → 30).
        assert_eq!(
            effective_turn_cap(&rp, &recipe_with_downgraded(false), &driven, &spec_with_turns(Some(30))),
            30
        );

        // Unset budget → DEFAULT_TURN_BUDGET (25), not tightened → 25.
        assert_eq!(
            effective_turn_cap(&rp, &recipe_with_downgraded(false), &anthropic, &spec_with_turns(None)),
            DEFAULT_TURN_BUDGET
        );

        // Downgraded → floor(30 × 0.6) = 18.
        assert_eq!(
            effective_turn_cap(&rp, &recipe_with_downgraded(true), &anthropic, &spec_with_turns(Some(30))),
            18
        );

        // codex_tighten + driven_cli role → tightened even without a downgrade:
        // floor(25 × 0.6) = 15.
        let mut rp_codex = rp.clone();
        rp_codex.codex_tighten = true;
        assert_eq!(
            effective_turn_cap(&rp_codex, &recipe_with_downgraded(false), &driven, &spec_with_turns(Some(25))),
            15
        );

        // codex_tighten + NON-driven role → NOT active → raw budget.
        assert_eq!(
            effective_turn_cap(&rp_codex, &recipe_with_downgraded(false), &anthropic, &spec_with_turns(Some(25))),
            25
        );

        // Min-1 clamp: tiny budget × factor floors to 0 → clamped to 1.
        assert_eq!(
            effective_turn_cap(&rp, &recipe_with_downgraded(true), &anthropic, &spec_with_turns(Some(1))),
            1
        );
    }

    // L15: the fix-in-place preamble names the failing command + its output and
    // instructs the worker to make the SMALLEST fix rather than rewrite. This is
    // the context injected into the next attempt after a `checks_failed`.
    #[test]
    fn check_fix_preamble_names_command_output_and_says_do_not_rewrite() {
        let cf = CheckFailure {
            command: "cargo build -p mycrate".into(),
            output_digest: "error[E0382]: use of moved value: `parser`".into(),
        };
        let p = check_fix_preamble(&cf);
        // The failing command and its output are both present verbatim.
        assert!(p.contains("cargo build -p mycrate"), "names the failing command");
        assert!(p.contains("use of moved value"), "includes the error output");
        // It steers toward a targeted fix, not a rewrite, and tells the worker its
        // edits are already present in the worktree.
        assert!(p.contains("do NOT rewrite") || p.contains("do NOT start over"));
        assert!(p.contains("ALREADY PRESENT"), "worker's edits are present");
        assert!(p.contains("SMALLEST"), "asks for the smallest targeted change");
    }

    #[test]
    fn driven_prompt_states_allowlist_is_a_hard_boundary() {
        let prompt = driven_prompt(&spec_with_allowlist(2));
        // The allowlist paths are listed.
        assert!(prompt.contains("src/f0.rs") && prompt.contains("src/f1.rs"));
        // The boundary + its consequence + the escape hatch are spelled out, so a
        // worker won't blindly split/restructure into out-of-allowlist files.
        assert!(prompt.contains("HARD BOUNDARY"));
        assert!(prompt.contains("WILL BE DROPPED"));
        assert!(prompt.contains("STOP and report"));
        // The plan-echo mechanic is preserved (do not break the plan extractor).
        assert!(prompt.contains("PLAN"));
    }
}
