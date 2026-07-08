use crate::ShimError;
use serde_json::json;

/// A raw extraction as returned by the model: a verbatim substring per field,
/// with NO offset. The model's only job is to copy the exact substring; the
/// daemon locates it in the content (see [`locate_offsets`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RawExtraction {
    pub field: String,
    pub verbatim: String,
}

/// A final extracted field with verbatim text and daemon-located byte offsets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ExtractionField {
    pub field: String,
    pub verbatim: String,
    pub char_offset: [usize; 2],
}

/// Trait for extraction models.
pub trait ExtractionModel {
    /// Extract fields from content given schema field names.
    /// Returns one [`RawExtraction`] (verbatim only, no offset) per field the
    /// model could fill. The daemon locates each verbatim afterward.
    fn extract(&self, content: &str, schema_fields: &[String]) -> Result<Vec<RawExtraction>, ShimError>;
}

/// Mock extraction model for tests.
pub struct MockExtractionModel {
    pub raws: Vec<RawExtraction>,
}

impl ExtractionModel for MockExtractionModel {
    fn extract(&self, _content: &str, _schema_fields: &[String]) -> Result<Vec<RawExtraction>, ShimError> {
        Ok(self.raws.clone())
    }
}

/// Locate each model-returned verbatim in the fetched content and compute its
/// byte offset. A verbatim that does not occur in the content is REJECTED — a
/// hallucinated quote is not in the page (ADR-005). An empty verbatim is
/// treated as not-found (we never emit a zero-length span).
///
/// The first occurrence is used (sufficient for M5).
pub fn locate_offsets(
    content: &str,
    raws: &[RawExtraction],
) -> Result<Vec<ExtractionField>, ShimError> {
    let mut fields = Vec::with_capacity(raws.len());
    for raw in raws {
        if raw.verbatim.is_empty() {
            return Err(ShimError::VerbatimNotFound {
                field: raw.field.clone(),
            });
        }
        match content.find(&raw.verbatim) {
            Some(byte_pos) => fields.push(ExtractionField {
                field: raw.field.clone(),
                verbatim: raw.verbatim.clone(),
                char_offset: [byte_pos, byte_pos + raw.verbatim.len()],
            }),
            None => {
                return Err(ShimError::VerbatimNotFound {
                    field: raw.field.clone(),
                });
            }
        }
    }
    Ok(fields)
}

/// Anthropic extraction model.
pub struct AnthropicExtractionModel {
    model: String,
    base_url_override: Option<String>,
}

impl AnthropicExtractionModel {
    pub fn new(model: String, base_url: Option<String>) -> Self {
        AnthropicExtractionModel {
            model,
            base_url_override: base_url,
        }
    }
}

impl ExtractionModel for AnthropicExtractionModel {
    fn extract(&self, content: &str, schema_fields: &[String]) -> Result<Vec<RawExtraction>, ShimError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ShimError::ModelUnavailable("ANTHROPIC_API_KEY not set".to_string()))?;

        let base_url = self.base_url_override.clone().or_else(|| {
            std::env::var("ANTHROPIC_BASE_URL").ok()
        }).unwrap_or_else(|| "https://api.anthropic.com".to_string());

        let body = build_extract_body(&self.model, content, schema_fields);

        let response = ureq::post(&format!("{}/v1/messages", base_url))
            .set("x-api-key", &api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(&body)
            .map_err(|e| ShimError::Http(e.to_string()))?;

        let response_str = response.into_string()
            .map_err(|e| ShimError::Http(e.to_string()))?;

        let response_json: serde_json::Value = serde_json::from_str(&response_str)
            .map_err(|e| ShimError::Http(e.to_string()))?;

        // Look for tool_use block with emit_extractions
        let content = response_json
            .get("content")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ShimError::Protocol("missing content array".to_string()))?;

        for block in content {
            if let Some("tool_use") = block.get("type").and_then(|v| v.as_str()) {
                if let Some("emit_extractions") = block.get("name").and_then(|v| v.as_str()) {
                    let input = block.get("input")
                        .ok_or_else(|| ShimError::Protocol("missing tool input".to_string()))?;

                    let extractions = input
                        .get("extractions")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| ShimError::Protocol("missing extractions array".to_string()))?;

                    let mut raws = Vec::new();
                    for extraction in extractions {
                        let field = extraction
                            .get("field")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| ShimError::Protocol("missing field name".to_string()))?
                            .to_string();

                        let verbatim = extraction
                            .get("verbatim")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| ShimError::Protocol("missing verbatim".to_string()))?
                            .to_string();

                        raws.push(RawExtraction { field, verbatim });
                    }

                    return Ok(raws);
                }
            }
        }

        Err(ShimError::Protocol("no emit_extractions tool call found".to_string()))
    }
}

