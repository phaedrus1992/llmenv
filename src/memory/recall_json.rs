//! The JSON form of an `icm_memory_recall` reply (icm 0.11.5 and later, #3368).
//!
//! The text form changes its layout between icm releases, and it has no record ids. The JSON
//! form gives each record its id, so a reader does not have to parse free text.

use serde::Deserialize;

mod error;
pub(crate) use error::RecallJsonError;

/// The value of the recall `format` argument that asks icm for the JSON form.
pub(crate) const FORMAT: &str = "json";

/// The recall `max_tokens` budget for a call that reads a whole set of records. icm clamps
/// `limit` to 20 without a budget. With a budget it returns as many records as fit, and allows a
/// `limit` of up to [`BUDGET_MAX_LIMIT`] (icm-v0.11.5, `crates/icm-mcp/src/tools.rs`, #3419).
/// 32000 is the largest budget icm accepts.
pub(crate) const MAX_TOKENS: u32 = 32_000;

/// The largest recall `limit` that icm honors when the call gives [`MAX_TOKENS`].
pub(crate) const BUDGET_MAX_LIMIT: u32 = 200;

/// One record of a JSON recall reply. icm adds more fields, such as `score`, `bm25`, and
/// `related_ids`; a reader that does not need them ignores them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct RecallRecord {
    /// The icm record id. It is never blank.
    pub id: String,
    pub topic: String,
    pub summary: String,
    /// The icm importance level, such as `high`, as icm spells it.
    pub importance: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// Most characters of a reply that is not a list that an error quotes. The quote shows the
/// shape of the reply, such as `--- <id>` or `No memories`, and little of the memory text.
const QUOTE_CHARS: usize = 24;

/// The first characters of `text`, on one line, so a quote cannot forge a log line.
fn quote(text: &str) -> String {
    text.chars()
        .take(QUOTE_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The records of a JSON recall reply, in icm's rank order.
///
/// icm can append an advisory line after the list, so only the first JSON value is read.
///
/// # Errors
/// The reply is empty, is cut off inside the list, does not start with a JSON array, or holds an
/// element that is not a record with a non-blank `id`, a `topic`, a `summary`, and an
/// `importance`.
pub(crate) fn parse(body: &str) -> Result<Vec<RecallRecord>, RecallJsonError> {
    let body = body.trim_start();
    let first = serde_json::Deserializer::from_str(body)
        .into_iter::<Vec<serde_json::Value>>()
        .next();
    let values = match first {
        Some(Ok(values)) => values,
        Some(Err(e)) if e.is_eof() => {
            return Err(RecallJsonError::CutOff {
                bytes: body.len(),
                reason: e.to_string(),
            });
        }
        Some(Err(e)) => {
            return Err(RecallJsonError::NotAList {
                reason: e.to_string(),
                start: quote(body),
            });
        }
        None => return Err(RecallJsonError::Empty),
    };
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| record(index, value))
        .collect()
}

/// The field names of a record, or its JSON type, for an error that must not quote memory text.
fn shape(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            format!("an object with the fields [{}]", keys.join(", "))
        }
        serde_json::Value::Array(_) => "an array".to_string(),
        serde_json::Value::String(_) => "a string".to_string(),
        serde_json::Value::Number(_) => "a number".to_string(),
        serde_json::Value::Bool(_) => "a boolean".to_string(),
        serde_json::Value::Null => "null".to_string(),
    }
}

fn record(index: usize, value: serde_json::Value) -> Result<RecallRecord, RecallJsonError> {
    let bad = |reason: String, value: &serde_json::Value| RecallJsonError::BadRecord {
        index,
        reason,
        shape: shape(value),
    };
    let parsed = RecallRecord::deserialize(&value).map_err(|e| bad(e.to_string(), &value))?;
    if parsed.id.trim().is_empty() {
        return Err(bad("the id is blank".to_string(), &value));
    }
    Ok(parsed)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
