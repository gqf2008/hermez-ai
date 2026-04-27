//! MCP input schema normalization for LLM tool-calling compatibility.
//!
//! MCP servers can emit plain JSON Schema with `definitions` /
//! `#/definitions/...` references. Kimi / Moonshot rejects that form and
//! requires local refs to point into `#/$defs/...` instead.
//!
//! Also applies provider-agnostic repairs: coerce missing/null `type`,
//! prune dangling `required` entries, ensure `properties` exists on objects.
//!
//! Mirrors Python `_normalize_mcp_input_schema()` (tools/mcp_tool.py:2281).

use serde_json::Value;

/// Normalize an MCP input schema for cross-provider compatibility.
///
/// Applies:
/// 1. Rewrite `definitions` → `$defs` and `#/definitions/...` → `#/$defs/...`
/// 2. Coerce missing/null `type` on object-shaped nodes to `"object"`
/// 3. Ensure `properties` exists on `type: object` nodes
/// 4. Prune `required` entries that don't exist in `properties`
/// 5. Strip integer/number/boolean enums (Gemini rejects them)
pub fn normalize_mcp_input_schema(schema: &Value) -> Value {
    if schema.is_null() || !schema.is_object() {
        return serde_json::json!({"type": "object", "properties": {}});
    }

    // Step 1: Rewrite definitions → $defs
    let mut normalized = rewrite_local_refs(schema);

    // Step 2-4: Repair object shapes
    normalized = repair_object_shape(&normalized);

    // Step 5: Drop enums unsupported by Gemini
    normalized = drop_unsupported_enums(&normalized);

    // Ensure top-level is a well-formed object schema
    if !normalized.is_object() {
        return serde_json::json!({"type": "object", "properties": {}});
    }
    let obj = normalized.as_object_mut().unwrap();
    if obj.get("type").and_then(Value::as_str) == Some("object")
        && !obj.contains_key("properties")
    {
        obj.insert("properties".to_string(), serde_json::json!({}));
    }

    normalized
}

/// Rewrite `definitions` → `$defs` and `#/definitions/...` → `#/$defs/...`.
fn rewrite_local_refs(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                let out_key = if key == "definitions" { "$defs" } else { key.as_str() };
                out.insert(out_key.to_string(), rewrite_local_refs(val));
            }
            if let Some(Value::String(ref_str)) = out.get("$ref") {
                if let Some(rest) = ref_str.strip_prefix("#/definitions/") {
                    out.insert("$ref".to_string(), Value::String(format!("#/$defs/{rest}")));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(rewrite_local_refs).collect()),
        other => other.clone(),
    }
}

/// Recursively repair object-shaped nodes: fill type, prune required.
fn repair_object_shape(value: &Value) -> Value {
    match value {
        Value::Array(arr) => Value::Array(arr.iter().map(repair_object_shape).collect()),
        Value::Object(map) => {
            let mut repaired = serde_json::Map::new();
            for (k, v) in map {
                repaired.insert(k.clone(), repair_object_shape(v));
            }

            // Coerce missing / null type when the shape has properties or required
            if !repaired.contains_key("type")
                && (repaired.contains_key("properties") || repaired.contains_key("required"))
            {
                repaired.insert("type".to_string(), Value::String("object".to_string()));
            }

            if repaired.get("type").and_then(Value::as_str) == Some("object") {
                // Ensure properties exists
                if !repaired.contains_key("properties")
                    || !repaired.get("properties").map_or(false, |v| v.is_object())
                {
                    repaired.insert("properties".to_string(), serde_json::json!({}));
                }

                // Prune required to only include names that exist in properties
                if let Some(Value::Array(required)) = repaired.get("required") {
                    let props = repaired
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|p| p.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default();
                    let valid: Vec<Value> = required
                        .iter()
                        .filter(|r| {
                            r.as_str().map_or(false, |s| props.iter().any(|p| p == s))
                        })
                        .cloned()
                        .collect();
                    if valid.len() != required.len() {
                        if valid.is_empty() {
                            repaired.remove("required");
                        } else {
                            repaired.insert("required".to_string(), Value::Array(valid));
                        }
                    }
                }
            }

            Value::Object(repaired)
        }
        other => other.clone(),
    }
}

/// Drop integer/number/boolean enums — Gemini rejects them.
/// Mirrors Python _sanitize_gemini_tool_parameters() (gemini_schema.py).
fn drop_unsupported_enums(value: &Value) -> Value {
    match value {
        Value::Array(arr) => Value::Array(arr.iter().map(drop_unsupported_enums).collect()),
        Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();
            for (k, v) in map {
                if k == "enum" {
                    // Only keep enum if all values are strings
                    if let Value::Array(items) = v {
                        let all_strings = items.iter().all(|i| i.is_string());
                        if all_strings {
                            cleaned.insert(k.clone(), v.clone());
                        }
                        // else: drop the enum entirely
                    }
                } else {
                    cleaned.insert(k.clone(), drop_unsupported_enums(v));
                }
            }
            Value::Object(cleaned)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_definitions_to_defs() {
        let input = serde_json::json!({
            "type": "object",
            "definitions": {
                "Foo": {"type": "string"}
            },
            "properties": {
                "bar": {"$ref": "#/definitions/Foo"}
            }
        });
        let result = normalize_mcp_input_schema(&input);
        assert!(result.to_string().contains("$defs"));
        assert!(!result.to_string().contains("\"definitions\""));
        assert!(result.to_string().contains("#/$defs/Foo"));
    }

    #[test]
    fn test_repair_missing_type() {
        let input = serde_json::json!({
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        });
        let result = normalize_mcp_input_schema(&input);
        assert_eq!(result["type"], "object");
    }

    #[test]
    fn test_prune_dangling_required() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name", "missing_field"]
        });
        let result = normalize_mcp_input_schema(&input);
        let required: Vec<_> = result["required"].as_array().unwrap().iter()
            .filter_map(|v| v.as_str()).collect();
        assert_eq!(required, vec!["name"]);
    }

    #[test]
    fn test_drop_numeric_enum() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer", "enum": [1, 2, 3]}
            }
        });
        let result = normalize_mcp_input_schema(&input);
        let prop = &result["properties"]["count"];
        assert!(prop.get("enum").is_none());
    }

    #[test]
    fn test_keep_string_enum() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "color": {"type": "string", "enum": ["red", "green", "blue"]}
            }
        });
        let result = normalize_mcp_input_schema(&input);
        let prop = &result["properties"]["color"];
        assert!(prop.get("enum").is_some());
    }

    #[test]
    fn test_null_schema_returns_object() {
        let result = normalize_mcp_input_schema(&Value::Null);
        assert_eq!(result["type"], "object");
    }
}
