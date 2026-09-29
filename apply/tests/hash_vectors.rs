//! The golden vectors of the managed-field hash
//! (`contracts/fixtures/apply-hash.json`, spec §3): RFC 8785 text, the
//! SHA-256 prefix, and the canonical protojson rendering of managed fields.

use serde_json::{Map, Value};
use sylphx_apply::hash::{hash_managed, jcs, managed_object, resource_hash, Mode};
use sylphx_apply::Registry;

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/fixtures/apply-hash.json"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).expect("the fixture is JSON")
}

fn object(v: &Value) -> Map<String, Value> {
    v.as_object().expect("an object").clone()
}

#[test]
fn rfc_8785_text() {
    let f = fixture();
    let cases = f["jcs"].as_array().unwrap();
    assert!(!cases.is_empty());
    for c in cases {
        let input: Value = serde_json::from_str(c["input_json"].as_str().unwrap()).unwrap();
        assert_eq!(
            jcs(&input),
            c["canonical"].as_str().unwrap(),
            "{}",
            c["name"]
        );
    }
}

#[test]
fn managed_object_hashes() {
    let f = fixture();
    let cases = f["hash"].as_array().unwrap();
    assert!(!cases.is_empty());
    for c in cases {
        let managed = object(&c["managed"]);
        assert_eq!(
            jcs(&Value::Object(managed.clone())),
            c["canonical"].as_str().unwrap(),
            "{}",
            c["name"]
        );
        let hash = hash_managed(&managed);
        assert_eq!(hash.len(), 32);
        assert_eq!(hash, c["hash"].as_str().unwrap(), "{}", c["name"]);
    }
}

#[test]
fn canonical_protojson_of_managed_fields() {
    let f = fixture();
    let reg = Registry::from_json(&f["registry"].to_string()).unwrap();
    let rt = reg.resource(f["kind"].as_str().unwrap()).unwrap();
    let cases = f["protojson"].as_array().unwrap();
    assert!(!cases.is_empty());
    for c in cases {
        let managed = managed_object(&reg, rt, &c["resource"], Mode::Hash);
        assert_eq!(
            Value::Object(managed.clone()),
            c["managed"],
            "{}",
            c["name"]
        );
        assert_eq!(hash_managed(&managed), c["hash"].as_str().unwrap());
        assert_eq!(
            resource_hash(&reg, rt, &c["resource"]),
            c["hash"].as_str().unwrap()
        );
    }
}
