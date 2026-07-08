use crate::{ExtractionField, ShimError};

/// Validate that each field's `char_offset` correctly points to its `verbatim` in the content.
///
/// For each field, checks:
/// - Byte offset bounds are within the string
/// - Offsets are at valid char boundaries
/// - The slice at [start..end] equals the claimed verbatim
///
/// Returns Err if ANY field fails validation.
pub fn validate_offsets(content: &str, fields: &[ExtractionField]) -> Result<(), ShimError> {
    for field in fields {
        let [start, end] = field.char_offset;

        // Check bounds
        if start > end || end > content.len() {
            return Err(ShimError::FabricatedOffset {
                field: field.field.clone(),
                start,
                end,
            });
        }

        // Check char boundaries
        if !content.is_char_boundary(start) || !content.is_char_boundary(end) {
            return Err(ShimError::FabricatedOffset {
                field: field.field.clone(),
                start,
                end,
            });
        }

        // Check content matches
        let actual = &content[start..end];
        if actual != field.verbatim {
            return Err(ShimError::FabricatedOffset {
                field: field.field.clone(),
                start,
                end,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_offsets_correct() {
        let content = "Hello world";
        let fields = vec![ExtractionField {
            field: "greeting".to_string(),
            verbatim: "Hello".to_string(),
            char_offset: [0, 5],
        }];

        assert!(validate_offsets(content, &fields).is_ok());
    }

    #[test]
    fn test_validate_offsets_wrong_text() {
        let content = "Hello world";
        let fields = vec![ExtractionField {
            field: "greeting".to_string(),
            verbatim: "Goodbye".to_string(), // wrong
            char_offset: [0, 5],
        }];

        let err = validate_offsets(content, &fields).unwrap_err();
        match err {
            ShimError::FabricatedOffset { field, start, end } => {
                assert_eq!(field, "greeting");
                assert_eq!(start, 0);
                assert_eq!(end, 5);
            }
            _ => panic!("Expected FabricatedOffset, got {}", err),
        }
    }

    #[test]
    fn test_validate_offsets_out_of_bounds() {
        let content = "Hello world";
        let fields = vec![ExtractionField {
            field: "greeting".to_string(),
            verbatim: "Hello".to_string(),
            char_offset: [0, 100],
        }];

        let err = validate_offsets(content, &fields).unwrap_err();
        match err {
            ShimError::FabricatedOffset { .. } => (),
            _ => panic!("Expected FabricatedOffset"),
        }
    }

    #[test]
    fn test_validate_offsets_char_boundary() {
        let content = "Hello 世界"; // "世" starts at byte 6, "界" at byte 9

        // Valid boundary at ASCII "Hello " (6 bytes)
        let fields = vec![ExtractionField {
            field: "text".to_string(),
            verbatim: "Hello ".to_string(),
            char_offset: [0, 6],
        }];
        assert!(validate_offsets(content, &fields).is_ok());

        // Invalid: middle of a UTF-8 character
        let fields = vec![ExtractionField {
            field: "text".to_string(),
            verbatim: "x".to_string(),
            char_offset: [7, 8], // middle of "世"
        }];
        assert!(validate_offsets(content, &fields).is_err());
    }

    #[test]
    fn test_validate_offsets_multiple_fields() {
        let content = "Hello world";
        let fields = vec![
            ExtractionField {
                field: "greeting".to_string(),
                verbatim: "Hello".to_string(),
                char_offset: [0, 5],
            },
            ExtractionField {
                field: "target".to_string(),
                verbatim: "world".to_string(),
                char_offset: [6, 11],
            },
        ];

        assert!(validate_offsets(content, &fields).is_ok());
    }

    #[test]
    fn test_validate_offsets_first_field_fails() {
        let content = "Hello world";
        let fields = vec![
            ExtractionField {
                field: "greeting".to_string(),
                verbatim: "Wrong".to_string(),
                char_offset: [0, 5],
            },
            ExtractionField {
                field: "target".to_string(),
                verbatim: "world".to_string(),
                char_offset: [6, 11],
            },
        ];

        let err = validate_offsets(content, &fields).unwrap_err();
        match err {
            ShimError::FabricatedOffset { field, .. } => {
                assert_eq!(field, "greeting");
            }
            _ => panic!("Expected FabricatedOffset"),
        }
    }
}
