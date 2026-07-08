//! maestro-shim: Search/fetch/extract ENGINE
//!
//! Pure engine for web search, content fetching, readability extraction, and field extraction.
//! No network calls in tests; all traits are mockable.

mod error;
mod extraction;
mod fetch;
mod html;
mod search;
mod validation;

pub use error::ShimError;
pub use extraction::{
    AnthropicExtractionModel, ExtractionModel, ExtractionField, MockExtractionModel, RawExtraction,
    build_extract_body, locate_offsets,
};
pub use fetch::fetch;
pub use html::html_to_text;
pub use search::{
    build_search_body, parse_search_results, AnthropicSearchBackend, MockSearchBackend,
    SearchBackend, SearchResult, SearxngBackend,
};
pub use validation::validate_offsets;

/// Content hash type: `sha256:<hex>` over the sorted, joined field names
pub fn schema_hash(schema_fields: &[String]) -> String {
    use sha2::{Sha256, Digest};

    let mut fields = schema_fields.to_vec();
    fields.sort();
    let joined = fields.join("|");

    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    let result = hasher.finalize();

    format!("sha256:{:x}", result)
}

/// Main extraction result returned to the daemon
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Extraction {
    pub url: String,
    pub retrieved_at: String,
    pub content_digest: String,
    pub extractions: Vec<ExtractionField>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_hash_deterministic() {
        let fields = vec!["title".to_string(), "author".to_string()];
        let hash1 = schema_hash(&fields);
        let hash2 = schema_hash(&fields);
        assert_eq!(hash1, hash2);
        assert!(hash1.starts_with("sha256:"));
    }

    #[test]
    fn test_schema_hash_order_independent() {
        let fields1 = vec!["title".to_string(), "author".to_string()];
        let fields2 = vec!["author".to_string(), "title".to_string()];
        assert_eq!(schema_hash(&fields1), schema_hash(&fields2));
    }
}
