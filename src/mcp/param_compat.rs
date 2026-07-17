//! Parameter compatibility helpers for LLM tool calls.
//!
//! LLMs sometimes stringify complex parameters (arrays, objects) instead of
//! sending proper JSON. These helpers accept both formats and return clear
//! errors when neither works.

use rmcp::ErrorData as McpError;
use serde_json::Value;

/// Parse a parameter that should be `Vec<String>` but might be stringified.
///
/// Accepts:
/// - `["a", "b"]` (proper JSON array)
/// - `"[\"a\", \"b\"]"` (stringified JSON array)
///
/// Returns `McpError::invalid_params` on failure with a descriptive message.
pub fn parse_string_vec(value: &Value) -> Result<Vec<String>, McpError> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        McpError::invalid_params("array element is not a string", None)
                    })
            })
            .collect(),
        Value::String(s) => serde_json::from_str::<Vec<String>>(s).map_err(|e| {
            McpError::invalid_params(
                format!("string is not a valid JSON array: {e}"),
                None,
            )
        }),
        _ => Err(McpError::invalid_params(
            "expected array or JSON string",
            None,
        )),
    }
}

/// Parse a parameter that should be a JSON object but might be stringified.
///
/// Accepts:
/// - `{"key": "value"}` (proper JSON object)
/// - `"{\"key\": \"value\"}"` (stringified JSON)
///
/// Returns `McpError::invalid_params` on failure.
pub fn parse_json_object(value: &Value) -> Result<Value, McpError> {
    match value {
        Value::Object(_) => Ok(value.clone()),
        Value::String(s) => {
            let parsed: Value = serde_json::from_str(s).map_err(|e| {
                McpError::invalid_params(format!("string is not valid JSON: {e}"), None)
            })?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Err(McpError::invalid_params(
                    "parsed JSON is not an object",
                    None,
                ))
            }
        }
        _ => Err(McpError::invalid_params(
            "expected object or JSON string",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_string_vec_from_array() {
        let v = json!(["a", "b", "c"]);
        let result = parse_string_vec(&v).unwrap();
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_string_vec_from_stringify() {
        let v = json!("[\"a\", \"b\"]");
        let result = parse_string_vec(&v).unwrap();
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn parse_string_vec_empty() {
        let v = json!([]);
        let result = parse_string_vec(&v).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_string_vec_error_on_number() {
        let v = json!(42);
        assert!(parse_string_vec(&v).is_err());
    }

    #[test]
    fn parse_string_vec_error_on_bad_stringify() {
        let v = json!("not a json array");
        assert!(parse_string_vec(&v).is_err());
    }

    #[test]
    fn parse_string_vec_error_on_mixed_array() {
        let v = json!(["a", 42]);
        assert!(parse_string_vec(&v).is_err());
    }

    #[test]
    fn parse_json_object_from_object() {
        let v = json!({"total": 6, "valid": 6});
        let result = parse_json_object(&v).unwrap();
        assert_eq!(result["total"], 6);
    }

    #[test]
    fn parse_json_object_from_stringify() {
        let v = json!("{\"total\": 6}");
        let result = parse_json_object(&v).unwrap();
        assert_eq!(result["total"], 6);
    }

    #[test]
    fn parse_json_object_error_on_stringify_array() {
        let v = json!("[1, 2]");
        assert!(parse_json_object(&v).is_err());
    }

    #[test]
    fn parse_json_object_error_on_number() {
        let v = json!(42);
        assert!(parse_json_object(&v).is_err());
    }
}
