#[path = "../assemble.rs"]
mod assemble;

use std::fs;
use sylphx_apply::registry::Registry;

#[test]
fn additions_and_deletions_are_in_the_assembled_registry() {
    let root = std::env::temp_dir().join(format!("apply-registry-{}", uuid::Uuid::now_v7()));
    for category in ["resources", "messages", "enums", "services"] {
        fs::create_dir_all(root.join(category)).unwrap();
    }
    let registry = || Registry::from_json(&assemble::assemble(&root).unwrap()).unwrap();
    assert!(registry().resources().is_empty());
    fs::write(
        root.join("resources/demo.One.json"),
        r#"{"type":"demo/One","message":"demo.One"}"#,
    )
    .unwrap();
    fs::write(
        root.join("messages/demo.One.json"),
        r#"{"name":"demo.One","fields":[]}"#,
    )
    .unwrap();
    fs::write(
        root.join("enums/demo.State.json"),
        r#"{"name":"demo.State","values":[]}"#,
    )
    .unwrap();
    fs::write(root.join("services/demo.ones.json"), r#"{"collections":[{"resource_type":"demo/One","methods":[{"id":"demo.ones.get","kind":"get"}]}]}"#).unwrap();
    let added = registry();
    assert!(added.resource("demo/One").is_some());
    assert!(added.message("demo.One").is_some());
    assert!(added.enum_("demo.State").is_some());
    assert!(added.method("demo/One", "get").is_some());
    let first = assemble::assemble(&root).unwrap();
    assert_eq!(first, assemble::assemble(&root).unwrap());
    for entry in [
        "resources/demo.One.json",
        "messages/demo.One.json",
        "enums/demo.State.json",
        "services/demo.ones.json",
    ] {
        fs::remove_file(root.join(entry)).unwrap();
    }
    for category in ["resources", "messages", "enums", "services"] {
        fs::remove_dir(root.join(category)).unwrap();
    }
    let deleted = registry();
    assert!(deleted.resource("demo/One").is_none());
    assert!(deleted.message("demo.One").is_none());
    assert!(deleted.enum_("demo.State").is_none());
    assert!(deleted.method("demo/One", "get").is_none());
    fs::remove_dir_all(root).unwrap();
}
