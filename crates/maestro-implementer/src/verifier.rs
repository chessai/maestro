//! The verifier backend (ADR-002).
//!
//! A verifier judges an implementer's diff against the [`TaskSpec`]'s
//! acceptance criteria. Per ADR-002 the verifier **reports; it never mutates**:
//! it writes no files on the task branch. It judges from the provided unified
//! diff plus the mechanical gate's command output, and MAY run additional
//! commands in a *throwaway checkout* of the implementation — a copy severed
//! from the repo and discarded after the report. Those runs are supplied to the
//! backend via a [`VerifierCommandRunner`] seam; the daemon's records of them
//! (never the model's self-report) populate the frozen `commands_run` field.
//!
//! Two backends are provided:
//! - [`MockVerifier`] — deterministic, driven by the spec's acceptance
//!   criteria. Selected when `task.model == "mock"`. Drives the M2 escalation
//!   tests: a spec with no `mock:pass` criterion fails every attempt. It ignores
//!   the command runner.
//! - [`AnthropicVerifier`] — a real Anthropic Messages API client that runs a
//!   tool-use loop offering a read-only `emit_report` tool AND a bounded
//!   `run_command` tool (executed in the throwaway checkout). Wired to the
//!   documented wire shape but never exercised live in CI.

use maestro_journal::report::{CommandRun, Finding, ReportBody, Severity, Verdict};
use maestro_journal::spec::TaskSpec;
use serde_json::{json, Value};

use crate::ImplementerError;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const MAX_TOKENS: u64 = 8000;
/// The most `run_command` invocations a verifier may make in one verification.
const MAX_COMMANDS: u32 = 5;
/// Turn cap: enough to fit `MAX_COMMANDS` command round-trips, the final report,
/// and one nudge if the model stops without a tool call.
const TURN_BUDGET: u32 = MAX_COMMANDS + 4;

/// A single command the verifier ran in its throwaway checkout, recorded by the
/// daemon (authoritative telemetry — never the model's self-report).
///
/// The daemon-side runner returns this from [`VerifierCommandRunner::run`]; the
/// [`AnthropicVerifier`] both echoes a capped `output_excerpt` back to the model
/// and keeps the full record to override `report.commands_run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifierCommandRun {
    /// The shell command that was run.
    pub cmd: String,
    /// Exit code, or `-1` when killed / no code / not available.
    pub exit: i64,
    /// sha256 (hex) of the FULL combined stdout+stderr.
    pub output_digest: String,
    /// The combined output, capped, as shown to the model.
    pub output_excerpt: String,
}

/// The seam by which a verifier runs additional commands in a throwaway checkout
/// of the implementation (ADR-002). The daemon supplies a runner backed by a
/// severed copy of the worktree; unit tests and any path with no checkout supply
/// [`NoCommandRunner`].
pub trait VerifierCommandRunner {
    /// Run one shell command in the throwaway checkout; return its record.
    fn run(&self, cmd: &str) -> VerifierCommandRun;
}

/// A [`VerifierCommandRunner`] that runs nothing: it reports every command as
/// unavailable (`exit: -1`). Used by unit tests and any path without a checkout.
pub struct NoCommandRunner;

impl VerifierCommandRunner for NoCommandRunner {
    fn run(&self, cmd: &str) -> VerifierCommandRun {
        let excerpt = "command execution is not available in this context".to_string();
        VerifierCommandRun {
            cmd: cmd.to_string(),
            exit: -1,
            output_digest: sha256_hex(&excerpt),
            output_excerpt: excerpt,
        }
    }
}

/// sha256 hex digest of `s` (no `sha256:` prefix), for a [`VerifierCommandRun`]'s
/// `output_digest`. A tiny inline SHA-256 keeps maestro-implementer free of an
/// extra crate dependency; the daemon reuses its own `sha2`-based helper.
fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finish_hex()
}

/// A minimal, dependency-free SHA-256 (FIPS 180-4). Only used here to digest a
/// short constant string in [`NoCommandRunner`]; the daemon's real runner uses
/// the `sha2` crate.
struct Sha256 {
    state: [u32; 8],
    buf: Vec<u8>,
    len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                0x1f83d9ab, 0x5be0cd19,
            ],
            buf: Vec::new(),
            len: 0,
        }
    }
    fn update(&mut self, data: &[u8]) {
        self.len += data.len() as u64;
        self.buf.extend_from_slice(data);
        while self.buf.len() >= 64 {
            let block: [u8; 64] = self.buf[..64].try_into().unwrap();
            self.process(&block);
            self.buf.drain(..64);
        }
    }
    fn finish_hex(mut self) -> String {
        let bit_len = self.len * 8;
        self.buf.push(0x80);
        while self.buf.len() % 64 != 56 {
            self.buf.push(0);
        }
        self.buf.extend_from_slice(&bit_len.to_be_bytes());
        let buf = std::mem::take(&mut self.buf);
        for chunk in buf.chunks_exact(64) {
            let block: [u8; 64] = chunk.try_into().unwrap();
            self.process(&block);
        }
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }
    fn process(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let j = i * 4;
            *word = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = self.state;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (i, val) in v.iter().enumerate() {
            self.state[i] = self.state[i].wrapping_add(*val);
        }
    }
}

/// A unit of verification work handed to a [`VerifierBackend`].
///
/// The verifier has no worktree: it judges from `diff` + `gate_output` only.
pub struct VerifyTask {
    /// The immutable spec (acceptance_criteria + title) (ADR-003).
    pub spec: TaskSpec,
    /// Unified diff of the implementer's changes vs `spec.base_ref`.
    pub diff: String,
    /// Mechanical-gate command outputs (build/test/lint) (ADR-002).
    pub gate_output: String,
    /// The verifier model id, e.g. `"mock"` or `"claude-sonnet-4-6"`.
    pub model: String,
    /// Anthropic `base_url` override; else `$ANTHROPIC_BASE_URL`, else default.
    pub base_url: Option<String>,
    /// Earlier verifier reports, for escalation context (ADR-002 / ADR-003).
    pub prior_reports: Vec<ReportBody>,
    /// The METER-ONLY task id sent as `X-Maestro-Meter` when the verifier is
    /// routed through the streaming credential proxy (ADR-006). When `Some`, the
    /// proxy meters the verifier's usage into the per-task ledger (so total task
    /// spend is accurate) but NEVER gates or hard-stops it — the verifier must
    /// always run (ADR-002 "verification never skipped"). `None` on the direct
    /// (proxy-off) path, where no such header is sent.
    pub meter_header: Option<String>,
}

/// A verifier's account of a verification: the report plus telemetry.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// The frozen report body (ADR-002).
    pub report: ReportBody,
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// The abstraction every verifier backend satisfies. Verifiers report; they
/// never mutate the task branch (ADR-002). A verifier MAY run bounded commands
/// via `runner`, which executes them in a throwaway checkout severed from the
/// repo.
pub trait VerifierBackend {
    /// Judge `task` and produce a report. Never writes the task branch. `runner`
    /// supplies the throwaway-checkout command capability (ADR-002).
    fn verify(
        &self,
        task: &VerifyTask,
        runner: &dyn VerifierCommandRunner,
    ) -> Result<VerifyOutcome, ImplementerError>;
}

