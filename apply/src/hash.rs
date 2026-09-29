//! The managed fields of a Resource and their hash (spec §3).
//!
//! Every managed top-level `spec` field, `meta.display_name`, and the
//! `meta.labels` other than `sylphx-*` are rendered as canonical protojson
//! (int64 as strings, enums as strings, unset and default fields omitted),
//! the object of them is serialised with RFC 8785, and the hash is the
//! SHA-256 of that text in hex, first 32 characters.

use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use crate::registry::{Field, FieldType, Registry, ResourceType};

/// The prefix of every marker label and annotation.
pub const RESERVED_PREFIX: &str = "sylphx-";

/// What a value that must not be shown is replaced with in a diff.
pub const REDACTED: &str = "[redacted]";

/// How [`canonical`] treats sensitive and INPUT_ONLY fields below a managed
/// field: dropped from the hash, redacted in a diff.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Hash,
    Display,
}

/// The managed state of a Resource, keyed by managed field
/// (`spec.<field>`, `meta.display_name`, `meta.labels`); fields that are
/// unset are absent. `resource` is a Resource in wire JSON: a Get answer, a
/// validate-only answer, or a declaration shaped as one.
pub fn managed_object(
    reg: &Registry,
    rt: &ResourceType,
    resource: &Value,
    mode: Mode,
) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(spec) = resource.get("spec").and_then(Value::as_object) {
        for field in reg.managed_fields(rt) {
            if let Some(v) = spec.get(&field.name) {
                if let Some(c) = canonical_field(reg, field, v, mode) {
                    out.insert(format!("spec.{}", field.name), c);
                }
            }
        }
    }
    let meta = resource.get("meta");
    if let Some(d) = meta
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        out.insert("meta.display_name".into(), Value::String(d.to_string()));
    }
    if let Some(labels) = meta
        .and_then(|m| m.get("labels"))
        .and_then(Value::as_object)
    {
        let kept: Map<String, Value> = labels
            .iter()
            .filter(|(k, _)| !k.starts_with(RESERVED_PREFIX))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !kept.is_empty() {
            out.insert("meta.labels".into(), Value::Object(kept));
        }
    }
    out
}

/// The hash of a managed object: SHA-256 of its RFC 8785 text, hex, first
/// 32 characters.
pub fn hash_managed(managed: &Map<String, Value>) -> String {
    let text = jcs(&Value::Object(managed.clone()));
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = to_hex(&digest);
    hex.truncate(32);
    hex
}

/// The hash of a Resource's managed fields.
pub fn resource_hash(reg: &Registry, rt: &ResourceType, resource: &Value) -> String {
    hash_managed(&managed_object(reg, rt, resource, Mode::Hash))
}

/// Lower-case hex.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Canonical protojson of one field value; `None` when it is unset (or the
/// proto3 default of a field without explicit presence).
pub fn canonical_field(reg: &Registry, field: &Field, value: &Value, mode: Mode) -> Option<Value> {
    if value.is_null() {
        return None;
    }
    if mode == Mode::Display && (field.sensitive || field.has("INPUT_ONLY")) {
        return Some(Value::String(REDACTED.into()));
    }
    if field.sensitive || field.has("INPUT_ONLY") {
        return None;
    }
    if field.repeated {
        let items = value.as_array()?;
        if items.is_empty() {
            return None;
        }
        let out: Vec<Value> = items
            .iter()
            .map(|v| canonical_value(reg, &field.ty, v, mode))
            .collect();
        return Some(Value::Array(out));
    }
    match &field.ty {
        FieldType::Map { value: inner } => {
            let entries = value.as_object()?;
            if entries.is_empty() {
                return None;
            }
            let out: Map<String, Value> = entries
                .iter()
                .map(|(k, v)| (k.clone(), canonical_value(reg, inner, v, mode)))
                .collect();
            Some(Value::Object(out))
        }
        ty => {
            let c = canonical_value(reg, ty, value, mode);
            if !field.optional && is_default(ty, &c) {
                None
            } else {
                Some(c)
            }
        }
    }
}

/// Proto3 default of a scalar, enum, or string-like value.
fn is_default(ty: &FieldType, v: &Value) -> bool {
    match ty {
        FieldType::Scalar { scalar } => match scalar.as_str() {
            "string" | "bytes" => v.as_str() == Some(""),
            "bool" => v.as_bool() == Some(false),
            "int64" | "uint64" => v.as_str() == Some("0"),
            _ => v.as_f64() == Some(0.0),
        },
        FieldType::Enum { .. } => v.as_str().is_none_or(str::is_empty),
        _ => false,
    }
}

