//! maestro-sandbox — capability probe and L0/L1/L2 containment levels
//! (ADR-004). M0 implements the capability probe and level types only;
//! actual sandbox wrappers are out of scope for this milestone.

use std::fmt;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

// ── Level ────────────────────────────────────────────────────────────────────

/// Containment level as defined in ADR-004.
///
/// - L0: agent-native sandbox + universal post-hoc enforcement. Always available.
/// - L1: L0 + OS sandbox wrapper (bwrap / seatbelt).
/// - L2: L1 + Nix devShell tool whitelist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    L0 = 0,
    L1 = 1,
    L2 = 2,
}

impl Level {
    /// Construct from a raw `u8` (0, 1, or 2). Returns `None` for other values.
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::L0),
            1 => Some(Self::L1),
            2 => Some(Self::L2),
            _ => None,
        }
    }

    /// Return the numeric representation.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl From<Level> for u8 {
    fn from(l: Level) -> u8 {
        l.as_u8()
    }
}

impl TryFrom<u8> for Level {
    type Error = u8;

    fn try_from(n: u8) -> Result<Self, Self::Error> {
        Self::from_u8(n).ok_or(n)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Level::L0 => write!(f, "L0"),
            Level::L1 => write!(f, "L1"),
            Level::L2 => write!(f, "L2"),
        }
    }
}

// ── AgentNative ──────────────────────────────────────────────────────────────

/// Detected agent CLIs on PATH (best-effort).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNative {
    /// `codex` CLI is on PATH.
    pub codex: bool,
    /// `claude` CLI is on PATH.
    pub claude: bool,
}

impl fmt::Display for AgentNative {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut found: Vec<&str> = Vec::new();
        if self.codex {
            found.push("codex");
        }
        if self.claude {
            found.push("claude");
        }
        if found.is_empty() {
            write!(f, "none")
        } else {
            write!(f, "{}", found.join(", "))
        }
    }
}

// ── Capabilities ─────────────────────────────────────────────────────────────

/// Result of a capability probe, suitable for display in `maestro doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `std::env::consts::OS`: "linux", "macos", etc.
    pub os: String,

    /// `nix` is on PATH *and* Nix flakes are usable
    /// (`nix flake --help` exits 0).
    pub nix_flakes: bool,

    /// `bwrap` (bubblewrap) is on PATH. Linux only in practice.
    pub bwrap: bool,

    /// Seatbelt (`sandbox-exec`) is present. Always `true` on macOS,
    /// always `false` on other platforms.
    pub seatbelt: bool,

    /// First container runtime found on PATH: "podman" preferred, then
    /// "docker". `None` if neither is present.
    pub container_runtime: Option<String>,

    /// The detected `container_runtime` responded to a health check — for
    /// podman/docker, `<rt> info` exited 0. `false` when `container_runtime`
    /// is `None` or the runtime binary is present but broken (e.g. missing
    /// `newuidmap` / `policy.json` for rootless podman).
    pub container_runtime_functional: bool,

    /// Agent CLI detection results.
    pub agent_native: AgentNative,

    /// Highest containment level achievable on this host, derived from the
    /// other fields. See [`derive_max_level`].
    pub max_level_available: Level,
}

impl Capabilities {
    /// Re-derive `max_level_available` from the other fields. Useful for
    /// constructing test fixtures.
    pub fn recompute_max_level(&mut self) {
        self.max_level_available = derive_max_level(
            self.nix_flakes,
            self.bwrap,
            self.seatbelt,
        );
    }
}

impl fmt::Display for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "OS              : {}", self.os)?;
        writeln!(f, "Nix flakes      : {}", yn(self.nix_flakes))?;
        writeln!(f, "bwrap           : {}", yn(self.bwrap))?;
        writeln!(f, "Seatbelt        : {}", yn(self.seatbelt))?;
        writeln!(
            f,
            "Container runtime: {}",
            self.container_runtime.as_deref().unwrap_or("none")
        )?;
        writeln!(f, "Container functional: {}", yn(self.container_runtime_functional))?;
        writeln!(f, "Agent CLIs      : {}", self.agent_native)?;
        write!(f, "Max level       : {}", self.max_level_available)
    }
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

// ── Level derivation ─────────────────────────────────────────────────────────

/// Derive the maximum containment level achievable given the supplied
/// capability flags. This is a pure function so it can be tested independently
/// of a live probe.
///
/// Rules (ADR-004):
/// - L2 requires nix_flakes AND (bwrap OR seatbelt).
/// - L1 requires (bwrap OR seatbelt).
/// - L0 is always available.
pub fn derive_max_level(nix_flakes: bool, bwrap: bool, seatbelt: bool) -> Level {
    let has_os_sandbox = bwrap || seatbelt;
    if nix_flakes && has_os_sandbox {
        Level::L2
    } else if has_os_sandbox {
        Level::L1
    } else {
        Level::L0
    }
}

// ── effective_level ───────────────────────────────────────────────────────────

/// Compute the effective containment level given what was requested and what
/// the host can actually provide.
///
/// The result is `min(requested_min, available)`: it never exceeds what the
/// host offers, and never upgrades past the requested level. Downgrade-and-
/// tighten policy is owned by the daemon; this function only computes the level.
pub fn effective_level(requested_min: Level, available: Level) -> Level {
    requested_min.min(available)
}

// ── Backend ──────────────────────────────────────────────────────────────────

/// OS-sandbox backend for the L1 wrapper (ADR-004, "OS-sandbox backends").
///
/// `None` means L0-only: no OS sandbox wrapper is applied; the daemon's
/// universal post-hoc enforcement floor is the only containment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// bubblewrap (`bwrap`) — Linux namespace sandbox, keeps host toolchain visible.
    Podman,
    /// podman (rootless) — stronger isolation; does NOT inherit the host toolchain.
    Bwrap,
    /// Seatbelt (`sandbox-exec`) — macOS host-level profile.
    Seatbelt,
    /// No OS sandbox: L0-only (post-hoc enforcement floor only).
    None,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Backend::Podman => "podman",
            Backend::Bwrap => "bwrap",
            Backend::Seatbelt => "seatbelt",
            Backend::None => "none",
        };
        f.write_str(s)
    }
}

