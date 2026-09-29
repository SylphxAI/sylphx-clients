//! The `ids` module against the golden vectors of `contracts/fixtures/ids.json`
//! (the TypeScript SDK reads the same file): exact TypeID grammar, round trips,
//! and every retired form refused.

use serde_json::Value;
use sylphx::ids::{self, IdError, Prefix, TypeId};

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/fixtures/ids.json"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).expect("ids.json")
}

fn prefix(text: &str) -> Prefix {
    Prefix::of(text).unwrap_or_else(|| panic!("`{text}` is not registered"))
}

#[test]
fn valid_ids_round_trip() {
    for v in fixture()["valid"].as_array().unwrap() {
        let (p, uuid, typeid, dns) = (
            v["prefix"].as_str().unwrap(),
            v["uuid"].as_str().unwrap(),
            v["typeid"].as_str().unwrap(),
            v["dns"].as_str().unwrap(),
        );
        let id = ids::parse(typeid).unwrap_or_else(|e| panic!("{typeid}: {e}"));
        assert_eq!(id.prefix(), prefix(p), "{typeid}");
        assert_eq!(id.uuid().to_string(), uuid, "{typeid}");
        assert_eq!(id.to_string(), typeid);
        assert_eq!(id.dns_form(), dns);
        assert_eq!(
            TypeId::from_uuid(prefix(p), uuid.parse().unwrap()).to_string(),
            typeid
        );
        assert_eq!(typeid.parse::<TypeId>().unwrap(), id);
        assert_eq!(ids::validate(prefix(p), typeid).unwrap(), id);
    }
}

#[test]
fn invalid_forms_are_refused() {
    for v in fixture()["invalid"].as_array().unwrap() {
        let input = v["input"].as_str().unwrap();
        assert!(
            ids::parse(input).is_err(),
            "{input:?} ({}) must not parse",
            v["why"]
        );
    }
}

#[test]
fn a_wrong_prefix_is_named() {
    for v in fixture()["wrong_prefix"].as_array().unwrap() {
        let expect = prefix(v["expect"].as_str().unwrap());
        let input = v["input"].as_str().unwrap();
        assert!(ids::parse(input).is_ok(), "{input}");
        match ids::validate(expect, input) {
            Err(IdError::WrongPrefix { expected, .. }) => assert_eq!(expected, expect.as_str()),
            other => panic!("{input}: {other:?}"),
        }
    }
}

#[test]
fn minted_ids_are_v7_and_ordered() {
    let a = ids::mint(ids::PRJ);
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = ids::mint(ids::PRJ);
    assert_eq!(a.uuid().get_version_num(), 7);
    assert!(a.to_string() < b.to_string());
    assert_eq!(ids::parse(&a.to_string()).unwrap(), a);
    assert!(a.to_string().starts_with("prj_"));
    assert_eq!(a.to_string().len(), "prj_".len() + 26);
}

#[test]
fn the_registry_is_well_formed() {
    let mut seen = std::collections::BTreeSet::new();
    for p in ids::ALL {
        assert!(
            (2..=5).contains(&p.as_str().len())
                && p.as_str().bytes().all(|b| b.is_ascii_lowercase()),
            "{p}"
        );
        assert!(seen.insert(p.as_str()), "duplicate {p}");
    }
    assert_eq!(ids::ORG.entity(), "Org");
    assert_eq!(ids::PRJ.collection(), Some("projects"));
    assert_eq!(ids::SVC.service(), "hosting");
}