/// Map the daemon's authoritative [`VerifierCommandRun`] records into the frozen
/// `commands_run` report elements (ADR-002 `{cmd, exit, output_digest}`). The
/// model's own `commands_run` is DISCARDED in favor of these records.
fn commands_run_from_records(records: &[VerifierCommandRun]) -> Vec<CommandRun> {
    records
        .iter()
        .map(|r| CommandRun {
            cmd: r.cmd.clone(),
            exit: r.exit,
            output_digest: r.output_digest.clone(),
        })
        .collect()
}

/// A deterministic verifier for tests and the M2 escalation loop.
///
/// Rule: **pass iff any `acceptance_criteria[i].check` equals `"mock:pass"`
/// (case-insensitive); otherwise fail.** A spec with no `mock:pass` criterion
/// fails on every attempt, which is what drives the escalation tests.
pub struct MockVerifier;

impl VerifierBackend for MockVerifier {
    fn verify(
        &self,
        task: &VerifyTask,
        _runner: &dyn VerifierCommandRunner,
    ) -> Result<VerifyOutcome, ImplementerError> {
        let passes = task
            .spec
            .acceptance_criteria
            .iter()
            .any(|c| c.check.eq_ignore_ascii_case("mock:pass"));

        let report = if passes {
            ReportBody {
                verdict: Verdict::Pass,
                findings: Vec::new(),
                out_of_scope_diff: false,
                commands_run: Vec::new(),
            }
        } else {
            // `fail` requires at least one `blocker` (ADR-002). Key it to the
            // first criterion's id, or null when there are none.
            let criterion_id = task
                .spec
                .acceptance_criteria
                .first()
                .map(|c| c.id.clone());
            ReportBody {
                verdict: Verdict::Fail,
                findings: vec![Finding {
                    severity: Severity::Blocker,
                    criterion_id,
                    evidence: "mock verifier: criterion not satisfied".into(),
                }],
                out_of_scope_diff: false,
                commands_run: Vec::new(),
            }
        };

        Ok(VerifyOutcome {
            report,
            turns: 1,
            tokens_in: 0,
            tokens_out: 0,
        })
    }
}

/// A real Anthropic Messages API verifier.
///
/// Construct with [`AnthropicVerifier::new`], which takes an optional `base_url`
/// override and does NOT require the API key eagerly — the key is only read at
/// request time in [`VerifierBackend::verify`], which returns
/// [`ImplementerError::Unavailable`] when it is missing.
///
/// The verifier is given a single read-only `emit_report` tool; it is offered
/// no file-writing tool, so "verifiers never mutate" (ADR-002) is structural.
pub struct AnthropicVerifier {
    base_url_override: Option<String>,
}

impl AnthropicVerifier {
    /// Build a verifier with an optional `base_url` override (ADR-008). This is
    /// non-eager: it never reads or requires `ANTHROPIC_API_KEY`; that check is
    /// deferred to [`VerifierBackend::verify`].
    pub fn new(base_url: Option<String>) -> Self {
        let base_url_override = base_url.filter(|s| !s.is_empty());
        Self { base_url_override }
    }

    /// The effective base URL at request time: the explicit override, else
    /// `$ANTHROPIC_BASE_URL`, else the compiled-in default.
    fn effective_base_url(&self, task: &VerifyTask) -> String {
        if let Some(ref u) = self.base_url_override {
            return u.clone();
        }
        if let Some(u) = task.base_url.as_ref().filter(|s| !s.is_empty()) {
            return u.clone();
        }
        std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into())
    }
}

impl VerifierBackend for AnthropicVerifier {
    fn verify(
        &self,
        task: &VerifyTask,
        runner: &dyn VerifierCommandRunner,
    ) -> Result<VerifyOutcome, ImplementerError> {
        // The API key is read (and required) here, not at construction time.
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            return Err(ImplementerError::Unavailable(
                "ANTHROPIC_API_KEY is unset or empty".into(),
            ));
        }

        let base_url = self.effective_base_url(task);
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));

        let mut messages = vec![initial_user_message(task)];
        let mut turns: u32 = 0;
        let mut tokens_in: u64 = 0;
        let mut tokens_out: u64 = 0;
        // The number of `run_command` invocations made so far (capped at
        // MAX_COMMANDS). These are the daemon-recorded, authoritative runs.
        let mut command_count: u32 = 0;
        let mut recorded_runs: Vec<VerifierCommandRun> = Vec::new();
        // We allow exactly one nudge if the model fails to call any tool.
        let mut nudged = false;

        loop {
            if turns >= TURN_BUDGET {
                return Err(ImplementerError::Budget(format!(
                    "turn budget of {TURN_BUDGET} exhausted before a report was emitted"
                )));
            }
            turns += 1;

            let body = build_verify_request_body(task, &messages);
            let response = send(&url, &api_key, task.meter_header.as_deref(), &body)?;

            if let Some(usage) = response.get("usage") {
                tokens_in += usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                tokens_out += usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }

            let content = response
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ImplementerError::Protocol("response missing `content` array".into())
                })?
                .clone();

            // If the model emitted `emit_report` (possibly alongside a
            // `run_command`), we FINISH: prefer the report. Its own
            // `commands_run` is discarded in favor of the daemon's records.
            if let Some(input) = find_tool_input(&content, "emit_report") {
                let mut report = parse_report(&input)?;
                report.commands_run = commands_run_from_records(&recorded_runs);
                return Ok(VerifyOutcome {
                    report,
                    turns,
                    tokens_in,
                    tokens_out,
                });
            }

            // Otherwise, collect every `run_command` tool_use in this turn. The
            // API requires a `tool_result` for EVERY tool_use id in an assistant
            // turn we echo back, so we answer each one (running it, or a
            // budget-exhausted result once over the cap).
            let run_uses = collect_run_command_uses(&content);
            if !run_uses.is_empty() {
                messages.push(json!({ "role": "assistant", "content": content }));
                let mut tool_results = Vec::with_capacity(run_uses.len());
                for (tool_use_id, cmd) in run_uses {
                    let result_text = if command_count < MAX_COMMANDS {
                        command_count += 1;
                        let record = runner.run(&cmd);
                        let text = format!("exit={}\n{}", record.exit, record.output_excerpt);
                        recorded_runs.push(record);
                        text
                    } else {
                        "the command budget is exhausted; you must call `emit_report` now"
                            .to_string()
                    };
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": result_text,
                    }));
                }
                messages.push(json!({ "role": "user", "content": tool_results }));
                continue;
            }

            // No tool call at all. Nudge once, else Protocol.
            if nudged {
                return Err(ImplementerError::Protocol(
                    "model stopped without calling `emit_report` after a nudge".into(),
                ));
            }
            nudged = true;
            messages.push(json!({ "role": "assistant", "content": content }));
            messages.push(json!({
                "role": "user",
                "content": "You did not call a tool. You must report your verdict by calling \
                            `emit_report` exactly once now (optionally after `run_command`).",
            }));
        }
    }
}

