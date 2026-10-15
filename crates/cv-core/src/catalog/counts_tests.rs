use super::*;
use crate::source::{Probe, ReadSet};

fn publish(r: &SessionRef, n: u32, reads: &ReadSet) {
    let source = Probe::default().snapshot(r).with_reads(reads);
    note_affordances_with_source(r, &source, || {
        [(crate::affordance::Kind::Plan, n)].into_iter().collect()
    });
}

fn compare(reader: &Reader, r: &SessionRef, expected: Option<u32>) {
    let source = Probe::default().snapshot(r);
    let demand = reader.counts_with_probe(r, &source, &mut Probe::default());
    let batch = AffordanceTable::load().counts_with_source(r, &source);
    assert_eq!(demand, batch);
    assert_eq!(
        demand.map(|c| c.get(&crate::affordance::Kind::Plan).copied().unwrap_or(0)),
        expected
    );
}

#[test]
fn demand_counts_match_batch_baselines_and_reject_stale_pairs() {
    let w = crate::test_world("counts-demand-baselines");
    let path = w.base.join("sessions.db");
    std::fs::write(&path, "database").unwrap();
    let r = SessionRef::new("selected", crate::Harness::Hermes, path.clone());
    let conn = open_db().unwrap();
    let reader = Reader::open().unwrap();
    compare(&reader, &r, None);
    publish(&r, 1, &ReadSet::default());
    compare(&reader, &r, Some(1));
    for mutation in [
        "UPDATE affordance_counts SET counts='{}'",         // missing view version
        "UPDATE affordance_counts SET counts='{\"_v\":0}'", // old view version
        "UPDATE affordance_counts SET counts='malformed'",
        "UPDATE affordance_counts SET sig_b=sig_b+1",
        "UPDATE source_dependencies SET size=size+1 WHERE consumer='counts'",
        "UPDATE source_dependencies SET stamp='{}' WHERE consumer='counts'",
        "UPDATE source_dependencies SET stamp='{\"version\":1}' WHERE consumer='counts'",
        "UPDATE source_dependencies SET stamp=REPLACE(stamp,'\"reads\":[]','\"reads\":null') WHERE consumer='counts'",
        "DELETE FROM source_dependencies WHERE consumer='counts'",
        "DELETE FROM affordance_counts",
    ] {
        conn.execute_batch(mutation).unwrap();
        compare(&reader, &r, None);
        conn.execute_batch("DELETE FROM affordance_counts; DELETE FROM source_dependencies WHERE consumer='counts';")
            .unwrap();
        publish(&r, 1, &ReadSet::default());
        compare(&reader, &r, Some(1));
    }
    let wal = w.base.join("sessions.db-wal");
    std::fs::write(&wal, "committed change").unwrap();
    compare(&reader, &r, None);
    std::fs::remove_file(&wal).unwrap();
    compare(&reader, &r, Some(1));
    let conditional = w.base.join("conditional-sidecar");
    let mut reads = ReadSet::default();
    reads.record(&conditional); // Captured absence is part of the selected body's provenance.
    conn.execute_batch("DELETE FROM affordance_counts; DELETE FROM source_dependencies WHERE consumer='counts';")
        .unwrap();
    publish(&r, 1, &reads);
    compare(&reader, &r, Some(1));
    std::fs::write(&conditional, "appeared").unwrap();
    compare(&reader, &r, None);
    std::fs::write(&path, "changed primary").unwrap();
    compare(&reader, &r, None);
    // A retained reader does not pin a transaction across reductions/publications.
    publish(&r, 2, &ReadSet::default());
    compare(&reader, &r, Some(2));
    // Uncommitted writes cannot expose one half of a count/provenance publication.
    conn.execute_batch("BEGIN IMMEDIATE; UPDATE affordance_counts SET counts='{}'; DELETE FROM source_dependencies WHERE consumer='counts';").unwrap();
    assert_eq!(
        reader
            .counts_with_probe(&r, &Probe::default().snapshot(&r), &mut Probe::default())
            .unwrap()
            .get(&crate::affordance::Kind::Plan),
        Some(&2)
    );
    conn.execute_batch("COMMIT;").unwrap();
    compare(&reader, &r, None);
}

#[test]
fn demand_counts_decode_only_selected_rows_as_inventory_grows() {
    let w = crate::test_world("counts-demand-work");
    let path = w.base.join("selected.jsonl");
    std::fs::write(&path, "selected").unwrap();
    let r = SessionRef::new("selected", crate::Harness::Codex, path);
    let conn = open_db().unwrap();
    publish(&r, 1, &ReadSet::default());
    for inventory in [8, 1024] {
        conn.execute(
            "WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<?1)
             INSERT OR REPLACE INTO affordance_counts(harness,id,path,sig_a,sig_b,counts)
             SELECT 'codex','irrelevant-'||n,'unused-'||n,1,1,'malformed unrelated counts' FROM numbers",
            [inventory],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO source_dependencies(consumer,harness,id,path,mtime,size,stamp)
             SELECT 'counts',harness,id,path,1,1,'malformed unrelated stamp' FROM affordance_counts WHERE id LIKE 'irrelevant-%'",
            [],
        ).unwrap();
        let reader = Reader::open().unwrap();
        compare(&reader, &r, Some(1));
        assert_eq!(reader.counts_work(), (1, 1));
        let missing = SessionRef::new("missing", r.harness, r.path.clone());
        compare(&reader, &missing, None);
        assert_eq!(reader.counts_work(), (2, 1));
    }
}