/// Canonical protojson of a single (non-repeated) value of type `ty`.
pub fn canonical_value(reg: &Registry, ty: &FieldType, value: &Value, mode: Mode) -> Value {
    match ty {
        FieldType::Scalar { scalar } => match scalar.as_str() {
            "int64" | "uint64" | "fixed64" | "sfixed64" | "sint64" => match value {
                Value::Number(n) => Value::String(n.to_string()),
                other => other.clone(),
            },
            "int32" | "uint32" | "fixed32" | "sfixed32" | "sint32" => match value {
                Value::String(s) => s
                    .parse::<i64>()
                    .map(|n| Value::Number(n.into()))
                    .unwrap_or_else(|_| value.clone()),
                other => other.clone(),
            },
            "double" | "float" => match value {
                Value::String(s) => s
                    .parse::<f64>()
                    .ok()
                    .and_then(Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or_else(|| value.clone()),
                other => other.clone(),
            },
            _ => value.clone(),
        },
        FieldType::Enum { name } => match value {
            // An enum number renders as its wire name.
            Value::Number(n) => n
                .as_i64()
                .and_then(|n| {
                    reg.enum_(name)
                        .and_then(|e| e.values.iter().find(|v| v.number == n))
                })
                .map(|v| Value::String(v.wire.clone()))
                .unwrap_or_else(|| value.clone()),
            other => other.clone(),
        },
        FieldType::Message { name } => {
            let (Some(message), Some(obj)) = (reg.message(name), value.as_object()) else {
                return value.clone();
            };
            let mut out = Map::new();
            for (k, v) in obj {
                match message.field(k) {
                    Some(f) => {
                        if let Some(c) = canonical_field(reg, f, v, mode) {
                            out.insert(k.clone(), c);
                        }
                    }
                    None => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            Value::Object(out)
        }
        FieldType::Map { value: inner } => match value.as_object() {
            Some(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), canonical_value(reg, inner, v, mode)))
                    .collect(),
            ),
            None => value.clone(),
        },
        FieldType::WellKnown { .. } => value.clone(),
    }
}

/// RFC 8785 (JSON Canonicalization Scheme) text of `value`.
pub fn jcs(value: &Value) -> String {
    let mut out = String::new();
    write_jcs(value, &mut out);
    out
}

fn write_jcs(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&es_number(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_jcs(v, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Members sort by their UTF-16 code units.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_jcs(&map[k], out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// ECMAScript `Number::toString` of a finite double, which RFC 8785 §3.2.2.3
/// requires.
pub fn es_number(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // Shortest round-trip digits and exponent: `d.ddde<exp>`.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    // `n` is the decimal exponent such that value = 0.digits * 10^n.
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign_e = if e < 0 { "-" } else { "+" };
        if k == 1 {
            format!("{digits}e{sign_e}{}", e.abs())
        } else {
            format!("{}.{}e{sign_e}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn es_numbers_follow_ecmascript() {
        for (x, want) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (4.5, "4.5"),
            (0.002, "0.002"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (5e-324, "5e-324"),
            (-1.5, "-1.5"),
            (333333333.3333333, "333333333.3333333"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (123456789012345680000.0, "123456789012345680000"),
        ] {
            assert_eq!(es_number(x), want, "{x:e}");
        }
    }

    #[test]
    fn jcs_sorts_by_utf16_and_escapes() {
        let v = json!({"b": 1, "a": [true, null, "x\ny\u{1}"], "\u{e9}": 2, "\u{10000}": 3});
        assert_eq!(
            jcs(&v),
            "{\"a\":[true,null,\"x\\ny\\u0001\"],\"b\":1,\"\u{e9}\":2,\"\u{10000}\":3}"
        );
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;
    use crate::check::fixtures;
    use serde_json::json;

    #[test]
    fn a_nested_sensitive_field_is_dropped_from_the_hash_and_redacted_in_a_diff() {
        let mut reg_json: Value = serde_json::from_str(fixtures::REGISTRY).unwrap();
        // `demo.Limits` gains a nested secret.
        for m in reg_json["messages"].as_array_mut().unwrap() {
            if m["name"] == "demo.Limits" {
                m["fields"].as_array_mut().unwrap().push(json!({
                    "name": "key", "behaviors": ["OPTIONAL"], "sensitive": true,
                    "type": {"kind": "scalar", "scalar": "string"}
                }));
            }
        }
        let reg = Registry::from_json(&reg_json.to_string()).unwrap();
        let rt = reg.resource("demo.sylphx.com/Thing").unwrap();
        let resource = json!({"spec": {"limits": {"cpu": 1, "key": "s3cret"}}});
        let hashed = managed_object(&reg, rt, &resource, Mode::Hash);
        assert_eq!(hashed["spec.limits"], json!({"cpu": 1}));
        let shown = managed_object(&reg, rt, &resource, Mode::Display);
        assert_eq!(shown["spec.limits"], json!({"cpu": 1, "key": REDACTED}));
        assert!(!jcs(&Value::Object(shown)).contains("s3cret"));
    }
}
