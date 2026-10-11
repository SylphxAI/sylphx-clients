//! Request validation against the registry's message schemas (spec §3.1):
//! unknown fields are refused, types and enum values are checked, and
//! output-only fields a caller sent are dropped.

use serde_json::{Map, Value};

use crate::ir::{Field, Index, Type};
use crate::problem::ApiError;

/// Checks `v` against message `msg`. `at` is the field path so far, for
/// error details.
pub fn validate(ix: &Index, msg: &str, v: &Value, at: &str) -> Result<(), ApiError> {
    let Some(m) = ix.message(msg) else {
        return Ok(());
    };
    let Value::Object(obj) = v else {
        if v.is_null() {
            return Ok(());
        }
        return Err(ApiError::invalid(format!(
            "`{}` must be an object.",
            show(at)
        )));
    };
    for (k, val) in obj {
        let path = join(at, k);
        let f = m.fields.iter().find(|f| &f.name == k).ok_or_else(|| {
            ApiError::new(
                "UNKNOWN_FIELD",
                format!("`{path}` is not a field of this request."),
            )
        })?;
        field(ix, f, val, &path)?;
    }
    Ok(())
}

fn field(ix: &Index, f: &Field, v: &Value, at: &str) -> Result<(), ApiError> {
    if v.is_null() {
        return Ok(());
    }
    if f.repeated {
        let Value::Array(items) = v else {
            return Err(ApiError::invalid(format!("`{at}` must be an array.")));
        };
        for (i, item) in items.iter().enumerate() {
            ty(ix, &f.r#type, item, &format!("{at}[{i}]"))?;
        }
        return Ok(());
    }
    ty(ix, &f.r#type, v, at)
}

fn ty(ix: &Index, t: &Type, v: &Value, at: &str) -> Result<(), ApiError> {
    let bad = |what: &str| Err(ApiError::invalid(format!("`{at}` must be {what}.")));
    match t {
        Type::Scalar { scalar } => match scalar.as_str() {
            "string" | "bytes" => {
                if !v.is_string() {
                    return bad("a string");
                }
            }
            "bool" => {
                if !v.is_boolean() {
                    return bad("a boolean");
                }
            }
            "int32" | "uint32" => {
                if !(v.is_i64() || v.is_u64()) {
                    return bad("an integer");
                }
            }
            "int64" | "uint64" => {
                let ok = v.is_i64()
                    || v.is_u64()
                    || v.as_str().is_some_and(|s| s.parse::<i128>().is_ok());
                if !ok {
                    return bad("an integer or a decimal string");
                }
            }
            _ => {
                if !v.is_number() {
                    return bad("a number");
                }
            }
        },
        Type::Enum { name } => {
            let Some(s) = v.as_str() else {
                return bad("a string");
            };
            if let Some(e) = ix.enum_(name) {
                if !e.values.iter().any(|x| x.wire == s) {
                    let allowed: Vec<&str> = e.values.iter().map(|x| x.wire.as_str()).collect();
                    return Err(ApiError::invalid(format!(
                        "`{at}` must be one of {}.",
                        allowed.join(", ")
                    )));
                }
            }
        }
        Type::Message { name } => validate(ix, name, v, at)?,
        Type::WellKnown { wkt } => match wkt.as_str() {
            "timestamp" => {
                let ok = v
                    .as_str()
                    .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok());
                if !ok {
                    return bad("an RFC 3339 timestamp");
                }
            }
            "duration" => {
                if !v.as_str().is_some_and(|s| {
                    s.strip_suffix('s')
                        .is_some_and(|n| n.parse::<f64>().is_ok())
                }) {
                    return bad("a duration like \"30s\"");
                }
            }
            "field_mask" => {
                if !v.is_string() {
                    return bad("a comma-separated field mask");
                }
            }
            "empty" | "any" | "struct" if !v.is_object() => {
                return bad("an object");
            }
            _ => {}
        },
        Type::Map { value } => {
            let Value::Object(m) = v else {
                return bad("an object");
            };
            for (k, mv) in m {
                ty(ix, value, mv, &format!("{at}[{k:?}]"))?;
            }
        }
    }
    Ok(())
}

/// Removes every output-only field (recursively): callers may echo a read
/// Resource back, and the server owns those fields.
pub fn strip_output_only(ix: &Index, msg: &str, v: &mut Value) {
    let (Some(m), Value::Object(obj)) = (ix.message(msg), v) else {
        return;
    };
    obj.retain(|k, _| {
        m.fields
            .iter()
            .find(|f| &f.name == k)
            .is_none_or(|f| !f.has("OUTPUT_ONLY"))
    });
    for f in &m.fields {
        if let (Type::Message { name }, Some(child)) = (&f.r#type, obj.get_mut(&f.name)) {
            if f.repeated {
                if let Value::Array(items) = child {
                    for i in items {
                        strip_output_only(ix, name, i);
                    }
                }
            } else {
                strip_output_only(ix, name, child);
            }
        }
    }
}

/// REQUIRED fields of `msg` absent from `v`, recursing into present
/// message fields. Paths are dotted from `at`.
pub fn missing_required(ix: &Index, msg: &str, v: &Value, at: &str, out: &mut Vec<String>) {
    let Some(m) = ix.message(msg) else {
        return;
    };
    let empty = Map::new();
    let obj = v.as_object().unwrap_or(&empty);
    for f in &m.fields {
        if f.has("OUTPUT_ONLY") || f.has("IDENTIFIER") {
            continue;
        }
        // A message field is set when present, even as `{}` (proto3
        // presence); a scalar, list, or map when non-default.
        let present = obj
            .get(&f.name)
            .is_some_and(|x| match (&f.r#type, f.repeated) {
                (Type::Message { .. }, false) => !x.is_null(),
                _ => !is_empty(x),
            });
        if f.has("REQUIRED") && !present {
            out.push(join(at, &f.name));
        }
        if let (Type::Message { name }, Some(child), false) =
            (&f.r#type, obj.get(&f.name), f.repeated)
        {
            missing_required(ix, name, child, &join(at, &f.name), out);
        }
    }
}

/// The field at a dotted path of message `msg`.
pub fn field_at<'a>(ix: &'a Index, msg: &str, path: &str) -> Option<&'a Field> {
    let mut cur = ix.message(msg)?;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        let f = cur.fields.iter().find(|f| f.name == p)?;
        if parts.peek().is_none() {
            return Some(f);
        }
        match &f.r#type {
            Type::Message { name } if !f.repeated => cur = ix.message(name)?,
            // A map path `meta.labels.team` ends at the map.
            Type::Map { .. } => return parts.next().map(|_| f).filter(|_| parts.peek().is_none()),
            _ => return None,
        }
    }
    None
}

