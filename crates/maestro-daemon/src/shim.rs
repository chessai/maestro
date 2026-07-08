//! Search / fetch-extract executor (ADR-005). The daemon owns the search
//! backend wiring, the `shim_cache`, and the `sessions` journaling; the shim
//! *engine* (`maestro-shim`) owns fetching, readability, model extraction, and
//! verbatim location. This module composes the two.
//!
//! # Contract (ADR-005)
//! - `search` is metadata-only: no model, never cached. An unset/unreachable
//!   backend is a loud `backend_unavailable` tool error, never silent.
//! - `fetch_extract` fetches the URL, runs readability, calls the extraction
//!   model for verbatim spans (verbatim text only — the model reports no
//!   offsets), then **locates each verbatim in the fetched content, computing
//!   its offset, and rejects any verbatim it cannot find** — purity is
//!   *checked*, not requested. A rejected result is never cached.
//! - There is no free-text summary anywhere; the model only maps page text →
//!   schema fields with verbatim offsets.
//!
//! # Test seams
//! Both executors are parameterized over `&dyn SearchBackend` / `&dyn
//! ExtractionModel` so tests pass mocks (no network). The fetch step of
//! `fetch_extract` is injected via a [`ContentFetcher`]; production wires
//! [`http_fetcher`] (`maestro_shim::fetch` + `html_to_text`), tests supply
//! deterministic content that the mock model's verbatim must occur in (or not,
//! to exercise rejection).

use std::sync::{Arc, Mutex};

use maestro_journal::domain::{ExitStatus, Role, SessionKind};
use maestro_journal::proto::Response;
use maestro_journal::Journal;
use maestro_shim::{
    locate_offsets, schema_hash, Extraction, ExtractionModel, SearchBackend, ShimError,
};
use sha2::{Digest, Sha256};

/// The cache TTL for `fetch_extract` results (ADR-005: 24h).
const CACHE_TTL_SECS: i64 = 24 * 60 * 60;

/// Injectable fetch step for `fetch_extract`. Given a URL, return the
/// **already-cleaned** page text (post-readability) that offsets index into.
/// Production wires [`http_fetcher`]; tests supply deterministic content.
pub trait ContentFetcher {
    fn fetch_content(&self, url: &str) -> Result<String, ShimError>;
}

/// Production fetcher: `maestro_shim::fetch` (HTTP GET) then
/// `maestro_shim::html_to_text` (readability). This is the real network path.
pub struct HttpFetcher;

impl ContentFetcher for HttpFetcher {
    fn fetch_content(&self, url: &str) -> Result<String, ShimError> {
        let html = maestro_shim::fetch(url)?;
        Ok(maestro_shim::html_to_text(&html))
    }
}

/// The production [`ContentFetcher`].
pub fn http_fetcher() -> HttpFetcher {
    HttpFetcher
}