/// The default no-output stall timeout for the driven verifier's CLI phase.
const DRIVEN_VERIFY_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(300);
/// The default absolute wall-clock ceiling for the driven verifier's CLI phase —
/// bounds an actively-emitting-but-looping session the idle watchdog would miss.
const DRIVEN_VERIFY_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(600);

/// A verifier that produces the structured verdict by DRIVING the local
/// subscription `claude` CLI read-only (ADR-002), rather than calling the
/// Anthropic API. This lets verification run with NO API credits: the CLI
/// authenticates via its subscription.
///
/// It runs the CLI in a SINGLE `--permission-mode plan` phase (via
/// [`maestro_driver::run_claude_verify_readonly`]) in a throwaway checkout, so
/// the model cannot edit anything (read-only is structural: plan mode forbids
/// edits, and the checkout's `.git` is severed). The prompt (built by
/// [`build_verifier_prompt`]) carries the acceptance criteria, the diff, the
/// gate output, and prior reports, and instructs the model to end with exactly
/// one fenced ```json verdict block. The final assistant text is parsed by
/// [`parse_verdict_from_text`] into a [`ReportBody`], enforcing the ADR-002
/// fail-requires-blocker invariant.
///
/// The `runner` argument to [`VerifierBackend::verify`] is IGNORED: this backend
/// gathers evidence through the CLI's own read-only tools inside the checkout,
/// not through the daemon's `run_command` seam, so `commands_run` is left empty
/// (the daemon overrides `commands_run` from its authoritative records on the API
/// path; on the driven path there are no daemon-recorded runs).
pub struct DrivenVerifier {
    /// The CLI program to spawn (e.g. `"claude"`, or `"bash"` in tests).
    program: String,
    /// Arguments before the injected stream-json flags (e.g. `["--print"]`, or a
    /// fake-script path in tests).
    args: Vec<String>,
    /// The directory the CLI runs in — the throwaway checkout (severed `.git`).
    /// `None` when the daemon could not build a checkout; the CLI then runs in a
    /// fresh empty tempdir it makes itself (it still judges from diff+gate text).
    cwd: Option<std::path::PathBuf>,
    /// Env-var names to strip from the child (subscription CLIs: drop API keys so
    /// they never bill per-token).
    env_remove: Vec<String>,
    /// No-output stall watchdog.
    watchdog: std::time::Duration,
    /// Absolute wall-clock ceiling for the phase.
    wall_clock: std::time::Duration,
}

impl DrivenVerifier {
    /// Build a driven verifier for a `driven_cli` verifier role.
    ///
    /// `program`/`args` come from the role's `command`/`args`; `cwd` is the
    /// throwaway checkout the daemon prepared (severed from the repo). `env_remove`
    /// strips provider API keys for a subscription CLI. Timeouts default to
    /// [`DRIVEN_VERIFY_WATCHDOG`] / [`DRIVEN_VERIFY_WALL_CLOCK`] via
    /// [`DrivenVerifier::new`]; [`DrivenVerifier::with_timeouts`] overrides them
    /// (tests use short values).
    pub fn new(
        program: String,
        args: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        env_remove: Vec<String>,
    ) -> Self {
        Self::with_timeouts(
            program,
            args,
            cwd,
            env_remove,
            DRIVEN_VERIFY_WATCHDOG,
            DRIVEN_VERIFY_WALL_CLOCK,
        )
    }

    /// Like [`DrivenVerifier::new`] but with explicit timeouts.
    pub fn with_timeouts(
        program: String,
        args: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        env_remove: Vec<String>,
        watchdog: std::time::Duration,
        wall_clock: std::time::Duration,
    ) -> Self {
        DrivenVerifier {
            program,
            args,
            cwd,
            env_remove,
            watchdog,
            wall_clock,
        }
    }
}

impl VerifierBackend for DrivenVerifier {
    fn verify(
        &self,
        task: &VerifyTask,
        _runner: &dyn VerifierCommandRunner,
    ) -> Result<VerifyOutcome, ImplementerError> {
        let prompt = build_verifier_prompt(task);

        // The CLI's cwd: the daemon-prepared throwaway checkout, else a fresh
        // empty tempdir the verifier makes itself (kept alive for the phase). The
        // model still judges from the diff + gate text embedded in the prompt.
        let (cwd, _tmp): (std::path::PathBuf, Option<std::path::PathBuf>) = match &self.cwd {
            Some(p) => (p.clone(), None),
            None => {
                let t = std::env::temp_dir().join(format!(
                    "maestro-driven-verify-{}",
                    std::process::id(),
                ));
                // Best-effort; a create failure still runs with temp_dir as cwd.
                let _ = std::fs::create_dir_all(&t);
                (t.clone(), Some(t))
            }
        };

        // A per-run log file alongside the cwd (best-effort location).
        let log_path = cwd.join("maestro-verify.log");

        let outcome = maestro_driver::run_claude_verify_readonly(
            &self.program,
            &self.args,
            &prompt,
            &cwd,
            &log_path,
            self.watchdog,
            Some(self.wall_clock),
            &self.env_remove,
        );

        // A non-clean terminal (spawn error, wedge, wall-clock, kill, non-zero
        // exit) is a verifier CRASH, mapped to a retryable error (ADR-002: a
        // crash retries once, then `internal_error`). An empty transcript on a
        // "clean" exit is likewise a crash (no verdict emitted).
        match outcome.reason {
            maestro_driver::EndReason::Completed => {}
            other => {
                return Err(ImplementerError::Protocol(format!(
                    "driven verifier CLI did not complete cleanly: {other:?}"
                )));
            }
        }

        let report = parse_verdict_from_text(&outcome.assistant_text)?;

        Ok(VerifyOutcome {
            report,
            turns: outcome.turns,
            tokens_in: outcome.tokens_in.unwrap_or(0),
            tokens_out: outcome.tokens_out.unwrap_or(0),
        })
    }
}