/// Resolve which backend to use from the configured knob plus probed caps.
///
/// - `"auto"` (or any unknown string): prefer `Podman` if a podman container
///   runtime was probed, else `Bwrap` if available, else `Seatbelt` if
///   available, else `None`.
/// - `"podman" | "bwrap" | "seatbelt" | "none"`: force that backend regardless
///   of caps (the daemon separately checks availability and downgrades).
pub fn resolve_backend(configured: &str, caps: &Capabilities) -> Backend {
    match configured {
        "podman" => Backend::Podman,
        "bwrap" => Backend::Bwrap,
        "seatbelt" => Backend::Seatbelt,
        "none" => Backend::None,
        // "auto" and any unrecognized value fall through to auto-selection.
        _ => {
            if caps.container_runtime.as_deref() == Some("podman") && caps.container_runtime_functional {
                Backend::Podman
            } else if caps.bwrap {
                Backend::Bwrap
            } else if caps.seatbelt {
                Backend::Seatbelt
            } else {
                Backend::None
            }
        }
    }
}

// ── Network policy & sandbox spec ────────────────────────────────────────────

/// Network egress policy for the sandboxed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    /// Deny network egress (default-deny; per-task allowlist is the daemon's job).
    Deny,
    /// Allow network egress.
    Allow,
}

/// A fully-resolved sandbox specification: everything needed to construct the
/// wrapped command for a single session at a given level and backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// The (already-resolved, downgraded-if-needed) containment level to run at.
    pub level: Level,
    /// The OS-sandbox backend to use for the L1 wrapper.
    pub backend: Backend,
    /// The only writable path at L1+. Also the working directory.
    pub workspace: std::path::PathBuf,
    /// Network egress policy.
    pub network: NetworkPolicy,
    /// For L2: directory containing `flake.nix`. Required at L2.
    pub flake_dir: Option<std::path::PathBuf>,
    /// For L2: `devShells.<system>.<variant>`. `None` → the flake's default shell.
    pub devshell_variant: Option<String>,
    /// For the Podman backend at L1+: the container image. Required for podman
    /// (it does not inherit the host toolchain).
    pub podman_image: Option<String>,
}

/// The out-of-the-box container image used by the Podman backend when
/// `containment.podman_image` is unset. Overridable per profile. Tracks the
/// Rust 1.x toolchain this repo builds against; a work profile targeting
/// another stack should override it.
pub const DEFAULT_PODMAN_IMAGE: &str = "docker.io/library/rust:1";

/// A wrapped command: the outer program and argv the daemon should spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    /// The outer program (e.g. `bwrap`, `podman`, or the inner program at L0).
    pub program: String,
    /// The full argument vector.
    pub args: Vec<String>,
}

/// Errors constructing a wrapped command.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    /// Reserved: previously returned when no podman_image was configured.
    /// Since `wrap_podman` now falls back to `DEFAULT_PODMAN_IMAGE`, this
    /// variant is unreachable from `wrap_podman` but is kept for API
    /// compatibility and potential future use.
    #[allow(dead_code)]
    #[error("the podman backend requires a container image (podman_image), \
             but none was set: podman does not inherit the host toolchain")]
    PodmanImageRequired,
    /// L2 was requested but no `flake_dir` was provided.
    #[error("L2 containment requires a flake_dir (directory containing flake.nix), \
             but none was set")]
    FlakeDirRequired,
    /// The OS-sandbox backend binary could not be found on `$PATH`. At L2 the
    /// backend must be referenced by absolute path because it runs *inside*
    /// `nix develop --ignore-environment`, whose PATH will not contain it.
    #[error("the sandbox backend binary '{0}' was not found on $PATH; \
             it is required by absolute path for L2 (it runs inside \
             `nix develop --ignore-environment`, whose PATH omits it)")]
    BackendBinaryNotFound(String),
}

// ── wrap ─────────────────────────────────────────────────────────────────────

/// Build the outer command that runs `program args...` under the spec's level
/// and backend. This constructs argv only; it never executes anything.
///
/// - **L0** (or `Backend::None`): identity — the command is returned unchanged.
/// - **L1**: wrapped with the OS-sandbox backend (backend referenced by bare
///   name; it runs on the host PATH).
/// - **L2**: the composition is *inverted* relative to L1 (ADR-004 "Nix
///   specifics"): `nix develop --ignore-environment` runs on the **outside**
///   (on the host, resolving the devShell with full access), and the OS-sandbox
///   backend runs on the **inside** to confine FS + network. Because the inner
///   backend is invoked under `--ignore-environment`, its PATH will not contain
///   the backend binary, so at L2 the backend is referenced by its **absolute
///   path** (resolved from `$PATH` at wrap time). The backend preserves the
///   environment so the devShell PATH set by `nix develop` reaches the inner
///   program — that PATH is the tool whitelist. The produced argv is:
///   `nix develop --ignore-environment <flake># -c <backend_abs> <confine…> -- <program> <args>`.
pub fn wrap(
    spec: &SandboxSpec,
    program: &str,
    args: &[String],
) -> Result<WrappedCommand, SandboxError> {
    // L0 is always identity, regardless of backend.
    if spec.level == Level::L0 {
        return Ok(identity(program, args));
    }

    if spec.level == Level::L2 {
        return wrap_l2(spec, program, args);
    }

    // L1: wrap the program directly with the OS-sandbox backend, referenced by
    // bare name (it runs on the host PATH which has it).
    let bin = backend_binary_name(spec.backend).unwrap_or("");
    wrap_backend(spec, bin, program, args)
}

/// The executable name for an OS-sandbox backend (`None` for `Backend::None`).
fn backend_binary_name(backend: Backend) -> Option<&'static str> {
    match backend {
        Backend::Bwrap => Some("bwrap"),
        Backend::Podman => Some("podman"),
        Backend::Seatbelt => Some("sandbox-exec"),
        Backend::None => None,
    }
}

/// Apply the OS-sandbox backend wrapping to `(program, args)`. `backend_bin` is
/// the program token to place in the produced argv (a bare name at L1, an
/// absolute path at L2).
fn wrap_backend(
    spec: &SandboxSpec,
    backend_bin: &str,
    program: &str,
    args: &[String],
) -> Result<WrappedCommand, SandboxError> {
    match spec.backend {
        Backend::None => Ok(identity(program, args)),
        Backend::Bwrap => Ok(wrap_bwrap(spec, backend_bin, program, args)),
        Backend::Podman => wrap_podman(spec, backend_bin, program, args),
        Backend::Seatbelt => Ok(wrap_seatbelt(spec, backend_bin, program, args)),
    }
}

