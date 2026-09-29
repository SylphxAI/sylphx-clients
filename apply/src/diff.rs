//! JSON Patch (RFC 6902) from the live managed state to the desired one, for
//! the plan's `diff`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One JSON Patch operation: `add`, `remove`, or `replace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchOp {
    pub op: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// The operations that turn `from` into `to`. Objects are compared member by
/// member; a list or a scalar that differs is replaced whole.
pub fn json_patch(from: &Value, to: &Value) -> Vec<PatchOp> {
    let mut out = Vec::new();
    walk(from, to, "", &mut out);
    out
}

fn walk(from: &Value, to: &Value, path: &str, out: &mut Vec<PatchOp>) {
    if from == to {
        return;
    }
    match (from, to) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, va) in a {
                let here = format!("{path}/{}", escape(k));
                match b.get(k) {
                    None => out.push(PatchOp {
                        op: "remove".into(),
                        path: here,
                        value: None,
                    }),
                    Some(vb) => walk(va, vb, &here, out),
                }
            }
            for (k, vb) in b {
                if !a.contains_key(k) {
                    out.push(PatchOp {
                        op: "add".into(),
                        path: format!("{path}/{}", escape(k)),
                        value: Some(vb.clone()),
                    });
                }
            }
        }
        _ => out.push(PatchOp {
            op: "replace".into(),
            path: path.to_string(),
            value: Some(to.clone()),
        }),
    }
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The structured view of a managed object (`spec.<f>`, `meta.<f>` keys):
/// `{"spec": {...}, "meta": {...}}`.
pub fn view_of(managed: &Map<String, Value>) -> Value {
    let mut spec = Map::new();
    let mut meta = Map::new();
    for (k, v) in managed {
        if let Some(f) = k.strip_prefix("spec.") {
            spec.insert(f.to_string(), v.clone());
        } else if let Some(f) = k.strip_prefix("meta.") {
            meta.insert(f.to_string(), v.clone());
        }
    }
    let mut out = Map::new();
    if !meta.is_empty() {
        out.insert("meta".into(), Value::Object(meta));
    }
    if !spec.is_empty() {
        out.insert("spec".into(), Value::Object(spec));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diffs_objects_member_by_member() {
        let from =
            json!({"spec": {"a": 1, "b": [1, 2], "gone": true}, "meta": {"labels": {"x/y": "1"}}});
        let to =
            json!({"spec": {"a": 2, "b": [1, 3], "new": "n"}, "meta": {"labels": {"x/y": "1"}}});
        let ops = json_patch(&from, &to);
        let text = serde_json::to_value(&ops).unwrap();
        assert_eq!(
            text,
            json!([
                {"op": "replace", "path": "/spec/a", "value": 2},
                {"op": "replace", "path": "/spec/b", "value": [1, 3]},
                {"op": "remove", "path": "/spec/gone"},
                {"op": "add", "path": "/spec/new", "value": "n"},
            ])
        );
    }

    #[test]
    fn keys_are_pointer_escaped() {
        let ops = json_patch(&json!({}), &json!({"a/b~c": 1}));
        assert_eq!(ops[0].path, "/a~1b~0c");
    }

    #[test]
    fn equal_values_have_no_diff() {
        assert!(json_patch(&json!({"a": [1]}), &json!({"a": [1]})).is_empty());
    }

    #[test]
    fn views_nest_by_prefix() {
        let mut m = Map::new();
        m.insert("spec.size".into(), json!("1"));
        m.insert("meta.display_name".into(), json!("N"));
        assert_eq!(
            view_of(&m),
            json!({"spec": {"size": "1"}, "meta": {"display_name": "N"}})
        );
    }
}
