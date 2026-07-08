use crate::ShimError;
use serde_json::json;
use std::time::SystemTime;

/// Search result from a search backend: metadata only, no model involvement.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SearchResult {
    pub query: String,
    pub url: String,
    pub title: String,
    pub engine_snippet: String,
    pub rank: u32,
    pub retrieved_at: String,
}

/// Trait for pluggable search backends.
pub trait SearchBackend {
    fn search(&self, queries: &[String]) -> Result<Vec<SearchResult>, ShimError>;
}

/// A best-effort RFC-3339 timestamp for "now" (UTC). Shared by the backends so
/// each [`SearchResult`] carries a `retrieved_at`.
fn now_rfc3339() -> String {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => {
            let secs = duration.as_secs();
            let nanos = duration.subsec_nanos();
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                1970 + secs / (365 * 24 * 3600),
                1,
                1,
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60,
                nanos / 1_000_000
            )
        }
        Err(_) => "1970-01-01T00:00:00Z".to_string(),
    }
}

/// SearXNG backend.
pub struct SearxngBackend {
    pub endpoint: Option<String>,
}

impl SearxngBackend {
    pub fn new(endpoint: Option<String>) -> Self {
        SearxngBackend { endpoint }
    }

    fn percent_encode(s: &str) -> String {
        s.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u8)
                }
            })
            .collect()
    }

    fn now_rfc3339() -> String {
        now_rfc3339()
    }
}

impl SearchBackend for SearxngBackend {
    fn search(&self, queries: &[String]) -> Result<Vec<SearchResult>, ShimError> {
        let endpoint = match &self.endpoint {
            Some(ep) => ep,
            None => {
                return Err(ShimError::BackendUnavailable(
                    "no search backend configured".to_string(),
                ))
            }
        };

        let mut results = Vec::new();
        let timestamp = Self::now_rfc3339();

        for query in queries {
            let encoded = Self::percent_encode(query);
            let url = format!("{}/search?q={}&format=json", endpoint, encoded);

            let response = ureq::get(&url)
                .call()
                .map_err(|e| ShimError::Http(e.to_string()))?
                .into_string()
                .map_err(|e| ShimError::Http(e.to_string()))?;

            let json: serde_json::Value =
                serde_json::from_str(&response).map_err(|e| ShimError::Http(e.to_string()))?;

            let search_results = json
                .get("results")
                .and_then(|v| v.as_array())
                .ok_or_else(|| ShimError::Protocol("missing 'results' array".to_string()))?;

            for (rank, result) in search_results.iter().enumerate() {
                let url = result
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let title = result
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let engine_snippet = result
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                results.push(SearchResult {
                    query: query.clone(),
                    url,
                    title,
                    engine_snippet,
                    rank: (rank + 1) as u32,
                    retrieved_at: timestamp.clone(),
                });
            }
        }

        Ok(results)
    }
}

/// Anthropic web-search backend (ADR-005 `search.backend = "anthropic"`, the
/// default). Uses Anthropic's server-side `web_search` tool: a cheap Tier-0
/// model issues the query, Anthropic runs the search on its own infrastructure
/// and returns the results in the SAME response (no client tool-execution loop).
///
/// The tool is declared with `"allowed_callers": ["direct"]` so that models
/// which do not support programmatic tool calling (e.g. `claude-haiku-4-5`) can
/// still invoke it. The tool is executed directly server-side; no client-side
/// tool-execution round-trip occurs.
///
/// Only the raw result **metadata (url + title)** is surfaced — the page content
/// Anthropic returns is encrypted for model citation, so there is no plain-text
/// `engine_snippet` on this backend (`engine_snippet` is left empty). The query
/// passes through a model but no model prose reaches the advisor.
///
/// Non-eager: the API key is read from `ANTHROPIC_API_KEY` at call time (a
/// missing key → [`ShimError::BackendUnavailable`]), and the base-url follows
/// the same precedence as [`crate::AnthropicExtractionModel`]: explicit override
/// → `$ANTHROPIC_BASE_URL` → `https://api.anthropic.com`.
pub struct AnthropicSearchBackend {
    model: String,
    base_url_override: Option<String>,
}

impl AnthropicSearchBackend {
    /// Construct the backend. `model` is the (cheap) model issuing the query —
    /// the default caller passes `"claude-haiku-4-5"`. `base_url` overrides the
    /// API base when `Some`. No network or env access happens here.
    pub fn new(model: String, base_url: Option<String>) -> Self {
        AnthropicSearchBackend {
            model,
            base_url_override: base_url,
        }
    }
}