/// Build the driven verifier's prompt: the same skeptical framing + evidence as
/// the API path ([`user_message_text`] + the API system prompt), but ending with
/// an instruction to emit the verdict as EXACTLY ONE fenced ```json block (there
/// are no API tools on the driven path).
///
/// Pure (no I/O), so it is unit-testable without the CLI.
pub fn build_verifier_prompt(task: &VerifyTask) -> String {
    let mut text = String::new();

    // The skeptical framing (mirrors the API system prompt), adapted: the driven
    // model MAY read files / run read-only bash in its checkout to gather
    // evidence, but it CANNOT edit and it reports via a fenced JSON block, not a
    // tool call.
    text.push_str(
        "You are a code-change verifier. You did NOT write this code. Judge whether the \
         provided unified DIFF satisfies each acceptance criterion, using the mechanical-gate \
         command output as evidence. Be skeptical: do not accept a criterion as met unless the \
         diff plainly demonstrates it, and watch for out-of-scope changes. You CANNOT edit any \
         files. You MAY read files or run read-only commands in your working directory (a \
         throwaway checkout) to gather evidence.\n\n",
    );

    // The evidence block: title, criteria, diff, gate output, prior reports —
    // reuse the API path's exact rendering so both backends judge from identical
    // material.
    text.push_str(&user_message_text(task));

    // The verdict instruction: EXACTLY ONE fenced json block, nothing after it,
    // with the schema inline (ADR-002 frozen shape).
    text.push_str(
        "\n---\n\
         When you are done, emit your verdict as EXACTLY ONE fenced ```json code block \
         matching the schema below, and output NOTHING after that block. Do not wrap it in \
         prose after the closing fence.\n\n\
         Schema:\n\
         ```json\n\
         {\n\
         \x20 \"verdict\": \"pass | fail\",\n\
         \x20 \"findings\": [\n\
         \x20   { \"severity\": \"blocker | concern | note\", \"criterion_id\": \"<id> | null\", \
         \"evidence\": \"<verbatim evidence>\" }\n\
         \x20 ],\n\
         \x20 \"out_of_scope_diff\": false,\n\
         \x20 \"commands_run\": []\n\
         }\n\
         ```\n\n\
         Rules: `verdict` is \"pass\" or \"fail\". A \"fail\" verdict REQUIRES at least one \
         finding with severity \"blocker\". `criterion_id` is the acceptance-criterion id the \
         finding relates to, or null. Leave `commands_run` as an empty array.\n",
    );

    text
}

/// Parse the driven CLI's final assistant text into a [`ReportBody`].
///
/// Extraction rule (the load-bearing, fragile part):
/// 1. Scan for ALL fenced code blocks whose info string is `json` (``` ```json```
///    … ``` ``` ```), in order.
/// 2. Take the LAST such block whose contents parse as a JSON object with a
///    `verdict` field — the model is instructed to end with exactly one verdict
///    block, and a trailing narrative is common; taking the last verdict block
///    tolerates earlier illustrative/schema blocks quoted in the reasoning.
/// 3. If NO ```json block parses into a report, fall back to the LAST balanced
///    top-level `{…}` object in the text that parses into a report (defensive: a
///    model that forgot the fence but still emitted the object).
/// 4. `serde_json::from_str` into a [`ReportBody`]; enforce the ADR-002
///    fail-requires-blocker invariant via [`parse_report`] (synthesizing a
///    blocker rather than rejecting a genuine fail).
///
/// A missing/invalid verdict block is an [`ImplementerError::Protocol`] error
/// (the daemon retries a verifier crash once, then fails `internal_error`).
///
/// Pure (no I/O), so it is unit-testable EXHAUSTIVELY without the CLI.
pub fn parse_verdict_from_text(text: &str) -> Result<ReportBody, ImplementerError> {
    // SECURITY (adversarial review): a verifier that turns a genuine FAIL into a
    // PASS ships broken code, so this parser is FAIL-CLOSED. Only fenced ```json
    // blocks whose `verdict` is EXACTLY "pass" or "fail" count as a verdict (a
    // quoted schema like "pass | fail" does not). Then:
    //   * 0 verdict blocks              → Protocol error (retry; NEVER a default pass)
    //   * blocks disagree on the verdict → ambiguous → Protocol error (fail-closed)
    //   * all agree (incl. exactly one) → parse it
    // There is deliberately NO unfenced fallback: a bare `{"verdict":"pass"}` in
    // prose — e.g. echoed from the diff under review, or an illustrative example —
    // must never be treated as the verdict.
    let blocks: Vec<Value> = fenced_json_blocks(text)
        .iter()
        .filter_map(|block| serde_json::from_str::<Value>(block.trim()).ok())
        .filter(|v| valid_verdict(v).is_some())
        .collect();

    if blocks.is_empty() {
        return Err(ImplementerError::Protocol(
            "driven verifier emitted no parseable ```json verdict block \
             (verdict must be exactly \"pass\" or \"fail\")"
                .into(),
        ));
    }
    let distinct: std::collections::BTreeSet<&str> =
        blocks.iter().filter_map(valid_verdict).collect();
    if distinct.len() > 1 {
        return Err(ImplementerError::Protocol(
            "driven verifier emitted conflicting verdict blocks (ambiguous); the \
             model must output EXACTLY ONE ```json verdict block"
                .into(),
        ));
    }
    parse_report(&blocks[0])
}

/// The verdict string of `v` iff it is exactly `"pass"` or `"fail"` — so a quoted
/// schema (`"pass | fail"`) or any other value is NOT counted as a verdict.
fn valid_verdict(v: &Value) -> Option<&str> {
    match v.get("verdict").and_then(Value::as_str) {
        Some(s) if s == "pass" || s == "fail" => Some(s),
        _ => None,
    }
}

/// Return the CONTENTS of every fenced code block whose info string is exactly
/// `json` (case-insensitive), in document order. A block opens on a line whose
/// trimmed text is ```` ```json ```` and closes on the next line whose trimmed
/// text is ```` ``` ````. An unterminated final block yields the text to
/// end-of-string (a truncated transcript should still surrender its verdict).
fn fenced_json_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut lines = text.lines();
    let mut current: Option<String> = None;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        match current {
            None => {
                // Opening fence: ```json (allow trailing spaces, any case for
                // the language tag).
                if let Some(rest) = trimmed.strip_prefix("```") {
                    if rest.trim().eq_ignore_ascii_case("json") {
                        current = Some(String::new());
                    }
                }
            }
            Some(ref mut buf) => {
                if trimmed == "```" {
                    blocks.push(std::mem::take(buf));
                    current = None;
                } else {
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
    }
    // An unterminated block (truncated output) still contributes its contents.
    if let Some(buf) = current {
        if !buf.trim().is_empty() {
            blocks.push(buf);
        }
    }
    blocks
}

/// Find the `input` of the first `tool_use` block named `name`, if present.
fn find_tool_input(content: &[Value], name: &str) -> Option<Value> {
    content.iter().find_map(|block| {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return None;
        }
        if block.get("name").and_then(Value::as_str) != Some(name) {
            return None;
        }
        block.get("input").cloned()
    })
}

/// Collect `(tool_use_id, cmd)` for every `run_command` tool_use in `content`,
/// in order. A block missing an id or `cmd` string is skipped (the API always
/// supplies both; skipping is defensive).
fn collect_run_command_uses(content: &[Value]) -> Vec<(String, String)> {
    content
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            if block.get("name").and_then(Value::as_str) != Some("run_command") {
                return None;
            }
            let id = block.get("id").and_then(Value::as_str)?.to_string();
            let cmd = block
                .get("input")
                .and_then(|i| i.get("cmd"))
                .and_then(Value::as_str)?
                .to_string();
            Some((id, cmd))
        })
        .collect()
}