/// `sha256:<hex>` digest of the cleaned content, matching the ADR-005
/// `content_digest` shape.
fn content_digest(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Parse an RFC-3339 timestamp to Unix seconds; `None` if unparseable (a
/// malformed cached timestamp is treated as expired, forcing a refetch).
fn rfc3339_to_unix(ts: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    OffsetDateTime::parse(ts, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// Whether a cache entry retrieved at `retrieved_at` is still fresh relative to
/// `now` (both RFC-3339). A retrieved_at within [now - TTL, now] is fresh.
fn cache_fresh(retrieved_at: &str, now: &str) -> bool {
    match (rfc3339_to_unix(retrieved_at), rfc3339_to_unix(now)) {
        (Some(r), Some(n)) => n.saturating_sub(r) < CACHE_TTL_SECS && n >= r,
        _ => false,
    }
}

/// `search` executor (ADR-005): journal a `role='shim'` session, run the
/// metadata-only backend search, and return the results. `model_name` is the
/// backend label recorded in the session (there is no model in the search path).
///
/// - `BackendUnavailable` → a loud `backend_unavailable` [`Response::Error`].
/// - `Http`/`Protocol` → a `search failed` [`Response::Error`].
/// - success → [`Response::SearchResults`] with the serialized results.
pub fn run_search(
    journal: &Arc<Mutex<Journal>>,
    advisor_session_id: &str,
    backend: &dyn SearchBackend,
    model_name: &str,
    queries: &[String],
) -> Response {
    // Journal the shim session up front (task_id=None, attributed to advisor).
    let session_id = {
        let j = journal.lock().expect("journal mutex poisoned");
        match j.insert_session(
            None,
            Some(advisor_session_id),
            Role::Shim,
            model_name,
            SessionKind::OneShotApi,
            None,
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "shim search: failed to journal session");
                None
            }
        }
    };

    let result = backend.search(queries);

    // Best-effort session finalization; never overrides the tool result.
    let exit = match &result {
        Ok(_) => ExitStatus::Ok,
        Err(_) => ExitStatus::Error,
    };
    finish_shim_session(journal, session_id.as_deref(), exit);

    match result {
        Ok(results) => match serde_json::to_value(&results) {
            Ok(value) => Response::SearchResults { results: value },
            Err(e) => Response::Error {
                message: format!("search serialize failed: {e}"),
            },
        },
        Err(ShimError::BackendUnavailable(msg)) => Response::Error {
            message: format!("backend_unavailable: {msg}"),
        },
        Err(e) => Response::Error {
            message: format!("search failed: {e}"),
        },
    }
}

/// Truncate an [`ExtractionField`]'s verbatim to at most `cap_chars` Unicode
/// scalar values (chars). If truncation occurs the `char_offset` end is updated
/// to reflect the shorter byte span (offsets are byte-based; the truncated
/// string is a prefix of the original verbatim so its byte length is the new
/// span width). A `cap_chars` of 0 means unlimited — never truncate to empty.
fn cap_field(mut field: maestro_shim::ExtractionField, cap_chars: usize) -> maestro_shim::ExtractionField {
    if cap_chars == 0 {
        return field;
    }
    let char_count = field.verbatim.chars().count();
    if char_count <= cap_chars {
        return field;
    }
    // Find the byte boundary after `cap_chars` chars (char-safe truncation).
    let byte_end = field
        .verbatim
        .char_indices()
        .nth(cap_chars)
        .map(|(i, _)| i)
        .unwrap_or(field.verbatim.len());
    field.verbatim.truncate(byte_end);
    // Update the offset end: start is unchanged; new end = start + new byte length.
    field.char_offset[1] = field.char_offset[0] + field.verbatim.len();
    field
}

/// `fetch_extract` executor (ADR-005). See the module docs for the contract.
///
/// 1. Cache lookup by `(url, schema_hash)`; a fresh (<24h) hit returns the cached
///    [`Response::Extraction`] **without** fetching or calling the model.
/// 2. Miss/stale → fetch + clean via `fetcher`; fetch errors → `fetch failed`.
/// 3. `model.extract` → `ModelUnavailable` → `extraction model unavailable`.
/// 4. `locate_offsets` → a verbatim not found in the content (`VerbatimNotFound`)
///    is **rejected** (never cached); found verbatims get daemon-computed offsets.
/// 5. Each located verbatim is capped to `cap_chars` Unicode scalar values
///    (ADR-005 / ADR-007: `shim.excerpt_cap_chars`). `cap_chars = 0` → no cap.
/// 6. Valid → build + cache the [`Extraction`], journal the session, return it.
#[allow(clippy::too_many_arguments)]
pub fn run_fetch_extract(
    journal: &Arc<Mutex<Journal>>,
    advisor_session_id: &str,
    fetcher: &dyn ContentFetcher,
    model: &dyn ExtractionModel,
    model_name: &str,
    url: &str,
    schema_fields: &[String],
    cap_chars: usize,
) -> Response {
    let h = schema_hash(schema_fields);
    let now = maestro_journal::now_iso8601();

    // 1. Cache lookup (fresh hit short-circuits — no fetch, no model call).
    {
        let j = journal.lock().expect("journal mutex poisoned");
        match j.shim_cache_get(url, &h) {
            Ok(Some((retrieved_at, payload))) if cache_fresh(&retrieved_at, &now) => {
                match serde_json::from_str::<serde_json::Value>(&payload) {
                    Ok(value) => return Response::Extraction { extraction: value },
                    Err(e) => {
                        // A corrupt cache row is not fatal; fall through to refetch.
                        tracing::warn!(error = %e, url, "shim cache payload unparseable; refetching");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, url, "shim cache lookup failed; proceeding to fetch");
            }
        }
    }

    // 2. Fetch + readability (injected seam).
    let content = match fetcher.fetch_content(url) {
        Ok(c) => c,
        Err(e) => {
            return Response::Error {
                message: format!("fetch failed: {e}"),
            };
        }
    };

    // Journal the shim session for the model call (task_id=None, advisor-scoped).
    let session_id = {
        let j = journal.lock().expect("journal mutex poisoned");
        match j.insert_session(
            None,
            Some(advisor_session_id),
            Role::Shim,
            model_name,
            SessionKind::OneShotApi,
            None,
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "shim fetch_extract: failed to journal session");
                None
            }
        }
    };

    // 3. Extraction model: verbatim text only (no offsets).
    let raws = match model.extract(&content, schema_fields) {
        Ok(r) => r,
        Err(ShimError::ModelUnavailable(msg)) => {
            finish_shim_session(journal, session_id.as_deref(), ExitStatus::Error);
            return Response::Error {
                message: format!("extraction model unavailable: {msg}"),
            };
        }
        Err(e) => {
            finish_shim_session(journal, session_id.as_deref(), ExitStatus::Error);
            return Response::Error {
                message: format!("extraction failed: {e}"),
            };
        }
    };

    // 4. Locate each verbatim in the content (daemon computes offsets) — the
    //    exit criterion. A verbatim not present in the page is a hallucination:
    //    reject it and do NOT cache.
    let located = match locate_offsets(&content, &raws) {
        Ok(f) => f,
        Err(ShimError::VerbatimNotFound { field }) => {
            finish_shim_session(journal, session_id.as_deref(), ExitStatus::Error);
            return Response::Error {
                message: format!("rejected: verbatim not found in content — field {field}"),
            };
        }
        Err(e) => {
            finish_shim_session(journal, session_id.as_deref(), ExitStatus::Error);
            return Response::Error {
                message: format!("extraction failed: {e}"),
            };
        }
    };

    // 5. Apply the per-field excerpt cap (ADR-005 / ADR-007: `shim.excerpt_cap_chars`).
    //    Truncation is char-safe; offsets are adjusted to reflect the shorter span.
    //    The cache is populated with already-capped excerpts so cache hits stay
    //    within the cap without a second pass.
    let fields: Vec<_> = located.into_iter().map(|f| cap_field(f, cap_chars)).collect();

    // 6. Valid: build the extraction, cache it, finalize the session, return it.
    let extraction = Extraction {
        url: url.to_string(),
        retrieved_at: now.clone(),
        content_digest: content_digest(&content),
        extractions: fields,
    };

    let value = match serde_json::to_value(&extraction) {
        Ok(v) => v,
        Err(e) => {
            finish_shim_session(journal, session_id.as_deref(), ExitStatus::Error);
            return Response::Error {
                message: format!("extraction serialize failed: {e}"),
            };
        }
    };

    {
        let j = journal.lock().expect("journal mutex poisoned");
        if let Err(e) = j.shim_cache_put(url, &h, &now, &value.to_string()) {
            tracing::warn!(error = %e, url, "shim cache put failed (result still returned)");
        }
    }
    finish_shim_session(journal, session_id.as_deref(), ExitStatus::Ok);

    Response::Extraction { extraction: value }
}

/// Best-effort session finalization (token accounting is not yet wired for the
/// shim path; ADR-005 says shim calls enter cost accounting but the engine does
/// not surface token counts today — recorded as a TODO ambiguity).
fn finish_shim_session(
    journal: &Arc<Mutex<Journal>>,
    session_id: Option<&str>,
    exit: ExitStatus,
) {
    let Some(session_id) = session_id else { return };
    let j = journal.lock().expect("journal mutex poisoned");
    if let Err(e) = j.finish_session(session_id, exit, None, None, None) {
        tracing::warn!(error = %e, session_id, "shim: finish_session failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_shim::{
        MockSearchBackend, RawExtraction, SearchResult, SearxngBackend,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn journal() -> Arc<Mutex<Journal>> {
        Arc::new(Mutex::new(Journal::open_in_memory().unwrap()))
    }

    fn advisor(j: &Arc<Mutex<Journal>>) -> String {
        j.lock()
            .unwrap()
            .create_advisor("test", "claude-fable-5", "standard")
            .unwrap()
    }

    /// A counting fetcher returning fixed content; asserts how often fetch ran.
    struct CountingFetcher {
        content: String,
        calls: AtomicUsize,
    }
    impl ContentFetcher for CountingFetcher {
        fn fetch_content(&self, _url: &str) -> Result<String, ShimError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.content.clone())
        }
    }

    /// A counting extraction model returning fixed raw verbatims; asserts call
    /// count. The daemon locates each verbatim itself.
    struct CountingModel {
        raws: Vec<RawExtraction>,
        calls: AtomicUsize,
    }
    impl ExtractionModel for CountingModel {
        fn extract(
            &self,
            _content: &str,
            _schema_fields: &[String],
        ) -> Result<Vec<RawExtraction>, ShimError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.raws.clone())
        }
    }

    // AC4: an unset SearxngBackend → backend_unavailable Response::Error.
    #[test]
    fn ac4_search_backend_unavailable_is_loud() {
        let j = journal();
        let adv = advisor(&j);
        let backend = SearxngBackend::new(None);
        let resp = run_search(&j, &adv, &backend, "searxng", &["rust async".to_string()]);
        match resp {
            Response::Error { message } => {
                assert!(
                    message.contains("backend_unavailable"),
                    "message must mention backend_unavailable: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // A configured backend returns metadata results, and a shim session was
    // journaled attributed to the advisor with task_id NULL.
    #[test]
    fn search_success_journals_shim_session() {
        let j = journal();
        let adv = advisor(&j);
        let backend = MockSearchBackend {
            results: vec![SearchResult {
                query: "q".to_string(),
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
                engine_snippet: "snippet".to_string(),
                rank: 1,
                retrieved_at: "2026-07-07T00:00:00Z".to_string(),
            }],
        };
        let resp = run_search(&j, &adv, &backend, "searxng", &["q".to_string()]);
        match resp {
            Response::SearchResults { results } => {
                assert_eq!(results.as_array().unwrap().len(), 1);
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }
        // A shim session exists for the advisor, task_id NULL, kind one_shot_api.
        let guard = j.lock().unwrap();
        let (count, role, kind, task_null): (i64, String, String, i64) = guard
            .connection()
            .query_row(
                "SELECT COUNT(*), MAX(role), MAX(kind), SUM(task_id IS NULL)
                 FROM sessions WHERE advisor_session_id = ?1",
                [&adv],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(role, "shim");
        assert_eq!(kind, "one_shot_api");
        assert_eq!(task_null, 1, "shim session must have task_id NULL");
    }

    // AC5 (exit criterion): a verbatim absent from the content is rejected and
    // NOT cached — the daemon's locate step catches the hallucination.
    #[test]
    fn ac5_verbatim_not_found_rejected_and_not_cached() {
        let j = journal();
        let adv = advisor(&j);
        let content = "Hello world".to_string();
        let fetcher = CountingFetcher {
            content: content.clone(),
            calls: AtomicUsize::new(0),
        };
        // "Goodbye" does not occur in "Hello world" → located as not-found.
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "greeting".to_string(),
                verbatim: "Goodbye".to_string(),
            }],
            calls: AtomicUsize::new(0),
        };
        let schema = vec!["greeting".to_string()];
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com",
            &schema,
            0, // no cap
        );
        match resp {
            Response::Error { message } => {
                assert!(
                    message.contains("rejected") && message.contains("verbatim not found"),
                    "message must indicate a verbatim-not-found rejection: {message}"
                );
            }
            other => panic!("expected rejection Error, got {other:?}"),
        }
        // Nothing cached for this (url, schema_hash).
        let h = schema_hash(&schema);
        let guard = j.lock().unwrap();
        assert!(
            guard
                .shim_cache_get("https://example.com", &h)
                .unwrap()
                .is_none(),
            "a rejected extraction must not be cached"
        );
    }

    // AC6: a valid extraction is returned and cached; a second identical call is
    // served from cache without re-fetching or re-invoking the model.
    #[test]
    fn ac6_valid_extraction_caches_and_second_call_hits_cache() {
        let j = journal();
        let adv = advisor(&j);
        let content = "Hello world".to_string();
        let fetcher = CountingFetcher {
            content: content.clone(),
            calls: AtomicUsize::new(0),
        };
        // "Hello" occurs at byte 0 in "Hello world" — daemon locates [0,5].
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "greeting".to_string(),
                verbatim: "Hello".to_string(),
            }],
            calls: AtomicUsize::new(0),
        };
        let schema = vec!["greeting".to_string()];

        // First call: fetch + model + locate + cache.
        let r1 = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com",
            &schema,
            0, // no cap
        );
        let v1 = match r1 {
            Response::Extraction { extraction } => extraction,
            other => panic!("expected Extraction, got {other:?}"),
        };
        assert_eq!(v1.get("url").and_then(|u| u.as_str()), Some("https://example.com"));
        assert!(v1.get("content_digest").and_then(|d| d.as_str()).unwrap().starts_with("sha256:"));
        // The extraction carries a correct daemon-located offset.
        let ext = v1
            .get("extractions")
            .and_then(|e| e.as_array())
            .unwrap();
        assert_eq!(ext.len(), 1);
        let off = ext[0].get("char_offset").and_then(|o| o.as_array()).unwrap();
        assert_eq!(off[0].as_u64(), Some(0));
        assert_eq!(off[1].as_u64(), Some(5));
        assert_eq!(ext[0].get("verbatim").and_then(|v| v.as_str()), Some("Hello"));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);

        // Second identical call: cache hit → no fetch, no model call.
        let r2 = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com",
            &schema,
            0, // no cap
        );
        let v2 = match r2 {
            Response::Extraction { extraction } => extraction,
            other => panic!("expected cached Extraction, got {other:?}"),
        };
        assert_eq!(v1, v2, "cached result must equal the first result");
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            1,
            "cache hit must not re-fetch"
        );
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "cache hit must not re-invoke the model"
        );
    }

    // A stale cache entry (retrieved_at older than the TTL) forces a refetch.
    #[test]
    fn stale_cache_entry_is_refetched() {
        let j = journal();
        let adv = advisor(&j);
        let schema = vec!["greeting".to_string()];
        let h = schema_hash(&schema);
        // Seed a stale entry (well beyond 24h ago).
        j.lock()
            .unwrap()
            .shim_cache_put(
                "https://example.com",
                &h,
                "2000-01-01T00:00:00Z",
                r#"{"stale":true}"#,
            )
            .unwrap();

        let fetcher = CountingFetcher {
            content: "Hello world".to_string(),
            calls: AtomicUsize::new(0),
        };
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "greeting".to_string(),
                verbatim: "Hello".to_string(),
            }],
            calls: AtomicUsize::new(0),
        };
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com",
            &schema,
            0, // no cap
        );
        assert!(matches!(resp, Response::Extraction { .. }));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "stale entry must refetch");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    // The model-unavailable path surfaces a distinct, loud error.
    #[test]
    fn model_unavailable_is_surfaced() {
        struct Unavailable;
        impl ExtractionModel for Unavailable {
            fn extract(
                &self,
                _c: &str,
                _f: &[String],
            ) -> Result<Vec<RawExtraction>, ShimError> {
                Err(ShimError::ModelUnavailable("no api key".to_string()))
            }
        }
        let j = journal();
        let adv = advisor(&j);
        let fetcher = CountingFetcher {
            content: "content".to_string(),
            calls: AtomicUsize::new(0),
        };
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &Unavailable,
            "claude-haiku-4-5",
            "https://example.com",
            &["f".to_string()],
            0, // no cap
        );
        match resp {
            Response::Error { message } => {
                assert!(message.contains("extraction model unavailable"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ADR-005 / ADR-007: excerpt_cap_chars enforcement.
    // A verbatim longer than cap_chars is truncated to exactly cap_chars chars,
    // the returned excerpt is a prefix of the original, and the char_offset span
    // length (in bytes) equals the truncated verbatim's byte length.
    #[test]
    fn excerpt_cap_truncates_long_verbatim() {
        let j = journal();
        let adv = advisor(&j);
        // Content with a long span that will be returned as verbatim by the model.
        let long = "abcdefghij"; // 10 ASCII chars
        let content = long.to_string();
        let fetcher = CountingFetcher {
            content: content.clone(),
            calls: AtomicUsize::new(0),
        };
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "body".to_string(),
                verbatim: long.to_string(), // full 10 chars
            }],
            calls: AtomicUsize::new(0),
        };
        let schema = vec!["body".to_string()];
        let cap = 5usize;
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com/cap",
            &schema,
            cap,
        );
        let extraction = match resp {
            Response::Extraction { extraction } => extraction,
            other => panic!("expected Extraction, got {other:?}"),
        };
        let extractions = extraction.get("extractions").and_then(|e| e.as_array()).unwrap();
        assert_eq!(extractions.len(), 1);
        let verbatim = extractions[0].get("verbatim").and_then(|v| v.as_str()).unwrap();
        // The returned verbatim must be exactly cap chars long and a prefix of original.
        assert_eq!(
            verbatim.chars().count(),
            cap,
            "verbatim must be truncated to exactly cap_chars={cap} chars"
        );
        assert!(
            long.starts_with(verbatim),
            "truncated verbatim must be a prefix of the original"
        );
        // The char_offset span byte-length must equal the truncated verbatim's byte-length.
        let off = extractions[0].get("char_offset").and_then(|o| o.as_array()).unwrap();
        let start = off[0].as_u64().unwrap() as usize;
        let end = off[1].as_u64().unwrap() as usize;
        assert_eq!(
            end - start,
            verbatim.len(),
            "char_offset span byte-length must match truncated verbatim byte-length"
        );
    }

    // A verbatim shorter than cap_chars is returned unchanged.
    #[test]
    fn excerpt_cap_leaves_short_verbatim_unchanged() {
        let j = journal();
        let adv = advisor(&j);
        let content = "Hello world".to_string();
        let fetcher = CountingFetcher {
            content: content.clone(),
            calls: AtomicUsize::new(0),
        };
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "greeting".to_string(),
                verbatim: "Hello".to_string(), // 5 chars
            }],
            calls: AtomicUsize::new(0),
        };
        let schema = vec!["greeting".to_string()];
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com/short",
            &schema,
            100, // cap well above verbatim length
        );
        let extraction = match resp {
            Response::Extraction { extraction } => extraction,
            other => panic!("expected Extraction, got {other:?}"),
        };
        let extractions = extraction.get("extractions").and_then(|e| e.as_array()).unwrap();
        assert_eq!(
            extractions[0].get("verbatim").and_then(|v| v.as_str()),
            Some("Hello"),
            "short verbatim must be returned unchanged"
        );
        let off = extractions[0].get("char_offset").and_then(|o| o.as_array()).unwrap();
        assert_eq!(off[0].as_u64(), Some(0));
        assert_eq!(off[1].as_u64(), Some(5));
    }

    // Multibyte (Unicode) verbatim is truncated on a char boundary and does NOT panic.
    #[test]
    fn excerpt_cap_multibyte_no_panic() {
        let j = journal();
        let adv = advisor(&j);
        // "café→world": 'é' is 2 bytes, '→' is 3 bytes.
        // char sequence: c(1) a(1) f(1) é(2) →(3) w(1) o(1) r(1) l(1) d(1) = 10 chars, 14 bytes
        let content = "caf\u{00e9}\u{2192}world".to_string();
        assert_eq!(content.chars().count(), 10, "fixture sanity");
        let fetcher = CountingFetcher {
            content: content.clone(),
            calls: AtomicUsize::new(0),
        };
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "text".to_string(),
                verbatim: content.clone(), // full string
            }],
            calls: AtomicUsize::new(0),
        };
        let schema = vec!["text".to_string()];
        // Cap at 5 chars: "café→" (1+1+1+2+3 = 8 bytes)
        let cap = 5usize;
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com/multibyte",
            &schema,
            cap,
        );
        let extraction = match resp {
            Response::Extraction { extraction } => extraction,
            other => panic!("expected Extraction, got {other:?}"),
        };
        let extractions = extraction.get("extractions").and_then(|e| e.as_array()).unwrap();
        let verbatim = extractions[0].get("verbatim").and_then(|v| v.as_str()).unwrap();
        assert_eq!(
            verbatim.chars().count(),
            cap,
            "multibyte verbatim must be capped to {cap} chars"
        );
        assert!(
            content.starts_with(verbatim),
            "truncated multibyte verbatim must be a prefix of the original"
        );
        // Offset span byte-length must match.
        let off = extractions[0].get("char_offset").and_then(|o| o.as_array()).unwrap();
        let start = off[0].as_u64().unwrap() as usize;
        let end = off[1].as_u64().unwrap() as usize;
        assert_eq!(
            end - start,
            verbatim.len(),
            "char_offset span must match truncated verbatim byte-length for multibyte string"
        );
        // The truncated verbatim must be a valid substring (char boundary-safe).
        assert_eq!(&content[start..end], verbatim, "offset must index the correct substring");
    }

    // cap_chars = 0 means unlimited (never truncate).
    #[test]
    fn excerpt_cap_zero_means_unlimited() {
        let j = journal();
        let adv = advisor(&j);
        let long = "a".repeat(5000);
        let content = long.clone();
        let fetcher = CountingFetcher {
            content: content.clone(),
            calls: AtomicUsize::new(0),
        };
        let model = CountingModel {
            raws: vec![RawExtraction {
                field: "big".to_string(),
                verbatim: long.clone(),
            }],
            calls: AtomicUsize::new(0),
        };
        let schema = vec!["big".to_string()];
        let resp = run_fetch_extract(
            &j,
            &adv,
            &fetcher,
            &model,
            "claude-haiku-4-5",
            "https://example.com/unlimited",
            &schema,
            0, // 0 = no cap
        );
        let extraction = match resp {
            Response::Extraction { extraction } => extraction,
            other => panic!("expected Extraction, got {other:?}"),
        };
        let extractions = extraction.get("extractions").and_then(|e| e.as_array()).unwrap();
        let verbatim = extractions[0].get("verbatim").and_then(|v| v.as_str()).unwrap();
        assert_eq!(verbatim.len(), 5000, "cap=0 must not truncate");
    }
}