pub fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
    }
}

fn join(at: &str, k: &str) -> String {
    if at.is_empty() {
        k.to_string()
    } else {
        format!("{at}.{k}")
    }
}

fn show(at: &str) -> &str {
    if at.is_empty() {
        "body"
    } else {
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_and_bad_types_are_refused() {
        let ix = Index::embedded();
        let db = "sylphx.data.v1.Database";
        assert!(validate(
            &ix,
            db,
            &json!({"spec": {"compute_units": 1.5, "region": "hk"}}),
            ""
        )
        .is_ok());
        let e = validate(&ix, db, &json!({"spec": {"size": 1}}), "").unwrap_err();
        assert_eq!(e.code, "UNKNOWN_FIELD");
        assert!(e.detail.contains("spec.size"));
        let e = validate(&ix, db, &json!({"spec": {"compute_units": "big"}}), "").unwrap_err();
        assert_eq!(e.code, "INVALID_FIELD");
        let e = validate(&ix, db, &json!({"meta": {"labels": {"a": 1}}}), "").unwrap_err();
        assert_eq!(e.code, "INVALID_FIELD");
        assert!(validate(&ix, db, &json!({"meta": {"generation": "3"}}), "").is_ok());
    }

    #[test]
    fn output_only_fields_are_dropped_and_required_found() {
        let ix = Index::embedded();
        let mut v = json!({"uid": "x", "status": {}, "meta": {"etag": "e", "labels": {"a": "b"}}, "spec": {}});
        strip_output_only(&ix, "sylphx.data.v1.Database", &mut v);
        assert_eq!(v, json!({"meta": {"labels": {"a": "b"}}, "spec": {}}));
        let mut miss = vec![];
        missing_required(
            &ix,
            "sylphx.access.v1.ApiKey",
            &json!({"spec": {}}),
            "",
            &mut miss,
        );
        assert!(miss.contains(&"spec.kind".to_string()), "{miss:?}");
        assert!(
            field_at(&ix, "sylphx.data.v1.Database", "spec.region")
                .unwrap()
                .filterable
        );
        assert!(field_at(&ix, "sylphx.data.v1.Database", "meta.labels").is_some());
        assert!(field_at(&ix, "sylphx.data.v1.Database", "spec.nope").is_none());
    }
}