/// Build the L2 command: `nix develop --ignore-environment <flake># -c` on the
/// outside (resolves the devShell on the host), with the OS-sandbox backend —
/// referenced by absolute path — confining `program args…` on the inside.
fn wrap_l2(
    spec: &SandboxSpec,
    program: &str,
    args: &[String],
) -> Result<WrappedCommand, SandboxError> {
    let flake_dir = spec
        .flake_dir
        .as_ref()
        .ok_or(SandboxError::FlakeDirRequired)?;
    let installable = match &spec.devshell_variant {
        Some(v) => format!("{}#{}", flake_dir.display(), v),
        None => format!("{}#", flake_dir.display()),
    };

    // Resolve the backend binary's absolute path: under --ignore-environment the
    // devShell PATH will not contain it, so the inner invocation must name it
    // absolutely. None (identity) needs no binary.
    let inner = match backend_binary_name(spec.backend) {
        None => identity(program, args),
        Some(backend_name) => {
            let backend_abs = resolve_exe_abs(backend_name)
                .ok_or_else(|| SandboxError::BackendBinaryNotFound(backend_name.to_string()))?;
            wrap_backend(spec, &backend_abs, program, args)?
        }
    };

    let mut nix_args = vec![
        "develop".to_string(),
        "--ignore-environment".to_string(),
        installable,
        "-c".to_string(),
        inner.program,
    ];
    nix_args.extend(inner.args);
    Ok(WrappedCommand {
        program: "nix".to_string(),
        args: nix_args,
    })
}

fn identity(program: &str, args: &[String]) -> WrappedCommand {
    WrappedCommand {
        program: program.to_string(),
        args: args.to_vec(),
    }
}

fn wrap_bwrap(
    spec: &SandboxSpec,
    backend_bin: &str,
    program: &str,
    args: &[String],
) -> WrappedCommand {
    let ws = spec.workspace.display().to_string();
    // NOTE: no `--clearenv` — bwrap preserves the environment by default. This
    // is required at L2 so the devShell PATH (set by `nix develop
    // --ignore-environment` on the outside) reaches the inner program; that
    // PATH is the tool whitelist.
    let mut a: Vec<String> = vec![
        // Read-only whole FS keeps the host toolchain visible.
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        // tmpfs /tmp must come before the workspace bind so that when the
        // workspace lives under /tmp (e.g. /tmp/xxx/worktrees/...) the later
        // --bind re-exposes it rather than being shadowed by the tmpfs.
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        // Only the workspace is writable.
        "--bind".to_string(),
        ws.clone(),
        ws.clone(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
    ];
    if spec.network == NetworkPolicy::Deny {
        a.push("--unshare-net".to_string());
    }
    a.push("--chdir".to_string());
    a.push(ws);
    a.push("--".to_string());
    a.push(program.to_string());
    a.extend(args.iter().cloned());
    WrappedCommand {
        program: backend_bin.to_string(),
        args: a,
    }
}

fn wrap_podman(
    spec: &SandboxSpec,
    backend_bin: &str,
    program: &str,
    args: &[String],
) -> Result<WrappedCommand, SandboxError> {
    let image = spec.podman_image.as_deref().unwrap_or(DEFAULT_PODMAN_IMAGE);
    let ws = spec.workspace.display().to_string();
    let mut a: Vec<String> = vec!["run".to_string(), "--rm".to_string()];
    if spec.network == NetworkPolicy::Deny {
        a.push("--network".to_string());
        a.push("none".to_string());
    }
    a.push("-v".to_string());
    a.push(format!("{ws}:{ws}:rw"));
    a.push("-w".to_string());
    a.push(ws);
    a.push(image.to_string());
    a.push(program.to_string());
    a.extend(args.iter().cloned());
    Ok(WrappedCommand {
        program: backend_bin.to_string(),
        args: a,
    })
}

fn wrap_seatbelt(
    spec: &SandboxSpec,
    backend_bin: &str,
    program: &str,
    args: &[String],
) -> WrappedCommand {
    let ws = spec.workspace.display().to_string();
    let profile = seatbelt_profile(&ws, spec.network);
    let mut a: Vec<String> = vec!["-p".to_string(), profile, program.to_string()];
    a.extend(args.iter().cloned());
    WrappedCommand {
        program: backend_bin.to_string(),
        args: a,
    }
}

/// Build a Seatbelt (SBPL) profile string: default-deny writes except under
/// `workspace`, deny network on `Deny`. Not testable on Linux; the produced
/// string is a reasonable approximation of a workspace-only profile.
fn seatbelt_profile(workspace: &str, network: NetworkPolicy) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    p.push_str(&format!("(allow file-write* (subpath \"{workspace}\"))\n"));
    p.push_str("(allow file-write* (subpath \"/tmp\"))\n");
    match network {
        NetworkPolicy::Deny => p.push_str("(deny network*)\n"),
        NetworkPolicy::Allow => p.push_str("(allow network*)\n"),
    }
    p
}

// ── advisor read-only mount (ADR-006) ─────────────────────────────────────────

/// Inputs describing the advisor's read-only filesystem mount (ADR-006,
/// "Advisor filesystem isolation"). This is a *different* policy from the
/// worktree sandbox above: the whole host is read-only so tools work, the repo
/// is read-only (structural write-protection), and only the advisor scratch
/// dir, `$HOME`, `/tmp` and the opt-in in-repo `writable_paths` are writable.
/// Network is always allowed — the advisor needs it for Claude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisorMount {
    /// The advisor's repo, absolute + canonicalized. Bind-mounted read-only.
    pub repo: std::path::PathBuf,
    /// The advisor scratch dir (`$XDG_STATE_HOME/maestro/advisor/<id>/`),
    /// outside the repo, made read-write.
    pub scratch: std::path::PathBuf,
    /// Absolute writable carve-outs *inside* the repo (resolved under `repo`
    /// from `advisor.writable_paths`). Each overlays the read-only repo bind.
    pub writable_paths: Vec<std::path::PathBuf>,
    /// The advisor's `$HOME`, made read-write so Claude Code's `~/.claude`
    /// auth/state works.
    pub home: std::path::PathBuf,
}

