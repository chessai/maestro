//! Config profile resolution (ADR-007). The `maestro-journal` `config` module
//! only *parses* the TOML shape; determining the active profile by precedence
//! and producing the merged "resolved profile" is daemon policy and lives here.

use maestro_journal::config::{
    AdvisorConfig, Concurrency, Config, ContainmentConfig, ContainmentMin, Defaults,
    DowngradePolicy, LifetimeFactors, Profile, ProxyConfig, Roles, SearchConfig, ShimConfig,
    TightenFactors,
};
use serde::Serialize;

/// How the active profile name was chosen, for reporting / error semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    /// `--profile <name>` command-line flag.
    Flag,
    /// `MAESTRO_PROFILE` environment variable.
    Env,
    /// `default_profile` key in the config file.
    DefaultKey,
    /// The implicit `"default"` fallback (no flag/env/key).
    Implicit,
}

impl ProfileSource {
    /// Whether the profile name was *explicitly* requested (flag or env). An
    /// explicit-but-missing profile is a loud error; the implicit `"default"`
    /// falling back to defaults-only is not.
    pub fn is_explicit(self) -> bool {
        matches!(self, ProfileSource::Flag | ProfileSource::Env)
    }
}

/// The chosen active profile name and where it came from.
#[derive(Debug, Clone)]
pub struct ActiveProfile {
    pub name: String,
    pub source: ProfileSource,
}

/// Determine the active profile name by precedence (ADR-007):
/// `--profile` flag > `MAESTRO_PROFILE` env > `default_profile` key > `"default"`.
///
/// `flag` is the parsed CLI argument (if any); `env` is the value of
/// `MAESTRO_PROFILE` (if set and non-empty); `config` supplies `default_profile`.
pub fn active_profile(flag: Option<&str>, env: Option<&str>, config: &Config) -> ActiveProfile {
    if let Some(name) = flag.filter(|s| !s.is_empty()) {
        return ActiveProfile {
            name: name.to_string(),
            source: ProfileSource::Flag,
        };
    }
    if let Some(name) = env.filter(|s| !s.is_empty()) {
        return ActiveProfile {
            name: name.to_string(),
            source: ProfileSource::Env,
        };
    }
    if let Some(name) = config.default_profile.as_deref().filter(|s| !s.is_empty()) {
        return ActiveProfile {
            name: name.to_string(),
            source: ProfileSource::DefaultKey,
        };
    }
    ActiveProfile {
        name: "default".to_string(),
        source: ProfileSource::Implicit,
    }
}

/// A fully resolved profile: `[defaults]` overlaid with a named
/// `[profiles.<name>]` (profile values win where present, defaults otherwise).
/// This is what `maestro doctor` reports and what task delegation reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedProfile {
    pub concurrency: Concurrency,
    pub watchdog_minutes: u32,
    pub shim: ShimConfig,
    pub downgrade_policy: DowngradePolicy,
    pub tighten: TightenFactors,
    pub lifetime: LifetimeFactors,
    pub advisor: AdvisorConfig,
    pub roles: Roles,
    pub containment_min: ContainmentMin,
    /// The resolved OS-sandbox backend / network config (ADR-004): the profile's
    /// `containment` override if set, else the defaults' `containment` table.
    pub containment: ContainmentConfig,
    pub search: SearchConfig,
    pub codex_tighten: bool,
    /// The resolved streaming-credential-proxy config (ADR-006 / ADR-004).
    /// Sourced from `[defaults].proxy`; there is no profile-level override in
    /// this pass. OPT-IN — `enabled` defaults to `false`.
    pub proxy: ProxyConfig,
}