/// Parse an `emit_report` tool input into a [`ReportBody`] and enforce the
/// ADR-002 invariant.
///
/// Invariant handling: if `verdict == fail` but the report carries no `blocker`
/// finding, the report is malformed. We **normalize** by synthesizing a single
/// `blocker` finding rather than rejecting, so a well-intentioned "fail" is not
/// dropped on a technicality; the synthesized finding is clearly labelled. (The
/// spec permits either normalization or a `Protocol` error; normalization is
/// chosen to keep a genuine fail verdict actionable.)
fn parse_report(input: &Value) -> Result<ReportBody, ImplementerError> {
    let mut report: ReportBody = serde_json::from_value(input.clone()).map_err(|e| {
        ImplementerError::Protocol(format!("emit_report input is not a valid report: {e}"))
    })?;

    if report.verdict == Verdict::Fail
        && !report
            .findings
            .iter()
            .any(|f| f.severity == Severity::Blocker)
    {
        report.findings.push(Finding {
            severity: Severity::Blocker,
            criterion_id: None,
            evidence: "synthesized blocker: verifier returned verdict=fail with no blocker \
                       finding (ADR-002 invariant repair)"
                .into(),
        });
    }

    Ok(report)
}

/// Build the first user message: the acceptance criteria, the unified diff, the
/// mechanical-gate output, and a summary of any prior reports.
fn initial_user_message(task: &VerifyTask) -> Value {
    json!({ "role": "user", "content": user_message_text(task) })
}

/// The text of the first user message (pure; no network / filesystem).
fn user_message_text(task: &VerifyTask) -> String {
    let spec = &task.spec;

    let mut text = String::new();
    text.push_str(&format!("# Verify task: {}\n\n", spec.title));

    text.push_str("## Acceptance criteria\n");
    if spec.acceptance_criteria.is_empty() {
        text.push_str("(none specified)\n");
    } else {
        for c in &spec.acceptance_criteria {
            text.push_str(&format!("- {} ({:?}): {}\n", c.id, c.kind, c.check));
        }
    }

    text.push_str("\n## Unified diff (implementer's changes vs base)\n");
    text.push_str("```diff\n");
    text.push_str(&task.diff);
    text.push_str("\n```\n");

    text.push_str("\n## Mechanical gate output (build/test/lint)\n");
    text.push_str("```\n");
    text.push_str(&task.gate_output);
    text.push_str("\n```\n");

    if !task.prior_reports.is_empty() {
        text.push_str("\n## Prior verifier reports (escalation context)\n");
        for (i, r) in task.prior_reports.iter().enumerate() {
            let blockers = r
                .findings
                .iter()
                .filter(|f| f.severity == Severity::Blocker)
                .count();
            text.push_str(&format!(
                "- attempt {}: verdict={:?}, {} finding(s) ({} blocker(s)), out_of_scope_diff={}\n",
                i + 1,
                r.verdict,
                r.findings.len(),
                blockers,
                r.out_of_scope_diff,
            ));
            for f in &r.findings {
                text.push_str(&format!(
                    "    - {:?} [{}]: {}\n",
                    f.severity,
                    f.criterion_id.as_deref().unwrap_or("-"),
                    f.evidence,
                ));
            }
        }
    }

    text.push_str(
        "\nJudge whether the DIFF satisfies each acceptance criterion. Report by calling \
         `emit_report` exactly once.\n",
    );

    text
}

/// The single read-only `emit_report` tool (documented wire shape). It mirrors
/// the [`ReportBody`] schema. No file-writing tool is ever offered.
fn emit_report_tool() -> Value {
    json!({
        "name": "emit_report",
        "description": "Report the verification verdict. Call this exactly once. `verdict` \
                        must be \"pass\" or \"fail\"; a \"fail\" verdict requires at least one \
                        finding with severity \"blocker\".",
        "input_schema": {
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["pass", "fail"] },
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "severity": {
                                "type": "string",
                                "enum": ["blocker", "concern", "note"]
                            },
                            "criterion_id": { "type": ["string", "null"] },
                            "evidence": { "type": "string" }
                        },
                        "required": ["severity", "criterion_id", "evidence"]
                    }
                },
                "out_of_scope_diff": { "type": "boolean" },
                "commands_run": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "cmd": { "type": "string" },
                            "exit": { "type": "integer" },
                            "output_digest": { "type": "string" }
                        },
                        "required": ["cmd", "exit", "output_digest"]
                    }
                }
            },
            "required": ["verdict", "findings", "out_of_scope_diff", "commands_run"]
        }
    })
}

/// The bounded `run_command` tool (documented wire shape). It runs a shell
/// command in a throwaway checkout of the implementation — a copy severed from
/// the repo, discarded after verification — so its mutations can never reach the
/// task branch. Offered alongside `emit_report`; it is not a file-editing tool
/// on the task branch, so "verifiers never mutate [the branch]" (ADR-002) holds.
fn run_command_tool() -> Value {
    json!({
        "name": "run_command",
        "description": "Run one shell command in a THROWAWAY CHECKOUT of the implementation \
                        (a copy discarded after verification). Mutations in the copy cannot \
                        reach the task branch. Use it to build, run tests, or inspect files to \
                        gather evidence before you call `emit_report`. Output is capped.",
        "input_schema": {
            "type": "object",
            "properties": {
                "cmd": { "type": "string" }
            },
            "required": ["cmd"]
        }
    })
}

/// Construct the request body for a single Messages API call.
///
/// Pure and network-free so it can be unit-tested. `prior_messages` is the full
/// `messages` array accumulated so far (starting with the initial user turn).
/// Offers both the `run_command` tool (bounded, throwaway checkout) and the
/// terminal `emit_report` tool.
pub fn build_verify_request_body(task: &VerifyTask, prior_messages: &[Value]) -> Value {
    let system = "You are a code-change verifier. You did NOT write this code. Judge whether \
         the provided unified DIFF satisfies each acceptance criterion, using the \
         mechanical-gate command output as evidence. Be skeptical: do not accept a \
         criterion as met unless the diff plainly demonstrates it, and watch for \
         out-of-scope changes. You cannot edit the task branch. You MAY call `run_command` a \
         few times to build, test, or inspect in a THROWAWAY checkout (discarded afterward) \
         to gather evidence. You MUST report your verdict by calling the `emit_report` tool \
         exactly once.";

    json!({
        "model": task.model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": prior_messages,
        "tools": [run_command_tool(), emit_report_tool()],
    })
}