impl SearchBackend for AnthropicSearchBackend {
    fn search(&self, queries: &[String]) -> Result<Vec<SearchResult>, ShimError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            ShimError::BackendUnavailable(
                "no ANTHROPIC_API_KEY for anthropic web search".to_string(),
            )
        })?;

        let base_url = self
            .base_url_override
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());

        let mut results = Vec::new();
        for query in queries {
            let body = build_search_body(&self.model, query);

            let response = ureq::post(&format!("{}/v1/messages", base_url))
                .set("x-api-key", &api_key)
                .set("anthropic-version", "2023-06-01")
                .set("content-type", "application/json")
                .send_json(&body)
                .map_err(|e| ShimError::Http(e.to_string()))?;

            let response_str = response
                .into_string()
                .map_err(|e| ShimError::Http(e.to_string()))?;

            let response_json: serde_json::Value = serde_json::from_str(&response_str)
                .map_err(|e| ShimError::Protocol(e.to_string()))?;

            results.extend(parse_search_results(query, &response_json)?);
        }

        Ok(results)
    }
}

/// Build the `/v1/messages` request body for an Anthropic web search (ADR-005).
/// Pure function for unit testing (no network). Declares the server-side
/// `web_search_20260209` tool and instructs the model to run the query without
/// answering in prose.
///
/// The tool object carries `"allowed_callers": ["direct"]` because the cheap
/// shim model (Haiku) does not support programmatic tool calling. Setting
/// `allowed_callers` to `["direct"]` tells Anthropic's infrastructure to invoke
/// the tool directly server-side and return the results in the same response,
/// which is exactly the behaviour we need (no client tool-execution loop).
/// Without this field the API returns HTTP 400 for models that lack programmatic
/// tool-calling support.
pub fn build_search_body(model: &str, query: &str) -> serde_json::Value {
    json!({
        "model": model,
        "max_tokens": 1024,
        "tools": [
            {
                "type": "web_search_20260209",
                "name": "web_search",
                "max_uses": 1,
                "allowed_callers": ["direct"]
            }
        ],
        "tool_choice": { "type": "any" },
        "messages": [
            {
                "role": "user",
                "content": format!(
                    "Run a web search for the following query and do not answer in prose: {}",
                    query
                )
            }
        ]
    })
}

/// Parse the `web_search_tool_result` blocks of an Anthropic messages response
/// into [`SearchResult`]s (ADR-005). Pure function for unit testing (no network).
///
/// Each `web_search_result` block becomes one [`SearchResult`] with a 1-based
/// `rank` within this query; `engine_snippet` is left empty (this backend has no
/// plain-text snippet — the page content is encrypted for the model), and the
/// opaque `encrypted_content` is ignored. A `web_search_tool_result_error`
/// content object is surfaced as an error.
pub fn parse_search_results(
    query: &str,
    response: &serde_json::Value,
) -> Result<Vec<SearchResult>, ShimError> {
    let content = response
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ShimError::Protocol("missing content array".to_string()))?;

    let timestamp = now_rfc3339();
    let mut results = Vec::new();

    for block in content {
        if block.get("type").and_then(|v| v.as_str()) != Some("web_search_tool_result") {
            continue;
        }

        let inner = block.get("content").ok_or_else(|| {
            ShimError::Protocol("web_search_tool_result missing content".to_string())
        })?;

        // The tool result may carry an error object instead of an array of
        // results — surface it loudly rather than returning nothing.
        if inner.get("type").and_then(|v| v.as_str()) == Some("web_search_tool_result_error") {
            let code = inner
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(ShimError::Http(format!(
                "anthropic web_search error: {code}"
            )));
        }

        let items = inner.as_array().ok_or_else(|| {
            ShimError::Protocol("web_search_tool_result content is not an array".to_string())
        })?;

        for item in items {
            if item.get("type").and_then(|v| v.as_str()) != Some("web_search_result") {
                continue;
            }
            let url = item
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            results.push(SearchResult {
                query: query.to_string(),
                url,
                title,
                // No plain-text snippet on this backend — content is encrypted.
                engine_snippet: String::new(),
                rank: (results.len() + 1) as u32,
                retrieved_at: timestamp.clone(),
            });
        }
    }

    Ok(results)
}

/// Mock search backend for tests.
pub struct MockSearchBackend {
    pub results: Vec<SearchResult>,
}

