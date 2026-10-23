use super::*;
use crate::discover_cache::read::Context;
use rusqlite::params;

fn session(w: &std::path::Path, id: &str) -> SessionRef {
    SessionRef::new(id, Harness::Claude, w.join(format!("{id}.jsonl")))
}
fn column(r: &SessionRef) -> Option<String> {
    open_db()
        .unwrap()
        .query_row(
            "SELECT writer_version FROM sessions WHERE harness=?1 AND id=?2 AND path=?3",
            params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
            |row| row.get(0),
        )
        .unwrap()
}
fn observe_and_sync(context: &Context, r: &SessionRef, version: Option<&str>) {
    context.run(|| {
        context.observe_writer(r, version);
        sync(std::slice::from_ref(r));
    });
}

#[test]
fn observations_are_monotonic_retained_and_do_not_read_sources() {
    let w = crate::test_world("writer-observed");
    let r = session(&w.base, "absent-source");
    let other = session(&w.base, "unknown");
    sync(&[r.clone(), other.clone()]);
    assert_eq!(column(&r), None);
    let context = Context::current();
    observe_and_sync(&context, &r, Some("2.10.0-rc.2"));
    observe_and_sync(&context, &r, Some("2.10.0"));
    // Independent operation, as with concurrent/older discovery publishing after a newer one.
    observe_and_sync(&Context::current(), &r, Some("2.9.0"));
    observe_and_sync(&Context::current(), &r, None);
    let mut retitled = r.clone();
    retitled.title = Some("changed".into());
    replace_harness(Harness::Claude, &[retitled, other.clone()], &[], &Default::default());
    assert_eq!(column(&r).as_deref(), Some("2.10.0"));
    let summary = writer_versions();
    let claude = &summary[0];
    assert_eq!((claude.sessions, claude.unknown), (2, 1));
    assert_eq!(claude.newest.as_deref(), Some("2.10.0"));
    // There has never been a transcript at either path. Removing a row removes its evidence.
    replace_harness(Harness::Claude, &[other], &[], &Default::default());
    assert_eq!(writer_versions()[0].newest, None);
}

#[test]
fn null_rows_remain_unknown_until_normal_observation() {
    let w = crate::test_world("writer-null");
    let r = session(&w.base, "bare");
    sync(std::slice::from_ref(&r));
    assert_eq!(column(&r), None);
    assert_eq!(writer_versions()[0].unknown, 1);
    observe_and_sync(&Context::current(), &r, None);
    assert_eq!(column(&r).as_deref(), Some(""));
    assert_eq!(writer_versions()[0].unknown, 1);
    sync(std::slice::from_ref(&r));
    assert_eq!(column(&r).as_deref(), Some(""));
    assert!(!r.path.exists(), "catalog publication does not read sources");
}

#[test]
fn discovery_observes_newest_claude_tail_and_canonical_codex_meta() {
    // Claude folds the newest recorded writer version; Codex trusts its first canonical
    // session_meta, even when later copied parent metadata names a newer version.
    let w = crate::test_world("writer-discovery");
    let claude = w.base.join("claude/projects/project");
    std::fs::create_dir_all(&claude).unwrap();
    let path = claude.join("c.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"version\":\"2.9.0\"}\n{\"type\":\"user\",\"version\":\"2.10.0\"}\n",
    )
    .unwrap();
    let context = Context::current();
    context.run(|| {
        use crate::harness::Adapter;
        let adapter = crate::harness::claude::Claude::with_config_dir(&w.base.join("claude/projects"));
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        sync(&refs);
        assert_eq!(column(&refs[0]).as_deref(), Some("2.10.0"));
    });
    let codex = w.base.join("codex/sessions");
    std::fs::create_dir_all(&codex).unwrap();
    let path = codex.join("rollout-x.jsonl");
    std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"cli_version\":\"0.162.1\"}}\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"parent\",\"cli_version\":\"9.0.0\"}}\n").unwrap();
    Context::current().run(|| {
        use crate::harness::Adapter;
        let adapter = crate::harness::codex::Codex::with_home(&w.base.join("codex"));
        let refs = adapter.discover().unwrap();
        sync(&refs);
        assert_eq!(column(&refs[0]).as_deref(), Some("0.162.1"));
    });
}

#[test]
fn cached_unknown_stays_unknown_until_normal_source_change() {
    let w = crate::test_world("writer-cached-null");
    let projects = w.base.join("projects");
    std::fs::create_dir_all(projects.join("p")).unwrap();
    let path = projects.join("p/c.jsonl");
    std::fs::write(&path, "{\"type\":\"user\",\"version\":\"2.1.296\"}\n").unwrap();
    let discover = || {
        use crate::harness::Adapter;
        crate::harness::claude::Claude::with_config_dir(&projects)
            .discover()
            .unwrap()
    };
    let refs = Context::current().run(|| {
        let refs = discover();
        sync(&refs);
        crate::discover_cache::persist(false);
        refs
    });
    open_db()
        .unwrap()
        .execute("UPDATE sessions SET writer_version=NULL", [])
        .unwrap();
    let settled = Context::current();
    settled.run(|| {
        sync(&discover());
        assert!(
            settled.writer_versions(Harness::Claude).is_empty(),
            "writer coverage must not force a cached source read"
        );
    });
    assert_eq!(column(&refs[0]), None);
    let observed = writer_versions();
    let claude = observed.iter().find(|row| row.harness == "claude").unwrap();
    assert_eq!((claude.sessions, claude.unknown), (1, 1));
    // The metadata continuation retains its previous maximum and folds a newer appended writer.
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{{\"type\":\"user\",\"version\":\"2.2.0\"}}"
    )
    .unwrap();
    Context::current().run(|| {
        sync(&discover());
    });
    assert_eq!(column(&refs[0]).as_deref(), Some("2.2.0"));
}

#[cfg(unix)]
#[test]
fn read_only_catalog_serves_observations_and_drops_publication() {
    use std::os::unix::fs::PermissionsExt;
    let w = crate::test_world("writer-read-only");
    let r = session(&w.base, "recorded");
    observe_and_sync(&Context::current(), &r, Some("2.1.296"));
    let modes: Vec<_> = std::fs::read_dir(&w.cv_home)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            (path, mode)
        })
        .collect();
    std::fs::set_permissions(&w.cv_home, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = std::panic::catch_unwind(|| {
        assert_eq!(writer_versions()[0].newest.as_deref(), Some("2.1.296"));
        observe_and_sync(&Context::current(), &r, Some("9.0.0"));
        assert_eq!(writer_versions()[0].newest.as_deref(), Some("2.1.296"));
    });
    std::fs::set_permissions(&w.cv_home, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (path, mode) in modes {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    result.unwrap();
}