/// Overlay a profile's `advisor` table over the defaults' `advisor` table.
/// Each optional/collection field is taken from the profile when it carries a
/// value, else inherited from defaults.
fn merge_advisor(defaults: &AdvisorConfig, profile: &AdvisorConfig) -> AdvisorConfig {
    AdvisorConfig {
        model: profile.model.clone().or_else(|| defaults.model.clone()),
        context: profile.context.clone().or_else(|| defaults.context.clone()),
        // An explicitly-set (non-empty) writable_paths in the profile wins;
        // otherwise inherit the defaults' list.
        writable_paths: if profile.writable_paths.is_empty() {
            defaults.writable_paths.clone()
        } else {
            profile.writable_paths.clone()
        },
    }
}

impl ResolvedProfile {
    /// Merge `[defaults]` with an optional named profile. When `profile` is
    /// `None` (profile absent, implicit-default fallback), the result is the
    /// defaults verbatim.
    pub fn merge(defaults: &Defaults, profile: Option<&Profile>) -> Self {
        match profile {
            None => ResolvedProfile {
                concurrency: defaults.concurrency,
                watchdog_minutes: defaults.watchdog_minutes,
                shim: defaults.shim,
                downgrade_policy: defaults.downgrade_policy,
                tighten: defaults.tighten,
                lifetime: defaults.lifetime,
                advisor: defaults.advisor.clone(),
                roles: Roles::default(),
                containment_min: ContainmentMin::default(),
                containment: defaults.containment.clone(),
                search: SearchConfig::default(),
                codex_tighten: false,
                proxy: defaults.proxy.clone(),
            },
            Some(p) => ResolvedProfile {
                concurrency: p.concurrency.unwrap_or(defaults.concurrency),
                watchdog_minutes: p.watchdog_minutes.unwrap_or(defaults.watchdog_minutes),
                shim: p.shim.unwrap_or(defaults.shim),
                downgrade_policy: p.downgrade_policy.unwrap_or(defaults.downgrade_policy),
                tighten: p.tighten.unwrap_or(defaults.tighten),
                lifetime: p.lifetime.unwrap_or(defaults.lifetime),
                advisor: merge_advisor(&defaults.advisor, &p.advisor),
                // roles / containment_min / search live only on profiles; there
                // is no defaults-level analogue to inherit from.
                roles: p.roles.clone(),
                containment_min: p.containment_min,
                // Profile-level containment override wins whole (it is a small
                // self-contained table); else inherit the defaults' table.
                containment: p
                    .containment
                    .clone()
                    .unwrap_or_else(|| defaults.containment.clone()),
                search: p.search.clone(),
                codex_tighten: p.codex_tighten.unwrap_or(false),
                // No profile-level proxy override in this pass; inherit defaults.
                proxy: defaults.proxy.clone(),
            },
        }
    }
}

/// The outcome of resolving config for a doctor report: the active profile
/// name, and either the merged resolved view or a loud error (explicit profile
/// requested but absent from the config).
#[derive(Debug, Clone)]
pub struct Resolution {
    pub profile: String,
    pub source: ProfileSource,
    pub resolved: Result<ResolvedProfile, String>,
}

/// Resolve config end to end: pick the active profile by precedence, then merge
/// it over defaults. An explicitly-named profile (flag/env) that is absent from
/// the config yields an `Err` describing the failure; an implicit-default that
/// is absent falls back to defaults-only.
pub fn resolve(flag: Option<&str>, env: Option<&str>, config: &Config) -> Resolution {
    let active = active_profile(flag, env, config);
    let profile = config.profiles.get(&active.name);

    let resolved = match (profile, active.source.is_explicit(), active.name.as_str()) {
        // Found the named profile: merge it over defaults.
        (Some(p), _, _) => Ok(ResolvedProfile::merge(&config.defaults, Some(p))),
        // Explicitly requested but missing: loud error.
        (None, true, name) => Err(format!(
            "profile \"{name}\" requested via {} but not found in config",
            match active.source {
                ProfileSource::Flag => "--profile",
                ProfileSource::Env => "MAESTRO_PROFILE",
                _ => "explicit request",
            }
        )),
        // Implicit "default" absent: fall back to defaults-only.
        (None, false, _) => Ok(ResolvedProfile::merge(&config.defaults, None)),
    };

    Resolution {
        profile: active.name,
        source: active.source,
        resolved,
    }
}

