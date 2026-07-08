//! Load a `credentials.toml` file into the process environment at daemon
//! startup (ADR-004 / ADR-007). The environment always wins: keys already set
//! to a non-empty value are never overridden. The file must be mode 0600 or
//! stricter; a looser permission is logged loudly and the file is NOT loaded.

use maestro_journal::config::Credentials;
use maestro_journal::paths;

/// Returns `true` if `mode` is group- or world-accessible (more open than
/// 0600). Used to gate credential file loading.
///
/// - 0o600 → `false` (owner-only rw, ok)
/// - 0o400 → `false` (owner-only r, ok)
/// - 0o640 → `true` (group-readable, too open)
/// - 0o644 → `true` (world-readable, too open)
pub fn perms_too_open(mode: u32) -> bool {
    mode & 0o077 != 0
}

/// Returns the `(key, value)` pairs from `creds` whose key is not currently
/// set to a non-empty value in the environment, as reported by `is_set`.
/// `is_set(key)` should return `true` iff the key is already set to a
/// non-empty value (i.e. should NOT be overridden).
///
/// This is a pure helper so the precedence rule can be unit-tested without
/// touching the real process environment.
pub fn keys_to_inject(
    creds: &Credentials,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(&str, &str)> {
    creds
        .env
        .iter()
        .filter(|(k, _)| !is_set(k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

/// Load `credentials.toml` into the process environment (ADR-007).
///
/// - If the file does not exist: returns quietly (normal case).
/// - If the file has group- or world-accessible permissions: logs a loud
///   warning and returns without setting anything (fail-loud; daemon keeps
///   serving).
/// - If the file has a parse error: logs a warning with path + error, returns
///   (daemon keeps serving).
/// - For each `(key, value)` in the `[env]` table: if the key is not already
///   set to a non-empty value, injects it; otherwise logs at `debug` that the
///   env value takes precedence. Logs the injection count at `info` (not the
///   values).
///
/// Call this ONCE at daemon startup, before the serve loop and before any
/// worker threads spawn, so `set_var` is safe (single-threaded at that point).
pub fn load_credentials_into_env() {
    use std::os::unix::fs::MetadataExt;

    let path = paths::credentials_path();

    // If the file does not exist, that is the normal case — no warning.
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "credentials.toml: could not read metadata; skipping"
            );
            return;
        }
    };

    // Check Unix permissions — must be 0600 or stricter.
    let mode = metadata.mode();
    if perms_too_open(mode) {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:#o}", mode & 0o777),
            "credentials.toml is group- or world-accessible; \
             refusing to load secrets. Run: chmod 600 {}",
            path.display(),
        );
        return;
    }

    // Read and parse.
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "credentials.toml: read failed; skipping"
            );
            return;
        }
    };

    let creds = match Credentials::from_toml_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "credentials.toml: parse failed; skipping"
            );
            return;
        }
    };

    // Determine which keys to inject (env takes precedence over file).
    let is_set = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);

    // Log keys that the env is already providing (do not override them).
    for k in creds.env.keys() {
        if is_set(k.as_str()) {
            tracing::debug!(key = %k, "credentials.toml: env value takes precedence; skipping");
        }
    }

    let to_inject = keys_to_inject(&creds, is_set);
    let injected = to_inject.len();

    // SAFETY: set_var is called at daemon startup, before any other threads
    // have been spawned, so there are no concurrent readers of the env.
    #[allow(deprecated)] // set_var is deprecated in Rust 1.81+; safe here (single-threaded startup)
    for (key, value) in to_inject {
        unsafe { std::env::set_var(key, value) };
    }

    if injected > 0 {
        tracing::info!(
            path = %path.display(),
            count = injected,
            "credentials.toml: injected {injected} environment variable(s)"
        );
    } else {
        tracing::debug!(
            path = %path.display(),
            "credentials.toml: loaded but no new variables to inject"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::config::Credentials;
    use std::collections::BTreeMap;

    // ---------------------------------------------------------------------------
    // perms_too_open
    // ---------------------------------------------------------------------------

    #[test]
    fn perms_600_is_ok() {
        assert!(!perms_too_open(0o600));
    }

    #[test]
    fn perms_400_is_ok() {
        assert!(!perms_too_open(0o400));
    }

    #[test]
    fn perms_640_is_too_open() {
        assert!(perms_too_open(0o640));
    }

    #[test]
    fn perms_644_is_too_open() {
        assert!(perms_too_open(0o644));
    }

    #[test]
    fn perms_660_is_too_open() {
        assert!(perms_too_open(0o660));
    }

    #[test]
    fn perms_666_is_too_open() {
        assert!(perms_too_open(0o666));
    }

    #[test]
    fn perms_604_is_too_open() {
        // World-readable — still too open.
        assert!(perms_too_open(0o604));
    }

    #[test]
    fn perms_700_is_ok() {
        // Execute-only bits are in the owner set, no group/world bits set.
        assert!(!perms_too_open(0o700));
    }

    // ---------------------------------------------------------------------------
    // keys_to_inject
    // ---------------------------------------------------------------------------

    fn make_creds(pairs: &[(&str, &str)]) -> Credentials {
        let mut env = BTreeMap::new();
        for (k, v) in pairs {
            env.insert(k.to_string(), v.to_string());
        }
        Credentials { env }
    }

    #[test]
    fn keys_to_inject_includes_unset_key() {
        let creds = make_creds(&[("UNSET_KEY_XYZ", "value123")]);
        // Simulate: key is not set.
        let result = keys_to_inject(&creds, |_k| false);
        assert_eq!(result, vec![("UNSET_KEY_XYZ", "value123")]);
    }

    #[test]
    fn keys_to_inject_skips_already_set_key() {
        let creds = make_creds(&[("ALREADY_SET_KEY", "from_file")]);
        // Simulate: key is already set to a non-empty value.
        let result = keys_to_inject(&creds, |_k| true);
        assert!(result.is_empty());
    }

    #[test]
    fn keys_to_inject_mixed() {
        let creds = make_creds(&[
            ("KEY_SET", "file_value"),
            ("KEY_UNSET", "inject_me"),
        ]);
        let result = keys_to_inject(&creds, |k| k == "KEY_SET");
        // Only KEY_UNSET should be returned.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], ("KEY_UNSET", "inject_me"));
    }

    #[test]
    fn keys_to_inject_empty_creds_yields_empty() {
        let creds = Credentials::default();
        let result = keys_to_inject(&creds, |_| false);
        assert!(result.is_empty());
    }

    #[test]
    fn keys_to_inject_all_set_yields_empty() {
        let creds = make_creds(&[("A", "1"), ("B", "2")]);
        let result = keys_to_inject(&creds, |_| true);
        assert!(result.is_empty());
    }
}
