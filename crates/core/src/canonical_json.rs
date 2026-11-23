//! Canonical JSON byte writer for transient payload identity.
//!
//! `serde_json` can preserve insertion order. The identity writer orders keys explicitly.

/// Serialize JSON with recursively sorted object keys and preserve array order.
/// Primitive formatting uses `serde_json`.
#[must_use]
pub fn canonical_json_bytes(value: &serde_json::Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical_json(value, &mut out);
    out
}

fn write_canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(true) => out.extend_from_slice(b"true"),
        serde_json::Value::Bool(false) => out.extend_from_slice(b"false"),
        serde_json::Value::Number(number) => {
            serde_json::to_writer(out, number).expect("serializing JSON number to Vec cannot fail");
        }
        serde_json::Value::String(text) => {
            serde_json::to_writer(out, text).expect("serializing JSON string to Vec cannot fail");
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(item, out);
            }
            out.push(b']');
        }
        // Sort object keys at each level for stable identity, preserving array order.
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)
                    .expect("serializing JSON object key to Vec cannot fail");
                out.push(b':');
                write_canonical_json(
                    map.get(key)
                        .expect("object key collected from map must still exist"),
                    out,
                );
            }
            out.push(b'}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_nested_object_keys_and_preserves_array_order() {
        let input = serde_json::json!({"z": {"b": 2, "a": 1}, "a": ["z", "a"]});
        assert_eq!(
            canonical_json_bytes(&input),
            br#"{"a":["z","a"],"z":{"a":1,"b":2}}"#
        );
    }
}