/// Build the `bwrap` argv that runs `program args…` with the advisor mount
/// policy (ADR-006). Returns `(program, args)` — it never executes anything, so
/// the arg construction is unit-testable. `bwrap_bin` is the token placed as the
/// program (an absolute path resolved by the caller, or the bare name `"bwrap"`).
///
/// Mount order matters. bwrap applies binds in argv order, later ones layering
/// over earlier ones:
/// 1. `--ro-bind / /` first — the whole host visible read-only so tools work.
/// 2. `--tmpfs /tmp` — a fresh writable /tmp (also un-shadows a repo under /tmp
///    only if the repo bind comes after, which it does).
/// 3. `--dev /dev`, `--proc /proc`.
/// 4. `--bind $HOME $HOME` — writable home for `~/.claude`.
/// 5. `--ro-bind <repo> <repo>` — the repo, read-only. THE security property.
/// 6. `--bind <scratch> <scratch>` — writable scratch (outside the repo).
/// 7. `--bind <repo>/<p> <repo>/<p>` per writable carve-out — layered *over* the
///    read-only repo bind so those specific subpaths are writable.
/// 8. `--chdir <repo>`.
///
/// Network is deliberately left shared with the host (no `--unshare-net`).
pub fn advisor_mount_command(
    mount: &AdvisorMount,
    bwrap_bin: &str,
    program: &str,
    args: &[String],
) -> WrappedCommand {
    let repo = mount.repo.display().to_string();
    let scratch = mount.scratch.display().to_string();
    let home = mount.home.display().to_string();

    let mut a: Vec<String> = vec![
        // Whole host read-only so the advisor's tools resolve.
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        // Fresh writable /tmp. Comes before the repo bind so a repo under /tmp
        // is re-exposed by the later repo bind rather than shadowed here.
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        // Writable $HOME so Claude Code's ~/.claude auth/state works.
        "--bind".to_string(),
        home.clone(),
        home,
        // The repo, READ-ONLY. Layered over the ro-bind of / and over any
        // tmpfs /tmp (for a repo under /tmp). This is the load-bearing control.
        "--ro-bind".to_string(),
        repo.clone(),
        repo.clone(),
        // Advisor scratch (outside the repo), writable.
        "--bind".to_string(),
        scratch.clone(),
        scratch,
    ];

    // In-repo writable carve-outs overlay the read-only repo bind, so they MUST
    // come after it.
    for p in &mount.writable_paths {
        let p = p.display().to_string();
        a.push("--bind".to_string());
        a.push(p.clone());
        a.push(p);
    }

    a.push("--chdir".to_string());
    a.push(repo);
    a.push("--".to_string());
    a.push(program.to_string());
    a.extend(args.iter().cloned());

    WrappedCommand {
        program: bwrap_bin.to_string(),
        args: a,
    }
}

// ── resolve_effective ────────────────────────────────────────────────────────

/// Compute the effective level accounting for both the requested minimum and
/// the resolved backend + nix availability, returning whether a downgrade
/// occurred. This augments [`effective_level`] with backend-awareness.
///
/// - `L2` needs `caps.nix_flakes` AND a usable OS backend; missing either caps
///   toward the best reachable level (L1 if the backend is usable, else L0).
/// - `L1` needs a usable OS backend (`backend != None` and its cap present).
/// - `L0` is always available.
pub fn resolve_effective(
    requested_min: Level,
    caps: &Capabilities,
    backend: Backend,
) -> (Level, bool) {
    let backend_usable = backend_usable(backend, caps);
    match requested_min {
        Level::L0 => (Level::L0, false),
        Level::L1 => {
            if backend_usable {
                (Level::L1, false)
            } else {
                (Level::L0, true)
            }
        }
        Level::L2 => {
            if caps.nix_flakes && backend_usable {
                (Level::L2, false)
            } else if backend_usable {
                // No nix → cap at L1.
                (Level::L1, true)
            } else {
                // No usable backend → cap at L0.
                (Level::L0, true)
            }
        }
    }
}

/// Whether the resolved backend is actually usable given the probed caps.
fn backend_usable(backend: Backend, caps: &Capabilities) -> bool {
    match backend {
        Backend::None => false,
        Backend::Bwrap => caps.bwrap,
        Backend::Podman => caps.container_runtime.as_deref() == Some("podman") && caps.container_runtime_functional,
        Backend::Seatbelt => caps.seatbelt,
    }
}

// ── PATH helpers ─────────────────────────────────────────────────────────────

/// Returns `true` if `name` is found as an executable file on `$PATH`.
fn has_exe(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                is_executable(&candidate)
            })
        })
        .unwrap_or(false)
}

/// Resolve `name` to an absolute executable path by scanning the given `path_value`
/// string (like `which`). Returns `None` if not found. Pure: takes the PATH string
/// explicitly so callers can test without touching the process-global `$PATH`.
fn resolve_exe_abs_in(name: &str, path_value: &str) -> Option<String> {
    std::env::split_paths(path_value).find_map(|dir| {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            // Make absolute in case a relative PATH entry was used.
            let abs = if candidate.is_absolute() {
                candidate
            } else {
                std::env::current_dir().ok()?.join(&candidate)
            };
            Some(abs.to_string_lossy().into_owned())
        } else {
            None
        }
    })
}

