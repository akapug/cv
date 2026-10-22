//! Sidecar writer and schema compatibility; retrieval behavior is tested in prune/tests.rs.
use super::*;
fn owned_line(id: &str, name: &str, input: &Value, content: &str) -> String {
    // Keep this serializer owned to independently pin the bytes of the borrowed writer;
    // SidecarEntry supplies the shared schema while construction takes the owned path.
    serde_json::to_string(&SidecarEntry {
        id: id.to_owned(),
        slot: "content".into(),
        name: name.to_owned(),
        input: input.clone(),
        content: Value::String(content.to_owned()),
        size: content.len(),
        line_count: content.lines().count(),
        kind: "text".into(),
    })
    .unwrap()
}
#[test]
fn borrowed_sidecar_schema_and_staged_file_match_owned_oracle() -> anyhow::Result<()> {
    let world = cv_testkit::World::new("distill-sidecar-borrowed");
    let path = world.base.join("sidecar.jsonl");
    let mut input = serde_json::Map::new();
    input.insert(
        "z".into(),
        serde_json::json!({"float":-0.0,"unicode":"λ","escaped":"\n\\\""}),
    );
    input.insert("a".into(), Value::Null);
    let input = Value::Object(input);
    let contents = [
        String::new(),
        "\n".into(),
        "\r\n".into(),
        "line\r\nlast\n".into(),
        "\"\\\0λ".repeat(300_000),
    ];
    let mut expected = String::new();
    let mut elided = Vec::new();
    for content in contents {
        let old = owned_line("id\nλ", "name\"", &input, &content);
        assert_eq!(sidecar_line("id\nλ", "name\"", &input, &content), old);
        expected.push_str(&old);
        expected.push('\n');
        elided.push(crate::distill::Elided {
            tool_use_id: "id\nλ".into(),
            tool_name: "name\"".into(),
            input: input.clone(),
            content,
        });
    }
    std::fs::write(&path, b"previous artifact")?;
    let taken = crate::distill::write_sidecar(&path, &elided, crate::publish::Claim::New).unwrap_err();
    assert!(taken.downcast_ref::<crate::publish::Exists>().is_some(), "{taken:#}");
    assert_eq!(std::fs::read(&path)?, b"previous artifact");
    crate::distill::write_sidecar(&path, &elided, crate::publish::Claim::Replace)?;
    assert_eq!(std::fs::read(&path)?, expected.as_bytes());
    let mut persisted: Vec<SidecarEntry> = Vec::new();
    for line in expected.lines() {
        persisted.push(serde_json::from_str(line)?);
    }
    assert_eq!(persisted.len(), elided.len());
    assert!(
        crate::distill::write_sidecar(&world.base.join("missing/sidecar"), &elided, crate::publish::Claim::New)
            .is_err()
    );
    assert_eq!(std::fs::read(&path)?, expected.as_bytes());
    Ok(())
}
#[test]
fn owned_sidecar_deserialization_shape_and_positional_order_remain_unchanged() -> anyhow::Result<()> {
    let sequence =
        serde_json::json!(["id", "content", "Read", {"arg":1}, ["arbitrary original content"], 27, 1, "mixed"]);
    let entry: SidecarEntry = serde_json::from_value(sequence)?;
    assert_eq!(entry.id, "id");
    assert_eq!(entry.slot, "content");
    assert_eq!(entry.name, "Read");
    assert_eq!(entry.input, serde_json::json!({"arg":1}));
    assert_eq!(entry.content, serde_json::json!(["arbitrary original content"]));
    assert_eq!(entry.size, 27);
    assert_eq!(entry.line_count, 1);
    assert_eq!(entry.kind, "mixed");
    assert!(serde_json::from_str::<SidecarEntry>(r#"{"id":"first","id":"last","slot":"content","name":"Read","input":null,"content":"text","size":4,"line_count":1,"kind":"text"}"#).is_err());
    Ok(())
}

/// A replaced sidecar is a new file under the name: the old inode and its hard links keep the
/// old bytes, a symlink at the name is replaced (its target untouched), permissions carry over.
#[cfg(unix)]
#[test]
fn replaced_sidecar_is_a_new_file_and_linked_targets_keep_their_bytes() -> anyhow::Result<()> {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let world = cv_testkit::World::new("distill-sidecar-links");
    let path = world.base.join("sidecar.jsonl");
    let hard = world.base.join("hard");
    let link = world.base.join("link");
    std::fs::write(&path, b"old")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
    std::fs::hard_link(&path, &hard)?;
    symlink(&path, &link)?;
    let inode = std::fs::metadata(&path)?.ino();
    let elided = vec![crate::distill::Elided {
        tool_use_id: "id".into(),
        tool_name: "Read".into(),
        input: Value::Null,
        content: "new λ".into(),
    }];
    let expected = format!("{}\n", owned_line("id", "Read", &Value::Null, "new λ"));
    crate::distill::write_sidecar(&link, &elided, crate::publish::Claim::Replace)?;
    assert!(!std::fs::symlink_metadata(&link)?.file_type().is_symlink());
    assert_eq!(std::fs::read(&link)?, expected.as_bytes());
    assert_eq!(std::fs::read(&path)?, b"old");
    crate::distill::write_sidecar(&path, &elided, crate::publish::Claim::Replace)?;
    assert_ne!(std::fs::metadata(&path)?.ino(), inode);
    assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read(&path)?, expected.as_bytes());
    assert_eq!(std::fs::read(&hard)?, b"old");
    Ok(())
}