/// Send one request and parse the JSON response, mapping transport/status
/// errors to [`ImplementerError::Http`] and parse failures to
/// [`ImplementerError::Protocol`].
///
/// When `meter_header` is `Some`, the request carries an `X-Maestro-Meter` header
/// so the daemon's streaming credential proxy meters the verifier's usage into
/// the per-task ledger (making the implementer's pre-forward gate reflect TOTAL
/// task spend). It is METER-ONLY: the proxy never gates or hard-stops a verifier
/// request, so no `429 budget_exhausted` mapping is needed here (ADR-002
/// "verification never skipped"). On the direct (proxy-off) path `meter_header`
/// is `None` and no such header is sent — the request is identical to before.
fn send(
    url: &str,
    api_key: &str,
    meter_header: Option<&str>,
    body: &Value,
) -> Result<Value, ImplementerError> {
    let mut req = ureq::post(url)
        .set("x-api-key", api_key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .set("content-type", "application/json");
    if let Some(mid) = meter_header {
        req = req.set("X-Maestro-Meter", mid);
    }
    let resp = req.send_json(body);

    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| ImplementerError::Protocol(format!("cannot parse response JSON: {e}"))),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r
                .into_string()
                .unwrap_or_else(|_| "<unreadable body>".into());
            Err(ImplementerError::Http(format!("status {code}: {detail}")))
        }
        Err(ureq::Error::Transport(t)) => {
            Err(ImplementerError::Http(format!("transport error: {t}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::domain::Tier;
    use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind};

    fn spec_with_criteria(criteria: Vec<AcceptanceCriterion>) -> TaskSpec {
        TaskSpec {
            title: "Add x".into(),
            tier: Tier::T0,
            base_ref: "main".into(),
            file_allowlist: vec!["src/lib.rs".into()],
            instructions: "Add a function x".into(),
            acceptance_criteria: criteria,
            check_commands: vec!["cargo build".into()],
            house_rules_ref: None,
            budget: Budget::default(),
            lifetime_budget: Default::default(),
            containment_min: 0,
        }
    }

    fn task(model: &str, spec: TaskSpec) -> VerifyTask {
        VerifyTask {
            spec,
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+pub fn x() {}\n".into(),
            gate_output: "cargo build: ok\ncargo test: ok\n".into(),
            model: model.into(),
            base_url: None,
            prior_reports: Vec::new(),
            meter_header: None,
        }
    }

    fn ac(id: &str, check: &str, kind: CriterionKind) -> AcceptanceCriterion {
        AcceptanceCriterion {
            id: id.into(),
            check: check.into(),
            kind,
        }
    }

    // AC4: no mock:pass criterion → Fail with exactly one blocker.
    #[test]
    fn mock_fails_without_mock_pass() {
        let spec = spec_with_criteria(vec![
            ac("AC1", "cargo build", CriterionKind::Command),
            ac("AC2", "x is exported", CriterionKind::Invariant),
        ]);
        let out = MockVerifier
            .verify(&task("mock", spec), &NoCommandRunner)
            .unwrap();
        assert_eq!(out.report.verdict, Verdict::Fail);
        let blockers: Vec<_> = out
            .report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
            .collect();
        assert_eq!(blockers.len(), 1, "expected exactly one blocker");
        assert_eq!(out.report.findings.len(), 1);
        // Keyed to the first criterion.
        assert_eq!(blockers[0].criterion_id.as_deref(), Some("AC1"));
        assert_eq!(out.turns, 1);
    }

    #[test]
    fn mock_fail_criterion_id_null_when_no_criteria() {
        let spec = spec_with_criteria(vec![]);
        let out = MockVerifier
            .verify(&task("mock", spec), &NoCommandRunner)
            .unwrap();
        assert_eq!(out.report.verdict, Verdict::Fail);
        assert_eq!(out.report.findings.len(), 1);
        assert_eq!(out.report.findings[0].criterion_id, None);
    }

    // AC5: a mock:pass criterion → Pass, zero blockers.
    #[test]
    fn mock_passes_with_mock_pass_criterion() {
        let spec = spec_with_criteria(vec![ac("AC1", "mock:pass", CriterionKind::Invariant)]);
        let out = MockVerifier
            .verify(&task("mock", spec), &NoCommandRunner)
            .unwrap();
        assert_eq!(out.report.verdict, Verdict::Pass);
        let blockers = out
            .report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
            .count();
        assert_eq!(blockers, 0);
        assert!(out.report.findings.is_empty());
        assert!(out.report.commands_run.is_empty());
        assert!(!out.report.out_of_scope_diff);
    }

    #[test]
    fn mock_pass_is_case_insensitive() {
        let spec = spec_with_criteria(vec![ac("AC1", "MOCK:PASS", CriterionKind::Invariant)]);
        let out = MockVerifier
            .verify(&task("mock", spec), &NoCommandRunner)
            .unwrap();
        assert_eq!(out.report.verdict, Verdict::Pass);
    }

    // AC6: construct without a key; verify with ANTHROPIC_API_KEY unset →
    // Unavailable (save/restore env).
    #[test]
    fn verify_unavailable_without_key() {
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let verifier = AnthropicVerifier::new(None);
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let res = verifier.verify(&task("claude-sonnet-4-6", spec), &NoCommandRunner);
        assert!(
            matches!(res, Err(ImplementerError::Unavailable(_))),
            "verify must fail Unavailable without a key, got {res:?}"
        );

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }

    // AC7: build_verify_request_body wire shape.
    #[test]
    fn build_verify_request_body_matches_wire_shape() {
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let t = task("claude-sonnet-4-6", spec);
        let messages = vec![initial_user_message(&t)];
        let body = build_verify_request_body(&t, &messages);

        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["max_tokens"], MAX_TOKENS);

        // BOTH tools are offered: the bounded `run_command` AND `emit_report`.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"run_command"), "run_command tool offered");
        assert!(names.contains(&"emit_report"), "emit_report tool offered");

        // No file-writing tool on the task branch is offered.
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert_ne!(name, "write_file");
            assert!(
                !name.contains("write") && !name.contains("edit"),
                "no file-writing tool allowed, got {name}"
            );
        }

        // The first user message text contains the diff string.
        let first_user = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            first_user.contains(&t.diff),
            "first user message must contain the diff"
        );

        // The emit_report tool schema mirrors the ReportBody shape.
        let emit = tools
            .iter()
            .find(|t| t["name"] == "emit_report")
            .expect("emit_report present");
        let schema = &emit["input_schema"];
        assert_eq!(schema["properties"]["verdict"]["enum"][0], "pass");
        assert_eq!(schema["properties"]["verdict"]["enum"][1], "fail");
        assert!(schema["properties"]["findings"].is_object());
        assert!(schema["properties"]["out_of_scope_diff"].is_object());
        assert!(schema["properties"]["commands_run"].is_object());

        // The run_command tool takes a single `cmd` string.
        let run = tools
            .iter()
            .find(|t| t["name"] == "run_command")
            .expect("run_command present");
        assert_eq!(run["input_schema"]["properties"]["cmd"]["type"], "string");
        assert_eq!(run["input_schema"]["required"][0], "cmd");
    }

    #[test]
    fn base_url_precedence_override_then_task_then_env_then_default() {
        let saved = std::env::var("ANTHROPIC_BASE_URL").ok();
        std::env::remove_var("ANTHROPIC_BASE_URL");
        let spec = spec_with_criteria(vec![]);

        // Explicit override wins.
        let v = AnthropicVerifier::new(Some("http://override.example".into()));
        let mut t = task("claude-sonnet-4-6", spec.clone());
        t.base_url = Some("http://task.example".into());
        assert_eq!(v.effective_base_url(&t), "http://override.example");

        // No override → task.base_url.
        let v2 = AnthropicVerifier::new(None);
        assert_eq!(v2.effective_base_url(&t), "http://task.example");

        // No override, no task url → env.
        std::env::set_var("ANTHROPIC_BASE_URL", "http://env.example");
        let t2 = task("claude-sonnet-4-6", spec.clone());
        assert_eq!(v2.effective_base_url(&t2), "http://env.example");

        // Nothing → default.
        std::env::remove_var("ANTHROPIC_BASE_URL");
        assert_eq!(v2.effective_base_url(&t2), DEFAULT_BASE_URL);

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_BASE_URL", v);
        }
    }

    #[test]
    fn parse_report_normalizes_fail_without_blocker() {
        // verdict fail with only a `concern` → a blocker is synthesized.
        let input = json!({
            "verdict": "fail",
            "findings": [
                { "severity": "concern", "criterion_id": "AC1", "evidence": "weak" }
            ],
            "out_of_scope_diff": false,
            "commands_run": []
        });
        let report = parse_report(&input).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report
            .findings
            .iter()
            .any(|f| f.severity == Severity::Blocker));
    }

    #[test]
    fn parse_report_passes_through_valid_pass() {
        let input = json!({
            "verdict": "pass",
            "findings": [],
            "out_of_scope_diff": false,
            "commands_run": [
                { "cmd": "cargo test", "exit": 0, "output_digest": "abc" }
            ]
        });
        let report = parse_report(&input).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(report.commands_run.len(), 1);
        assert!(report.findings.is_empty());
    }

    /// A [`VerifierCommandRunner`] mock that returns canned output and records
    /// every command it was asked to run.
    struct RecordingRunner {
        exit: i64,
        excerpt: String,
        seen: std::cell::RefCell<Vec<String>>,
    }

    impl RecordingRunner {
        fn new(exit: i64, excerpt: &str) -> Self {
            RecordingRunner {
                exit,
                excerpt: excerpt.into(),
                seen: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl VerifierCommandRunner for RecordingRunner {
        fn run(&self, cmd: &str) -> VerifierCommandRun {
            self.seen.borrow_mut().push(cmd.to_string());
            VerifierCommandRun {
                cmd: cmd.to_string(),
                exit: self.exit,
                output_digest: sha256_hex(&self.excerpt),
                output_excerpt: self.excerpt.clone(),
            }
        }
    }

    // The recorded runs — NOT the model's own commands_run — populate the report.
    #[test]
    fn commands_run_override_uses_daemon_records_not_model() {
        // A report the "model" emitted, carrying a bogus self-reported command.
        let input = json!({
            "verdict": "pass",
            "findings": [],
            "out_of_scope_diff": false,
            "commands_run": [
                { "cmd": "rm -rf /", "exit": 0, "output_digest": "MODEL-LIE" }
            ]
        });
        let mut report = parse_report(&input).unwrap();
        // The daemon's authoritative records.
        let records = vec![
            VerifierCommandRun {
                cmd: "cargo test".into(),
                exit: 0,
                output_digest: "digest-a".into(),
                output_excerpt: "ok".into(),
            },
            VerifierCommandRun {
                cmd: "cargo clippy".into(),
                exit: 1,
                output_digest: "digest-b".into(),
                output_excerpt: "warn".into(),
            },
        ];
        // The override the AnthropicVerifier applies before returning.
        report.commands_run = commands_run_from_records(&records);

        assert_eq!(report.commands_run.len(), 2, "model's self-report discarded");
        assert_eq!(report.commands_run[0].cmd, "cargo test");
        assert_eq!(report.commands_run[0].exit, 0);
        assert_eq!(report.commands_run[0].output_digest, "digest-a");
        assert_eq!(report.commands_run[1].cmd, "cargo clippy");
        assert_eq!(report.commands_run[1].exit, 1);
        assert_eq!(report.commands_run[1].output_digest, "digest-b");
        // The model's fabricated entry is nowhere in the result.
        assert!(!report
            .commands_run
            .iter()
            .any(|c| c.output_digest == "MODEL-LIE"));
    }

    // MockVerifier ignores the runner entirely (never invokes it).
    #[test]
    fn mock_verifier_ignores_runner() {
        let spec = spec_with_criteria(vec![ac("AC1", "mock:pass", CriterionKind::Invariant)]);
        let runner = RecordingRunner::new(0, "unused");
        let out = MockVerifier
            .verify(&task("mock", spec), &runner)
            .unwrap();
        assert_eq!(out.report.verdict, Verdict::Pass);
        assert!(out.report.commands_run.is_empty());
        assert!(runner.seen.borrow().is_empty(), "mock must not run commands");
    }

    // NoCommandRunner reports unavailability with a stable digest of its excerpt.
    #[test]
    fn no_command_runner_reports_unavailable() {
        let r = NoCommandRunner.run("cargo test");
        assert_eq!(r.cmd, "cargo test");
        assert_eq!(r.exit, -1);
        assert_eq!(
            r.output_excerpt,
            "command execution is not available in this context"
        );
        assert_eq!(r.output_digest, sha256_hex(&r.output_excerpt));
    }

    // The inline SHA-256 matches known test vectors.
    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn user_message_includes_prior_reports_when_present() {
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let mut t = task("claude-sonnet-4-6", spec);
        t.prior_reports.push(ReportBody {
            verdict: Verdict::Fail,
            findings: vec![Finding {
                severity: Severity::Blocker,
                criterion_id: Some("AC1".into()),
                evidence: "build broke".into(),
            }],
            out_of_scope_diff: false,
            commands_run: Vec::new(),
        });
        let text = user_message_text(&t);
        assert!(text.contains("Prior verifier reports"));
        assert!(text.contains("build broke"));
    }

    // ---- DrivenVerifier: build_verifier_prompt --------------------------------

    #[test]
    fn build_verifier_prompt_contains_evidence_and_json_instruction() {
        let spec = spec_with_criteria(vec![
            ac("AC1", "x is exported", CriterionKind::Invariant),
            ac("AC2", "cargo build passes", CriterionKind::Command),
        ]);
        let t = task("claude-sonnet-4-6", spec);
        let prompt = build_verifier_prompt(&t);

        // Skeptical framing + read-only, no-edit stance.
        assert!(prompt.contains("You did NOT write this code"));
        assert!(prompt.contains("CANNOT edit"));
        // The criteria are present.
        assert!(prompt.contains("AC1"));
        assert!(prompt.contains("AC2"));
        // The diff + gate output are fenced into the prompt.
        assert!(prompt.contains(&t.diff), "diff must be embedded");
        assert!(prompt.contains(&t.gate_output), "gate output must be embedded");
        // The JSON verdict instruction + inline schema fields.
        assert!(prompt.contains("EXACTLY ONE fenced ```json"));
        assert!(prompt.contains("NOTHING after"));
        assert!(prompt.contains("\"verdict\""));
        assert!(prompt.contains("\"findings\""));
        assert!(prompt.contains("\"out_of_scope_diff\""));
        assert!(prompt.contains("\"commands_run\""));
        // The fail-requires-blocker rule is stated.
        assert!(prompt.contains("REQUIRES at least one"));
    }

    #[test]
    fn build_verifier_prompt_includes_prior_reports() {
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let mut t = task("claude-sonnet-4-6", spec);
        t.prior_reports.push(ReportBody {
            verdict: Verdict::Fail,
            findings: vec![Finding {
                severity: Severity::Blocker,
                criterion_id: Some("AC1".into()),
                evidence: "prior blocker text".into(),
            }],
            out_of_scope_diff: false,
            commands_run: Vec::new(),
        });
        let prompt = build_verifier_prompt(&t);
        assert!(prompt.contains("Prior verifier reports"));
        assert!(prompt.contains("prior blocker text"));
    }

    // ---- DrivenVerifier: parse_verdict_from_text (the fragile part) -----------

    #[test]
    fn parse_verdict_clean_pass() {
        let text = "Here is my assessment.\n\n```json\n{\n  \"verdict\": \"pass\",\n  \
                    \"findings\": [],\n  \"out_of_scope_diff\": false,\n  \"commands_run\": []\n}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
        assert!(report.findings.is_empty());
        assert!(!report.out_of_scope_diff);
        assert!(report.commands_run.is_empty());
    }

    #[test]
    fn parse_verdict_fail_with_blocker() {
        let text = "```json\n{\"verdict\":\"fail\",\"findings\":[\
                    {\"severity\":\"blocker\",\"criterion_id\":\"AC1\",\"evidence\":\"missing fn\"}],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        let blockers = report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
            .count();
        assert_eq!(blockers, 1);
        assert_eq!(report.findings[0].criterion_id.as_deref(), Some("AC1"));
    }

    #[test]
    fn parse_verdict_fail_without_blocker_synthesizes_one() {
        // A fail with only a `concern` must have a blocker synthesized (ADR-002).
        let text = "```json\n{\"verdict\":\"fail\",\"findings\":[\
                    {\"severity\":\"concern\",\"criterion_id\":\"AC1\",\"evidence\":\"weak\"}],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.severity == Severity::Blocker),
            "a blocker must be synthesized so a genuine fail survives the invariant"
        );
    }

    #[test]
    fn parse_verdict_trims_narrative_after_json() {
        // Model emits the block then keeps talking; we still parse the block.
        let text = "```json\n{\"verdict\":\"pass\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```\n\n\
                    Note: I also noticed the tests are a bit thin, but the criteria are met.";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
    }

    #[test]
    fn parse_verdict_schema_illustration_excluded() {
        // A quoted schema block (`"verdict":"pass | fail"`) is NOT a valid verdict
        // (verdict must be exactly "pass"/"fail"), so it is EXCLUDED; the one real
        // verdict block is used. (No conflict → not ambiguous.)
        let text = "First, the schema I will fill in:\n\
                    ```json\n{\"verdict\":\"pass | fail\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```\n\n\
                    Now my actual verdict:\n\
                    ```json\n{\"verdict\":\"fail\",\"findings\":[\
                    {\"severity\":\"blocker\",\"criterion_id\":\"AC1\",\"evidence\":\"regression\"}],\
                    \"out_of_scope_diff\":true,\"commands_run\":[]}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report.out_of_scope_diff);
        assert_eq!(report.findings[0].evidence, "regression");
    }

    #[test]
    fn parse_verdict_skips_non_verdict_json_block_before_real_one() {
        // A ```json block WITHOUT a `verdict` key (e.g. the model echoing some
        // other JSON) must be skipped; the real verdict block wins even though it
        // comes first here — a non-verdict block never overrides it.
        let text = "```json\n{\"verdict\":\"pass\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```\n\n\
                    Supporting data:\n```json\n{\"note\":\"some other object\"}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
    }

    #[test]
    fn parse_verdict_invalid_json_is_protocol_error() {
        let text = "```json\n{ this is not valid json at all }\n```";
        let err = parse_verdict_from_text(text).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn parse_verdict_no_block_is_protocol_error() {
        let text = "I think this looks fine, verdict: pass. (no fenced block at all)";
        let err = parse_verdict_from_text(text).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn parse_verdict_unknown_severity_is_protocol_error() {
        // `severity: "critical"` is not in the enum → serde rejects → Protocol.
        let text = "```json\n{\"verdict\":\"fail\",\"findings\":[\
                    {\"severity\":\"critical\",\"criterion_id\":\"AC1\",\"evidence\":\"x\"}],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let err = parse_verdict_from_text(text).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn parse_verdict_unknown_verdict_is_protocol_error() {
        let text = "```json\n{\"verdict\":\"maybe\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let err = parse_verdict_from_text(text).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn parse_verdict_fenced_language_tag_case_insensitive() {
        let text = "```JSON\n{\"verdict\":\"pass\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
    }

    #[test]
    fn parse_verdict_unterminated_fence_still_parses() {
        // A truncated transcript (no closing fence) should still surrender its
        // verdict rather than being lost.
        let text = "```json\n{\"verdict\":\"pass\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
    }

    #[test]
    fn parse_verdict_unfenced_object_is_rejected() {
        // Adversarial review: a bare (unfenced) verdict object in prose must NOT
        // be accepted — a worker could embed `{"verdict":"pass"}` in a source file
        // the model then echoes. Only a fenced ```json block counts; else error.
        let text = "My verdict is: {\"verdict\":\"pass\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]} — done.";
        let err = parse_verdict_from_text(text).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn parse_verdict_conflicting_blocks_are_ambiguous_not_pass() {
        // Adversarial review (the reproduced spurious-PASS): the model emits its
        // REAL fail verdict, then an illustrative pass EXAMPLE after it. This MUST
        // be fail-closed (Protocol error) — never silently a PASS.
        let text = "The function is unimplemented — a blocker.\n\
                    ```json\n{\"verdict\":\"fail\",\"findings\":[\
                    {\"severity\":\"blocker\",\"criterion_id\":\"AC1\",\"evidence\":\"unimplemented\"}],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```\n\n\
                    For reference, a passing verdict would look like:\n\
                    ```json\n{\"verdict\":\"pass\",\"findings\":[],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let err = parse_verdict_from_text(text).unwrap_err();
        assert!(matches!(err, ImplementerError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn parse_verdict_object_with_braces_in_strings() {
        // Evidence text containing braces must not confuse the brace matcher.
        let text = "```json\n{\"verdict\":\"fail\",\"findings\":[\
                    {\"severity\":\"blocker\",\"criterion_id\":null,\
                    \"evidence\":\"code was `fn f() { todo!() }` — unimplemented\"}],\
                    \"out_of_scope_diff\":false,\"commands_run\":[]}\n```";
        let report = parse_verdict_from_text(text).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report.findings[0].evidence.contains("todo!()"));
    }
}
