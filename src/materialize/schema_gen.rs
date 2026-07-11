//! JSON Schema generation for materialized agent configs.
//!
//! Adapters with typed output structs can derive JSON Schema from the same
//! structs that drive materialization. The resulting schema is emitted as a
//! sidecar file (`opencode.schema.json`, etc.) so IDEs and validators can
//! check materialized configs against a formal schema.
//!
//! # Safety
//! Generated schemas use `"additionalProperties": true` at the root so user
//! passthrough keys (e.g. `native.opencode` overlay keys not modeled by the
//! typed structs) never fail validation.

use serde_json::{Value, json};

/// Wrap a schemars-generated schema value with `"additionalProperties": true`
/// at the root, so user passthrough keys never fail validation.
///
/// `schemars::schema_for!(T)` produces a valid draft 2020-12 JSON Schema
/// document. This function adds the root-level passthrough tolerance.
///
/// Returns the input schema unchanged (except for the inserted key) — it is
/// an idempotent wrapper, not a schema constructor.
#[must_use]
pub fn with_root_additional_properties(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        obj.insert("additionalProperties".into(), json!(true));
    }
    schema
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn adds_additional_properties_to_root() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "mcp": { "type": "object" }
            }
        });
        let result = with_root_additional_properties(schema);
        assert_eq!(result["additionalProperties"], json!(true));
        assert_eq!(
            result["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(result["type"], "object");
    }

    #[test]
    fn idempotent_when_already_present() {
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {}
        });
        let result = with_root_additional_properties(schema);
        assert_eq!(result["additionalProperties"], json!(true));
    }

    #[test]
    fn non_object_value_returns_unchanged() {
        let schema = json!("string_schema");
        let result = with_root_additional_properties(schema);
        assert_eq!(result, json!("string_schema"));
    }

    #[test]
    #[expect(
        dead_code,
        reason = "test struct introspected via schemars::schema_for!"
    )]
    fn generated_schema_is_valid_json_schema_shape() {
        use schemars::JsonSchema;

        #[derive(JsonSchema)]
        struct TestConfig {
            name: String,
            count: i32,
        }

        let root = schemars::schema_for!(TestConfig);
        let value = serde_json::to_value(&root).unwrap();
        let result = with_root_additional_properties(value);

        assert_eq!(
            result["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(result["type"], "object");
        assert_eq!(result["additionalProperties"], json!(true));
        assert!(result["properties"]["name"].is_object());
        assert!(result["properties"]["count"].is_object());
    }
}