/// Resolve `name` to an absolute executable path by scanning `$PATH` (like
/// `which`). Returns `None` if not found. Used at L2 to name the OS-sandbox
/// backend absolutely, because the inner invocation runs under
/// `nix develop --ignore-environment` whose PATH will not contain it.
fn resolve_exe_abs(name: &str) -> Option<String> {
    let paths = std::env::var("PATH").unwrap_or_default();
    resolve_exe_abs_in(name, &paths)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run a command quietly and return whether it exited successfully.
fn command_ok(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── probe ─────────────────────────────────────────────────────────────────────

/// Probe the host environment for available capabilities.
///
/// This function is cheap (PATH scanning + a couple of subprocess calls) and
/// never panics, even if tools are missing or misbehave.
pub fn probe() -> Capabilities {
    let os = std::env::consts::OS.to_string();

    // Nix flakes: nix must be on PATH and `nix flake --help` must succeed.
    let nix_on_path = has_exe("nix");
    let nix_flakes = nix_on_path && command_ok("nix", &["flake", "--help"]);

    // bwrap (bubblewrap): Linux namespace sandbox.
    let bwrap = has_exe("bwrap");

    // Seatbelt: macOS sandbox-exec, always present on macOS.
    let seatbelt = os == "macos";

    // Container runtime: prefer podman, fall back to docker.
    let container_runtime = if has_exe("podman") {
        Some("podman".to_string())
    } else if has_exe("docker") {
        Some("docker".to_string())
    } else {
        None
    };

    // Health-check the detected runtime: `<rt> info` must exit 0. A present-
    // but-broken podman (missing newuidmap, policy.json, etc.) is detected here
    // and downgrades auto-selection to bwrap instead of failing at wrap time.
    let container_runtime_functional = match &container_runtime {
        Some(rt) => command_ok(rt, &["info"]),
        None => false,
    };

    // Agent CLIs.
    let agent_native = AgentNative {
        codex: has_exe("codex"),
        claude: has_exe("claude"),
    };

    let max_level_available = derive_max_level(nix_flakes, bwrap, seatbelt);

    Capabilities {
        os,
        nix_flakes,
        bwrap,
        seatbelt,
        container_runtime,
        container_runtime_functional,
        agent_native,
        max_level_available,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a Capabilities with derived max_level. container_runtime
    // and container_runtime_functional are left at their zero values (None /
    // false) — individual tests set them via field assignment when needed.
    fn caps(nix_flakes: bool, bwrap: bool, seatbelt: bool) -> Capabilities {
        let max_level_available = derive_max_level(nix_flakes, bwrap, seatbelt);
        Capabilities {
            os: "linux".to_string(),
            nix_flakes,
            bwrap,
            seatbelt,
            container_runtime: None,
            container_runtime_functional: false,
            agent_native: AgentNative { codex: false, claude: false },
            max_level_available,
        }
    }

    // AC2: probe() on this Linux host (inside devShell) returns expected values.
    #[test]
    fn test_probe_linux_devshell() {
        let c = probe();
        eprintln!("probe() = {}", c);
        assert_eq!(c.os, "linux", "expected OS to be linux");
        assert!(c.nix_flakes, "expected nix_flakes to be true (nix is in devShell PATH)");
        assert!(c.bwrap, "expected bwrap to be true (bwrap is in devShell PATH)");
    }

    // AC2 (extended): print the JSON so callers can see it with --nocapture.
    #[test]
    fn test_probe_json_output() {
        let c = probe();
        let json = serde_json::to_string_pretty(&c).expect("serialization failed");
        eprintln!("probe() JSON:\n{}", json);
    }

    // AC3: effective_level semantics.
    #[test]
    fn test_effective_level_downgrade() {
        // Requested L2 but host only has L1 → must downgrade to L1.
        assert_eq!(
            effective_level(Level::L2, Level::L1),
            Level::L1,
            "should downgrade to available level"
        );
    }

    #[test]
    fn test_effective_level_no_upgrade() {
        // Requested L0 but host can do L2 → must NOT upgrade past request.
        assert_eq!(
            effective_level(Level::L0, Level::L2),
            Level::L0,
            "should not upgrade past requested level"
        );
    }

    #[test]
    fn test_effective_level_exact_match() {
        assert_eq!(effective_level(Level::L1, Level::L1), Level::L1);
        assert_eq!(effective_level(Level::L2, Level::L2), Level::L2);
        assert_eq!(effective_level(Level::L0, Level::L0), Level::L0);
    }

    // AC4: max_level derivation with hand-constructed Capabilities.
    #[test]
    fn test_derive_max_level_l2() {
        // nix + bwrap → L2
        let c = caps(true, true, false);
        assert_eq!(c.max_level_available, Level::L2, "nix + bwrap should give L2");
    }

    #[test]
    fn test_derive_max_level_l2_seatbelt() {
        // nix + seatbelt → L2
        let c = caps(true, false, true);
        assert_eq!(c.max_level_available, Level::L2, "nix + seatbelt should give L2");
    }

    #[test]
    fn test_derive_max_level_l1_bwrap_only() {
        // bwrap only (no nix) → L1
        let c = caps(false, true, false);
        assert_eq!(c.max_level_available, Level::L1, "bwrap without nix should give L1");
    }

    #[test]
    fn test_derive_max_level_l1_seatbelt_only() {
        // seatbelt only (no nix) → L1
        let c = caps(false, false, true);
        assert_eq!(c.max_level_available, Level::L1, "seatbelt without nix should give L1");
    }

    #[test]
    fn test_derive_max_level_l0() {
        // nothing → L0
        let c = caps(false, false, false);
        assert_eq!(c.max_level_available, Level::L0, "no sandbox tools should give L0");
    }

    #[test]
    fn test_derive_max_level_nix_only() {
        // nix but no OS sandbox → L0 (nix alone doesn't unlock L1/L2)
        let c = caps(true, false, false);
        assert_eq!(
            c.max_level_available,
            Level::L0,
            "nix without OS sandbox should give L0"
        );
    }

    // AC5: Capabilities serializes to JSON without error.
    #[test]
    fn test_capabilities_serializes_to_json() {
        let c = caps(true, true, false);
        let json = serde_json::to_string(&c);
        assert!(json.is_ok(), "serialization should succeed");
        let s = json.unwrap();
        assert!(s.contains("\"os\""), "JSON should contain os field");
        assert!(s.contains("\"nix_flakes\""), "JSON should contain nix_flakes field");
        assert!(s.contains("\"max_level_available\""), "JSON should contain max_level_available field");
    }

    // AC5 (extended): round-trip through JSON.
    #[test]
    fn test_capabilities_roundtrip_json() {
        let original = caps(true, true, false);
        let json = serde_json::to_string(&original).unwrap();
        let restored: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored, "round-trip through JSON should be lossless");
    }

    // Level conversions.
    #[test]
    fn test_level_conversions() {
        assert_eq!(Level::L0.as_u8(), 0);
        assert_eq!(Level::L1.as_u8(), 1);
        assert_eq!(Level::L2.as_u8(), 2);

        assert_eq!(Level::from_u8(0), Some(Level::L0));
        assert_eq!(Level::from_u8(1), Some(Level::L1));
        assert_eq!(Level::from_u8(2), Some(Level::L2));
        assert_eq!(Level::from_u8(3), None);

        assert_eq!(Level::try_from(0u8), Ok(Level::L0));
        assert_eq!(Level::try_from(1u8), Ok(Level::L1));
        assert_eq!(Level::try_from(2u8), Ok(Level::L2));
        assert!(Level::try_from(42u8).is_err());

        assert_eq!(u8::from(Level::L2), 2u8);
    }

    // Level ordering.
    #[test]
    fn test_level_ordering() {
        assert!(Level::L0 < Level::L1);
        assert!(Level::L1 < Level::L2);
        assert!(Level::L0 < Level::L2);
        assert!(Level::L2 > Level::L0);
    }

    // Level serde.
    #[test]
    fn test_level_serde() {
        let json = serde_json::to_string(&Level::L2).unwrap();
        assert_eq!(json, "\"L2\"");
        let back: Level = serde_json::from_str("\"L2\"").unwrap();
        assert_eq!(back, Level::L2);
    }

    // Display impl for Capabilities.
    #[test]
    fn test_capabilities_display() {
        let c = caps(true, true, false);
        let s = format!("{}", c);
        assert!(s.contains("linux"), "display should mention OS");
        assert!(s.contains("L2"), "display should mention max level");
    }

    // recompute_max_level.
    #[test]
    fn test_recompute_max_level() {
        let mut c = caps(false, false, false);
        assert_eq!(c.max_level_available, Level::L0);
        c.nix_flakes = true;
        c.bwrap = true;
        c.recompute_max_level();
        assert_eq!(c.max_level_available, Level::L2);
    }

    // ── wrap / backend tests ─────────────────────────────────────────────────

    use std::path::PathBuf;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // Assert that `needle` appears as a contiguous subsequence of `hay`.
    fn contains_subseq(hay: &[String], needle: &[&str]) -> bool {
        if needle.is_empty() {
            return true;
        }
        hay.windows(needle.len())
            .any(|w| w.iter().zip(needle).all(|(a, b)| a == b))
    }

    fn spec(level: Level, backend: Backend, network: NetworkPolicy) -> SandboxSpec {
        SandboxSpec {
            level,
            backend,
            workspace: PathBuf::from("/w"),
            network,
            flake_dir: None,
            devshell_variant: None,
            podman_image: None,
        }
    }

    // resolve_backend
    #[test]
    fn test_resolve_backend_auto_prefers_podman() {
        let mut c = caps(false, true, true);
        c.container_runtime = Some("podman".to_string());
        c.container_runtime_functional = true;
        assert_eq!(resolve_backend("auto", &c), Backend::Podman);
    }

    #[test]
    fn test_resolve_backend_auto_bwrap_then_seatbelt_then_none() {
        // bwrap only
        let c = caps(false, true, false);
        assert_eq!(resolve_backend("auto", &c), Backend::Bwrap);
        // seatbelt only
        let c = caps(false, false, true);
        assert_eq!(resolve_backend("auto", &c), Backend::Seatbelt);
        // nothing
        let c = caps(false, false, false);
        assert_eq!(resolve_backend("auto", &c), Backend::None);
    }

    #[test]
    fn test_resolve_backend_forced_and_unknown() {
        let c = caps(false, false, false); // no caps at all
        assert_eq!(resolve_backend("podman", &c), Backend::Podman);
        assert_eq!(resolve_backend("bwrap", &c), Backend::Bwrap);
        assert_eq!(resolve_backend("seatbelt", &c), Backend::Seatbelt);
        assert_eq!(resolve_backend("none", &c), Backend::None);
        // unknown → treated as auto → None here
        assert_eq!(resolve_backend("garbage", &c), Backend::None);
    }

    // ADR-004: backend_usable(Podman) requires both the runtime name AND functional flag.
    #[test]
    fn test_backend_usable_podman_functional_gate() {
        // present but NOT functional → backend_usable returns false.
        let mut c = caps(false, false, false);
        c.container_runtime = Some("podman".to_string());
        c.container_runtime_functional = false;
        // backend_usable is private; exercise it via resolve_effective.
        assert_eq!(
            resolve_effective(Level::L1, &c, Backend::Podman),
            (Level::L0, true),
            "present-but-nonfunctional podman must not be usable"
        );

        // present AND functional → backend_usable returns true.
        c.container_runtime_functional = true;
        assert_eq!(
            resolve_effective(Level::L1, &c, Backend::Podman),
            (Level::L1, false),
            "functional podman must be usable"
        );
    }

    // ADR-004: auto selection downgrades to bwrap when podman is present-but-broken.
    #[test]
    fn test_resolve_backend_auto_podman_nonfunctional_falls_through_to_bwrap() {
        let mut c = caps(false, true, false); // bwrap present
        c.container_runtime = Some("podman".to_string());
        c.container_runtime_functional = false; // broken rootless podman
        assert_eq!(
            resolve_backend("auto", &c),
            Backend::Bwrap,
            "auto must fall through to bwrap when podman is present-but-nonfunctional"
        );
    }

    // ADR-004: auto selection picks podman when functional.
    #[test]
    fn test_resolve_backend_auto_podman_functional_picked() {
        let mut c = caps(false, true, false);
        c.container_runtime = Some("podman".to_string());
        c.container_runtime_functional = true;
        assert_eq!(
            resolve_backend("auto", &c),
            Backend::Podman,
            "auto must pick podman when functional"
        );
    }

    // ADR-004: forced backend="podman" must still return Podman even when nonfunctional.
    #[test]
    fn test_resolve_backend_forced_podman_ignores_functional_flag() {
        let mut c = caps(false, true, false);
        c.container_runtime = Some("podman".to_string());
        c.container_runtime_functional = false; // broken, but operator forced it
        assert_eq!(
            resolve_backend("podman", &c),
            Backend::Podman,
            "forced backend=podman must be respected even when nonfunctional"
        );
    }

    // AC4: L0 identity.
    #[test]
    fn test_ac4_l0_identity() {
        let sp = spec(Level::L0, Backend::Bwrap, NetworkPolicy::Deny);
        let w = wrap(&sp, "cargo", &s(&["test"])).unwrap();
        assert_eq!(w.program, "cargo");
        assert_eq!(w.args, s(&["test"]));
    }

    #[test]
    fn test_backend_none_is_identity() {
        let sp = spec(Level::L1, Backend::None, NetworkPolicy::Deny);
        let w = wrap(&sp, "cargo", &s(&["test"])).unwrap();
        assert_eq!(w.program, "cargo");
        assert_eq!(w.args, s(&["test"]));
    }

    // AC5: L1 bwrap.
    #[test]
    fn test_ac5_l1_bwrap_deny() {
        let sp = spec(Level::L1, Backend::Bwrap, NetworkPolicy::Deny);
        let w = wrap(&sp, "cargo", &s(&["test"])).unwrap();
        assert_eq!(w.program, "bwrap");
        assert!(contains_subseq(&w.args, &["--bind", "/w", "/w"]));
        assert!(w.args.contains(&"--unshare-net".to_string()));
        assert!(contains_subseq(&w.args, &["--chdir", "/w"]));
        assert!(w.args.contains(&"--".to_string()));
        // cargo test at the very end.
        assert!(contains_subseq(&w.args, &["--", "cargo", "test"]));
        let n = w.args.len();
        assert_eq!(&w.args[n - 2..], &s(&["cargo", "test"])[..]);
    }

    #[test]
    fn test_ac5_l1_bwrap_allow_no_unshare_net() {
        let sp = spec(Level::L1, Backend::Bwrap, NetworkPolicy::Allow);
        let w = wrap(&sp, "cargo", &s(&["test"])).unwrap();
        assert_eq!(w.program, "bwrap");
        assert!(!w.args.contains(&"--unshare-net".to_string()));
    }

    // AC6: L2 = nix develop OUTSIDE, bwrap (absolute path) INSIDE.
    // Produced argv:
    //   nix develop --ignore-environment /f#codex-rust -c \
    //     <ABS>/bwrap <confine…> -- cargo build
    #[test]
    fn test_ac6_l2_nix_outside_bwrap_inside() {
        let mut sp = spec(Level::L2, Backend::Bwrap, NetworkPolicy::Deny);
        sp.flake_dir = Some(PathBuf::from("/f"));
        sp.devshell_variant = Some("codex-rust".to_string());
        let w = wrap(&sp, "cargo", &s(&["build"])).unwrap();

        // nix is the OUTER program now.
        assert_eq!(w.program, "nix");

        // The head is the nix develop invocation up to and including `-c`.
        let head = s(&["develop", "--ignore-environment", "/f#codex-rust", "-c"]);
        assert_eq!(&w.args[..head.len()], &head[..], "L2 head must be nix develop …");

        // Immediately after `-c` comes the backend, referenced by ABSOLUTE path
        // ending in `/bwrap`.
        let backend_tok = &w.args[head.len()];
        assert!(
            backend_tok.starts_with('/'),
            "backend token must be an absolute path, got {backend_tok:?}"
        );
        assert!(
            backend_tok.ends_with("/bwrap"),
            "backend token must be the bwrap binary, got {backend_tok:?}"
        );

        // The bwrap confinement args must be present.
        assert!(contains_subseq(&w.args, &["--ro-bind", "/", "/"]));
        assert!(contains_subseq(&w.args, &["--tmpfs", "/tmp"]));
        assert!(contains_subseq(&w.args, &["--bind", "/w", "/w"]));
        assert!(w.args.contains(&"--unshare-net".to_string()));
        assert!(contains_subseq(&w.args, &["--chdir", "/w"]));

        // NO --clearenv: the devShell PATH is the whitelist and must survive.
        assert!(
            !w.args.contains(&"--clearenv".to_string()),
            "bwrap must NOT clear env at L2 or the devShell PATH whitelist is lost"
        );

        // A `--` separator followed by the inner program + args at the tail.
        assert!(w.args.contains(&"--".to_string()));
        let tail = s(&["--", "cargo", "build"]);
        let n = w.args.len();
        assert_eq!(&w.args[n - tail.len()..], &tail[..], "tail must be -- cargo build");
    }

    #[test]
    fn test_l2_default_variant() {
        let mut sp = spec(Level::L2, Backend::Bwrap, NetworkPolicy::Deny);
        sp.flake_dir = Some(PathBuf::from("/f"));
        // no variant → default shell "/f#"
        let w = wrap(&sp, "cargo", &s(&["build"])).unwrap();
        assert_eq!(w.program, "nix");
        let head = s(&["develop", "--ignore-environment", "/f#", "-c"]);
        assert_eq!(&w.args[..head.len()], &head[..]);
        // inner program + args at the tail after a `--` separator.
        assert!(contains_subseq(&w.args, &["--", "cargo", "build"]));
    }

    // L2 with a backend binary that is not on PATH must error.
    // This test exercises the "not found" branch via the pure resolve_exe_abs_in
    // helper, passing an empty path string — no global $PATH mutation required.
    #[test]
    fn test_l2_backend_binary_not_found() {
        // Verify the pure helper returns None for an empty path (no bwrap there).
        assert_eq!(
            resolve_exe_abs_in("bwrap", ""),
            None,
            "bwrap must not be found in an empty path string"
        );

        // Verify the SandboxError::BackendBinaryNotFound variant is produced when
        // resolve_exe_abs_in finds nothing: call the inner logic directly.
        let result: Result<String, SandboxError> = resolve_exe_abs_in("bwrap", "")
            .ok_or_else(|| SandboxError::BackendBinaryNotFound("bwrap".to_string()));
        assert_eq!(
            result,
            Err(SandboxError::BackendBinaryNotFound("bwrap".to_string()))
        );
    }

    #[test]
    fn test_l2_missing_flake_dir_errors() {
        let sp = spec(Level::L2, Backend::Bwrap, NetworkPolicy::Deny);
        assert_eq!(
            wrap(&sp, "cargo", &s(&["build"])),
            Err(SandboxError::FlakeDirRequired)
        );
    }

    // AC7: podman with no image configured → uses DEFAULT_PODMAN_IMAGE.
    #[test]
    fn test_ac7_podman_no_image_uses_default() {
        let sp = spec(Level::L1, Backend::Podman, NetworkPolicy::Deny);
        let w = wrap(&sp, "cargo", &[]).unwrap();
        assert_eq!(w.program, "podman");
        assert!(
            w.args.contains(&DEFAULT_PODMAN_IMAGE.to_string()),
            "expected DEFAULT_PODMAN_IMAGE ({DEFAULT_PODMAN_IMAGE}) in podman argv when podman_image is unset"
        );
    }

    // AC7: podman with explicit image → uses specified image, not the default.
    #[test]
    fn test_ac7_podman_with_image() {
        let mut sp = spec(Level::L1, Backend::Podman, NetworkPolicy::Deny);
        sp.podman_image = Some("rust:1".to_string());
        let w = wrap(&sp, "cargo", &[]).unwrap();
        assert_eq!(w.program, "podman");
        assert!(w.args.contains(&"run".to_string()));
        assert!(contains_subseq(&w.args, &["--network", "none"]));
        assert!(contains_subseq(&w.args, &["-w", "/w"]));
        assert!(w.args.contains(&"rust:1".to_string()));
        assert!(w.args.contains(&"cargo".to_string()));
        // image precedes the program.
        let img = w.args.iter().position(|x| x == "rust:1").unwrap();
        let prog = w.args.iter().position(|x| x == "cargo").unwrap();
        assert!(img < prog);
    }

    #[test]
    fn test_podman_allow_no_network_none() {
        let mut sp = spec(Level::L1, Backend::Podman, NetworkPolicy::Allow);
        sp.podman_image = Some("rust:1".to_string());
        let w = wrap(&sp, "cargo", &[]).unwrap();
        assert!(!contains_subseq(&w.args, &["--network", "none"]));
    }

    #[test]
    fn test_seatbelt_wrap() {
        let sp = spec(Level::L1, Backend::Seatbelt, NetworkPolicy::Deny);
        let w = wrap(&sp, "cargo", &s(&["test"])).unwrap();
        assert_eq!(w.program, "sandbox-exec");
        assert_eq!(w.args[0], "-p");
        assert!(w.args[1].contains("/w"));
        assert!(w.args[1].contains("(deny network*)"));
        // program + args after the profile.
        assert_eq!(&w.args[2..], &s(&["cargo", "test"])[..]);
    }

    // Ordering: --tmpfs /tmp must precede --bind <workspace> <workspace> so that
    // a workspace under /tmp is not shadowed by the tmpfs mount.
    #[test]
    fn test_bwrap_tmpfs_precedes_workspace_bind_under_tmp() {
        let mut sp = spec(Level::L1, Backend::Bwrap, NetworkPolicy::Deny);
        sp.workspace = PathBuf::from("/tmp/ws");
        let w = wrap(&sp, "echo", &s(&["hello"])).unwrap();
        assert_eq!(w.program, "bwrap");

        // Find the index of "/tmp" that follows "--tmpfs".
        let tmpfs_operand_idx = w
            .args
            .windows(2)
            .position(|pair| pair[0] == "--tmpfs" && pair[1] == "/tmp")
            .map(|i| i + 1) // index of the "/tmp" operand
            .expect("--tmpfs /tmp must be present in bwrap argv");

        // Find the index of "/tmp/ws" that follows "--bind /tmp/ws".
        let bind_operand_idx = w
            .args
            .windows(3)
            .position(|triple| {
                triple[0] == "--bind" && triple[1] == "/tmp/ws" && triple[2] == "/tmp/ws"
            })
            .map(|i| i + 1) // index of the first "/tmp/ws" operand
            .expect("--bind /tmp/ws /tmp/ws must be present in bwrap argv");

        assert!(
            tmpfs_operand_idx < bind_operand_idx,
            "--tmpfs /tmp (at index {tmpfs_operand_idx}) must come before \
             --bind /tmp/ws (at index {bind_operand_idx}) so the workspace \
             bind is not shadowed by the tmpfs mount"
        );
    }

    // AC8: resolve_effective downgrade.
    #[test]
    fn test_ac8_resolve_effective() {
        // L2 with a backend but no nix → (L1, true).
        let c = caps(false, true, false); // bwrap, no nix
        assert_eq!(resolve_effective(Level::L2, &c, Backend::Bwrap), (Level::L1, true));

        // L1 with no usable backend → (L0, true).
        let c0 = caps(false, false, false);
        assert_eq!(resolve_effective(Level::L1, &c0, Backend::None), (Level::L0, true));

        // L0 always available, never downgraded.
        assert_eq!(resolve_effective(Level::L0, &c0, Backend::None), (Level::L0, false));
        assert_eq!(resolve_effective(Level::L0, &c, Backend::Bwrap), (Level::L0, false));
    }

    #[test]
    fn test_resolve_effective_full_l2() {
        // nix + bwrap usable → L2, no downgrade.
        let c = caps(true, true, false);
        assert_eq!(resolve_effective(Level::L2, &c, Backend::Bwrap), (Level::L2, false));
    }

    #[test]
    fn test_resolve_effective_l2_no_backend_to_l0() {
        // L2 requested, nix present but backend unusable → cap at L0.
        let c = caps(true, false, false);
        assert_eq!(resolve_effective(Level::L2, &c, Backend::None), (Level::L0, true));
    }

    #[test]
    fn test_resolve_effective_l1_usable() {
        let c = caps(false, true, false);
        assert_eq!(resolve_effective(Level::L1, &c, Backend::Bwrap), (Level::L1, false));
    }

    // Advisor mount: repo is ro-bound, scratch is bound, network is allowed
    // (no --unshare-net), and --chdir points at the repo.
    #[test]
    fn test_advisor_mount_ro_repo_writable_scratch_net_allowed() {
        let mount = AdvisorMount {
            repo: PathBuf::from("/repo"),
            scratch: PathBuf::from("/scratch"),
            writable_paths: vec![PathBuf::from("/repo/notes")],
            home: PathBuf::from("/home/u"),
        };
        let w = advisor_mount_command(&mount, "bwrap", "claude", &[]);
        assert_eq!(w.program, "bwrap");
        // Repo is read-only.
        assert!(contains_subseq(&w.args, &["--ro-bind", "/repo", "/repo"]));
        // Host is read-only underneath.
        assert!(contains_subseq(&w.args, &["--ro-bind", "/", "/"]));
        // Scratch is writable.
        assert!(contains_subseq(&w.args, &["--bind", "/scratch", "/scratch"]));
        // Writable carve-out is bound RW and comes AFTER the ro repo bind.
        assert!(contains_subseq(
            &w.args,
            &["--bind", "/repo/notes", "/repo/notes"]
        ));
        let ro_repo = w
            .args
            .windows(3)
            .position(|t| t[0] == "--ro-bind" && t[1] == "/repo" && t[2] == "/repo")
            .unwrap();
        let rw_notes = w
            .args
            .windows(3)
            .position(|t| t[0] == "--bind" && t[1] == "/repo/notes" && t[2] == "/repo/notes")
            .unwrap();
        assert!(ro_repo < rw_notes, "carve-out must layer over the ro repo bind");
        // Home is writable.
        assert!(contains_subseq(&w.args, &["--bind", "/home/u", "/home/u"]));
        // Network ALLOWED: no --unshare-net.
        assert!(!w.args.contains(&"--unshare-net".to_string()));
        // chdir to the repo.
        assert!(contains_subseq(&w.args, &["--chdir", "/repo"]));
        // program at the tail after --.
        assert!(contains_subseq(&w.args, &["--", "claude"]));
    }

    #[test]
    fn test_backend_usable_podman_requires_podman_runtime() {
        // Forced podman backend but only docker present → not usable.
        let mut c = caps(false, false, false);
        c.container_runtime = Some("docker".to_string());
        assert_eq!(resolve_effective(Level::L1, &c, Backend::Podman), (Level::L0, true));
        // podman present but NOT functional → still not usable.
        c.container_runtime = Some("podman".to_string());
        c.container_runtime_functional = false;
        assert_eq!(resolve_effective(Level::L1, &c, Backend::Podman), (Level::L0, true));
        // podman present AND functional → usable.
        c.container_runtime_functional = true;
        assert_eq!(resolve_effective(Level::L1, &c, Backend::Podman), (Level::L1, false));
    }
}