impl SearchBackend for MockSearchBackend {
    fn search(&self, _queries: &[String]) -> Result<Vec<SearchResult>, ShimError> {
        Ok(self.results.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_searxng_backend_no_endpoint() {
        let backend = SearxngBackend::new(None);
        let result = backend.search(&["test".to_string()]);

        match result {
            Err(ShimError::BackendUnavailable(msg)) => {
                assert!(msg.contains("no search backend configured"));
            }
            _ => panic!("Expected BackendUnavailable error"),
        }
    }

    #[test]
    fn test_mock_search_backend() {
        let canned = vec![SearchResult {
            query: "test".to_string(),
            url: "http://example.com".to_string(),
            title: "Example".to_string(),
            engine_snippet: "An example page".to_string(),
            rank: 1,
            retrieved_at: "2025-01-01T00:00:00Z".to_string(),
        }];

        let backend = MockSearchBackend {
            results: canned.clone(),
        };

        let results = backend.search(&["test".to_string()]).unwrap();
        assert_eq!(results, canned);
    }

    #[test]
    fn test_percent_encode() {
        assert_eq!(SearxngBackend::percent_encode("hello"), "hello");
        assert_eq!(SearxngBackend::percent_encode("hello world"), "hello%20world");
        assert_eq!(SearxngBackend::percent_encode("a&b=c"), "a%26b%3Dc");
    }

    // AC4: with ANTHROPIC_API_KEY unset, the anthropic backend is unavailable
    // (no network, structured error). Env is saved/restored around the test.
    #[test]
    fn test_anthropic_search_no_api_key() {
        let old_key = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let backend = AnthropicSearchBackend::new("claude-haiku-4-5".to_string(), None);
        let result = backend.search(&["x".to_string()]);

        match result {
            Err(ShimError::BackendUnavailable(msg)) => {
                assert!(
                    msg.contains("no ANTHROPIC_API_KEY"),
                    "message must mention the missing key: {msg}"
                );
            }
            other => panic!("expected BackendUnavailable, got {other:?}"),
        }

        if let Some(key) = old_key {
            std::env::set_var("ANTHROPIC_API_KEY", key);
        }
    }

    // AC4: the request body declares the web_search tool and carries the query.
    #[test]
    fn test_build_search_body_structure() {
        let body = build_search_body("claude-haiku-4-5", "rust ownership");

        assert_eq!(
            body.get("model").and_then(|v| v.as_str()),
            Some("claude-haiku-4-5")
        );
        assert_eq!(body.get("max_tokens").and_then(|v| v.as_u64()), Some(1024));

        let tools = body.get("tools").and_then(|v| v.as_array()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("type").and_then(|v| v.as_str()),
            Some("web_search_20260209")
        );
        assert_eq!(
            tools[0].get("name").and_then(|v| v.as_str()),
            Some("web_search")
        );
        // `allowed_callers: ["direct"]` is required for models that do not
        // support programmatic tool calling (e.g. claude-haiku-4-5).
        let allowed_callers = tools[0]
            .get("allowed_callers")
            .and_then(|v| v.as_array())
            .expect("tool must have allowed_callers array");
        assert_eq!(allowed_callers.len(), 1);
        assert_eq!(
            allowed_callers[0].as_str(),
            Some("direct"),
            "allowed_callers must contain exactly \"direct\""
        );

        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        let content = messages[0].get("content").and_then(|v| v.as_str()).unwrap();
        assert!(
            content.contains("rust ownership"),
            "user message must contain the query: {content}"
        );
    }

    // AC5: two web_search_result entries parse into two SearchResults with the
    // right url/title, empty engine_snippet, and 1-based ranks.
    #[test]
    fn test_parse_search_results_two_entries() {
        let response = json!({
            "content": [
                { "type": "text", "text": "ignored prose" },
                {
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_1",
                    "content": [
                        {
                            "type": "web_search_result",
                            "url": "https://a.example/one",
                            "title": "First Result",
                            "page_age": "1 day",
                            "encrypted_content": "opaque-blob-1"
                        },
                        {
                            "type": "web_search_result",
                            "url": "https://b.example/two",
                            "title": "Second Result",
                            "page_age": "2 days",
                            "encrypted_content": "opaque-blob-2"
                        }
                    ]
                }
            ]
        });

        let results = parse_search_results("q", &response).unwrap();
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].query, "q");
        assert_eq!(results[0].url, "https://a.example/one");
        assert_eq!(results[0].title, "First Result");
        assert_eq!(results[0].engine_snippet, "");
        assert_eq!(results[0].rank, 1);

        assert_eq!(results[1].url, "https://b.example/two");
        assert_eq!(results[1].title, "Second Result");
        assert_eq!(results[1].engine_snippet, "");
        assert_eq!(results[1].rank, 2);
    }

    // AC5: a web_search_tool_result_error content object → Err.
    #[test]
    fn test_parse_search_results_error_object() {
        let response = json!({
            "content": [
                {
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_1",
                    "content": {
                        "type": "web_search_tool_result_error",
                        "error_code": "max_uses_exceeded"
                    }
                }
            ]
        });

        let err = parse_search_results("q", &response).unwrap_err();
        match err {
            ShimError::Http(msg) => assert!(
                msg.contains("max_uses_exceeded"),
                "error must carry the error_code: {msg}"
            ),
            other => panic!("expected Http error, got {other:?}"),
        }
    }
}