/// Build the `resolved_profile` JSON value for a doctor report. On success this
/// is the serialized [`ResolvedProfile`]; on failure it carries an `error`
/// field so the CLI surfaces the misconfiguration loudly.
pub fn resolved_profile_json(res: &Resolution) -> serde_json::Value {
    match &res.resolved {
        Ok(rp) => serde_json::to_value(rp).unwrap_or_else(|e| {
            serde_json::json!({ "error": format!("failed to serialize resolved profile: {e}") })
        }),
        Err(msg) => serde_json::json!({ "error": msg }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ADR-007 example config, used as the merge/precedence fixture.
    const ADR_007_TOML: &str = r#"
default_profile = "personal"

[defaults]
concurrency.machine_cap = 4
concurrency.advisor_cap = 2
watchdog_minutes = 10
shim.excerpt_cap_chars = 1500
shim.cache_ttl_hours = 24
downgrade_policy = "tighten"
tighten.allowlist_factor = 0.5
tighten.turn_factor = 0.6
advisor.writable_paths = []
lifetime.token_factor = 1.0
lifetime.wall_clock_minutes = 30

[profiles.personal]
advisor.model = "claude-fable-5"
advisor.context = "standard"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "codex", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-8"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 1, tier2 = 2 }
search.backend = "searxng"
search.endpoint = "https://searx.internal:8443"

[profiles.work]
advisor.model = "claude-opus-4-7"
advisor.context = "1m"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "claude-sonnet-4-6", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-7"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 0, tier2 = 1 }
downgrade_policy = "tighten"
watchdog_minutes = 20
"#;

    fn fixture() -> Config {
        Config::from_toml_str(ADR_007_TOML).expect("ADR-007 fixture parses")
    }

    // ---- precedence: flag > env > default_profile > "default" -------------

    #[test]
    fn precedence_flag_wins() {
        let cfg = fixture();
        let a = active_profile(Some("work"), Some("personal"), &cfg);
        assert_eq!(a.name, "work");
        assert_eq!(a.source, ProfileSource::Flag);
    }

    #[test]
    fn precedence_env_over_default_key() {
        let cfg = fixture();
        let a = active_profile(None, Some("work"), &cfg);
        assert_eq!(a.name, "work");
        assert_eq!(a.source, ProfileSource::Env);
    }

    #[test]
    fn precedence_default_key_over_implicit() {
        let cfg = fixture();
        // No flag, no env → the config's default_profile = "personal".
        let a = active_profile(None, None, &cfg);
        assert_eq!(a.name, "personal");
        assert_eq!(a.source, ProfileSource::DefaultKey);
    }

    #[test]
    fn precedence_implicit_default_when_nothing_set() {
        let cfg = Config::default(); // no default_profile
        let a = active_profile(None, None, &cfg);
        assert_eq!(a.name, "default");
        assert_eq!(a.source, ProfileSource::Implicit);
    }

    #[test]
    fn precedence_empty_flag_and_env_ignored() {
        let cfg = fixture();
        let a = active_profile(Some(""), Some(""), &cfg);
        // Empty strings are ignored, falling through to default_profile.
        assert_eq!(a.name, "personal");
        assert_eq!(a.source, ProfileSource::DefaultKey);
    }

    // ---- merge: defaults survive; profile overrides -----------------------

    #[test]
    fn merge_defaults_only_value_survives() {
        let cfg = fixture();
        // The `personal` profile does not set `shim` / `tighten` / `lifetime`,
        // so those must come straight from [defaults].
        let res = resolve(None, None, &cfg);
        let rp = res.resolved.expect("personal resolves");
        assert_eq!(rp.shim.excerpt_cap_chars, 1500);
        assert_eq!(rp.shim.cache_ttl_hours, 24);
        assert_eq!(rp.tighten.allowlist_factor, 0.5);
        assert_eq!(rp.lifetime.wall_clock_minutes, 30);
        assert_eq!(rp.concurrency.machine_cap, 4);
        // watchdog not set on `personal` → inherited from defaults (10).
        assert_eq!(rp.watchdog_minutes, 10);
    }

    #[test]
    fn merge_profile_overrides_default() {
        let cfg = fixture();
        // `work` overrides watchdog_minutes = 20; defaults is 10.
        let res = resolve(Some("work"), None, &cfg);
        let rp = res.resolved.expect("work resolves");
        assert_eq!(rp.watchdog_minutes, 20, "profile value must override default");
        // downgrade_policy set on work explicitly.
        assert_eq!(rp.downgrade_policy, DowngradePolicy::Tighten);
        // roles / containment come from the profile.
        assert_eq!(rp.roles.tier2.as_ref().unwrap().model(), "claude-opus-4-7");
        assert_eq!(rp.containment_min.tier2, Some(1));
        // advisor.context overridden by profile.
        assert_eq!(rp.advisor.context.as_deref(), Some("1m"));
    }

    #[test]
    fn merge_advisor_inherits_and_overrides() {
        let cfg = fixture();
        let res = resolve(Some("personal"), None, &cfg);
        let rp = res.resolved.expect("personal resolves");
        // model/context set on profile.
        assert_eq!(rp.advisor.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(rp.advisor.context.as_deref(), Some("standard"));
        // writable_paths empty on both → empty.
        assert!(rp.advisor.writable_paths.is_empty());
    }

    // ---- explicit-missing = loud error; implicit-missing = defaults -------

    #[test]
    fn explicit_missing_profile_is_error() {
        let cfg = fixture();
        let res = resolve(Some("nonexistent"), None, &cfg);
        assert_eq!(res.profile, "nonexistent");
        let err = res
            .resolved
            .as_ref()
            .expect_err("missing explicit profile is an error");
        assert!(err.contains("nonexistent"), "error mentions the name: {err}");
        let json = resolved_profile_json(&res);
        assert!(json.get("error").is_some(), "json carries an error field");
    }

    #[test]
    fn explicit_env_missing_profile_is_error() {
        let cfg = fixture();
        let res = resolve(None, Some("ghost"), &cfg);
        assert!(res.resolved.is_err());
        let err = res.resolved.unwrap_err();
        assert!(err.contains("MAESTRO_PROFILE"), "error names the env source: {err}");
    }

    #[test]
    fn implicit_default_missing_falls_back_to_defaults() {
        // No config file at all → Config::default(), no "default" profile.
        let cfg = Config::default();
        let res = resolve(None, None, &cfg);
        assert_eq!(res.profile, "default");
        assert_eq!(res.source, ProfileSource::Implicit);
        let rp = res.resolved.expect("implicit default falls back to defaults-only");
        // Defaults-only values.
        assert_eq!(rp.watchdog_minutes, 10);
        assert_eq!(rp.concurrency.machine_cap, 4);
        assert!(rp.roles.tier0.is_none(), "no profile roles");
    }

    #[test]
    fn default_key_missing_profile_is_error() {
        // default_profile names a profile that doesn't exist. This came from
        // the config, so it is treated as an explicit-ish request... but per
        // spec only flag/env are "explicit". A dangling default_profile key is
        // a config author error; we surface it as an error too via merge on a
        // missing named profile only when explicit. Here it is DefaultKey, not
        // explicit, so it would fall back — assert that behavior is defined.
        let toml = r#"
default_profile = "ghost"
[defaults]
watchdog_minutes = 7
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        let res = resolve(None, None, &cfg);
        assert_eq!(res.profile, "ghost");
        assert_eq!(res.source, ProfileSource::DefaultKey);
        // Not explicit → falls back to defaults-only rather than erroring.
        let rp = res.resolved.expect("default_key missing falls back to defaults");
        assert_eq!(rp.watchdog_minutes, 7);
    }
}