/// Build the request body for the extraction model.
/// Pure function for unit testing (no network).
pub fn build_extract_body(model: &str, content: &str, schema_fields: &[String]) -> serde_json::Value {
    let fields_list = schema_fields.join(", ");

    json!({
        "model": model,
        "max_tokens": 2000,
        "system": "You extract VERBATIM spans only, never summarize or paraphrase. For each requested field, return the EXACT verbatim substring copied character-for-character from the provided content. Do NOT report positions or byte counts — return only the exact text.",
        "tools": [
            {
                "name": "emit_extractions",
                "description": "Emit extracted fields with the exact verbatim text copied from the content",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "extractions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "field": {
                                        "type": "string",
                                        "description": "Schema field name"
                                    },
                                    "verbatim": {
                                        "type": "string",
                                        "description": "Exact substring copied verbatim from content"
                                    }
                                },
                                "required": ["field", "verbatim"]
                            }
                        }
                    },
                    "required": ["extractions"]
                }
            }
        ],
        "messages": [
            {
                "role": "user",
                "content": format!("Extract the following fields from the content below. For each field return ONLY the exact verbatim substring copied from the content — no positions, no byte counts, no summaries.\n\nFields: {}\n\nContent:\n{}", fields_list, content)
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_extraction_model() {
        let canned = vec![RawExtraction {
            field: "title".to_string(),
            verbatim: "Example Title".to_string(),
        }];

        let model = MockExtractionModel {
            raws: canned.clone(),
        };

        let results = model.extract("Example Title content", &["title".to_string()]).unwrap();
        assert_eq!(results, canned);
    }

    #[test]
    fn test_locate_offsets_present() {
        let content = "Hello world";
        let raws = vec![RawExtraction {
            field: "target".to_string(),
            verbatim: "world".to_string(),
        }];
        let fields = locate_offsets(content, &raws).unwrap();
        assert_eq!(fields.len(), 1);
        let off = fields[0].char_offset;
        assert_eq!(off, [6, 11]);
        assert_eq!(&content[off[0]..off[1]], "world");
        assert_eq!(fields[0].field, "target");
        assert_eq!(fields[0].verbatim, "world");
    }

    #[test]
    fn test_locate_offsets_absent_is_rejected() {
        let content = "Hello world";
        let raws = vec![RawExtraction {
            field: "greeting".to_string(),
            verbatim: "Goodbye".to_string(),
        }];
        let err = locate_offsets(content, &raws).unwrap_err();
        match err {
            ShimError::VerbatimNotFound { field } => assert_eq!(field, "greeting"),
            other => panic!("expected VerbatimNotFound, got {other}"),
        }
    }

    #[test]
    fn test_locate_offsets_multiple_fields() {
        let content = "Hello world";
        let raws = vec![
            RawExtraction {
                field: "greeting".to_string(),
                verbatim: "Hello".to_string(),
            },
            RawExtraction {
                field: "target".to_string(),
                verbatim: "world".to_string(),
            },
        ];
        let fields = locate_offsets(content, &raws).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].char_offset, [0, 5]);
        assert_eq!(fields[1].char_offset, [6, 11]);
        for f in &fields {
            assert_eq!(&content[f.char_offset[0]..f.char_offset[1]], f.verbatim);
        }
    }

    #[test]
    fn test_locate_offsets_empty_verbatim_rejected() {
        let content = "Hello world";
        let raws = vec![RawExtraction {
            field: "empty".to_string(),
            verbatim: String::new(),
        }];
        let err = locate_offsets(content, &raws).unwrap_err();
        match err {
            ShimError::VerbatimNotFound { field } => assert_eq!(field, "empty"),
            other => panic!("expected VerbatimNotFound, got {other}"),
        }
    }

    #[test]
    fn test_locate_offsets_first_absent_rejects_before_second() {
        let content = "Hello world";
        let raws = vec![
            RawExtraction {
                field: "bad".to_string(),
                verbatim: "nope".to_string(),
            },
            RawExtraction {
                field: "good".to_string(),
                verbatim: "world".to_string(),
            },
        ];
        let err = locate_offsets(content, &raws).unwrap_err();
        match err {
            ShimError::VerbatimNotFound { field } => assert_eq!(field, "bad"),
            other => panic!("expected VerbatimNotFound, got {other}"),
        }
    }

    #[test]
    fn test_anthropic_no_api_key() {
        // Remove API key if it exists
        let old_key = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let model = AnthropicExtractionModel::new("claude-haiku-4-5".to_string(), None);
        let result = model.extract("test content", &["field".to_string()]);

        match result {
            Err(ShimError::ModelUnavailable(msg)) => {
                assert!(msg.contains("ANTHROPIC_API_KEY"));
            }
            _ => panic!("Expected ModelUnavailable error"),
        }

        // Restore API key if it was set
        if let Some(key) = old_key {
            std::env::set_var("ANTHROPIC_API_KEY", key);
        }
    }

    #[test]
    fn test_build_extract_body_structure() {
        let body = build_extract_body("claude-haiku-4-5", "test content", &["title".to_string()]);

        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("claude-haiku-4-5"));
        assert_eq!(body.get("max_tokens").and_then(|v| v.as_u64()), Some(2000));

        let tools = body.get("tools").and_then(|v| v.as_array()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].get("name").and_then(|v| v.as_str()), Some("emit_extractions"));

        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        let content = messages[0].get("content").and_then(|v| v.as_str()).unwrap();
        assert!(content.contains("test content"));
        assert!(content.contains("title"));

        // The verbatim-only contract must not mention offsets/positions anywhere.
        assert!(!content.to_lowercase().contains("offset"));
        let system = body.get("system").and_then(|v| v.as_str()).unwrap();
        assert!(!system.to_lowercase().contains("offset"));
    }

    #[test]
    fn test_build_extract_body_tool_schema() {
        let body = build_extract_body("model", "content", &["field1".to_string(), "field2".to_string()]);

        let tools = body.get("tools").and_then(|v| v.as_array()).unwrap();
        let input_schema = tools[0].get("input_schema").unwrap();

        assert_eq!(input_schema.get("type").and_then(|v| v.as_str()), Some("object"));

        let properties = input_schema.get("properties").unwrap();
        assert!(properties.get("extractions").is_some());

        // The emit_extractions item schema has field + verbatim only (no char_offset).
        let item = properties
            .get("extractions")
            .and_then(|e| e.get("items"))
            .unwrap();
        let item_props = item.get("properties").unwrap();
        assert!(item_props.get("field").is_some());
        assert!(item_props.get("verbatim").is_some());
        assert!(
            item_props.get("char_offset").is_none(),
            "verbatim-only contract must not include char_offset"
        );
        let required = item.get("required").and_then(|r| r.as_array()).unwrap();
        assert!(!required.iter().any(|v| v.as_str() == Some("char_offset")));
    }
}
