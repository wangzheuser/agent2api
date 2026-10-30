use super::*;
use crate::server::{db::test_temp::TempDb, logging};
use std::sync::atomic::{AtomicUsize, Ordering};

fn append(store: &LogStore, message: &str) -> LogEntry {
    store
        .append(NewEntry {
            level: "info",
            category: "server",
            message,
            data: None,
            ts: None,
        })
        .unwrap()
}

#[test]
fn capacity_changes_prune_immediately_without_resetting_ids() {
    let (db, _temp) = TempDb::open("logs-dynamic-capacity");
    let max = Arc::new(AtomicUsize::new(1000));
    let current = max.clone();
    let store = LogStore::with_limits(
        Some(db.clone()),
        || 30,
        move || current.load(Ordering::SeqCst),
    );
    for n in 0..1010 {
        append(&store, &format!("entry {n}"));
    }
    assert_eq!(store.len(), 1000);
    let newest = store.query(&Query::default()).entries[0].id;
    max.store(2000, Ordering::SeqCst);
    assert_eq!(store.try_prune().unwrap(), 0);
    max.store(10, Ordering::SeqCst);
    assert_eq!(store.try_prune().unwrap(), 990);
    assert_eq!(store.query(&Query::default()).entries[0].id, newest);
    let restored = LogStore::with_limits(Some(db.clone()), || 30, || 10);
    assert_eq!(restored.len(), 10);
    assert!(append(&restored, "after reopen").id > newest);
    max.store(1, Ordering::SeqCst);
    store.try_prune().unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(append(&store, "latest").message, "latest");
    assert_eq!(store.len(), 1);
    db.with(|conn| conn.execute_batch("CREATE TRIGGER fail_log_prune BEFORE DELETE ON logs BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap().unwrap();
    assert!(store
        .append(NewEntry {
            level: "info",
            category: "server",
            message: "must roll back",
            data: None,
            ts: None
        })
        .is_none());
    assert_eq!(store.query(&Query::default()).entries[0].message, "latest");
}

#[test]
fn age_and_capacity_are_both_enforced_and_prune_errors_are_visible() {
    let (db, _temp) = TempDb::open("logs-age-and-count");
    let store = LogStore::with_limits(Some(db.clone()), || 1, || 1000);
    append(&store, "fresh");
    store
        .append(NewEntry {
            level: "info",
            category: "server",
            message: "old",
            data: None,
            ts: Some(1),
        })
        .unwrap();
    assert_eq!(store.len(), 1);
    db.with(|conn| conn.execute("UPDATE logs SET ts=1", []))
        .unwrap()
        .unwrap();
    assert_eq!(store.try_prune().unwrap(), 1);
    append(&store, "fresh after prune");
    assert_eq!(store.len(), 1);
    db.with(|conn| conn.execute_batch("UPDATE logs SET ts=1; CREATE TRIGGER fail_prune BEFORE DELETE ON logs BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap().unwrap();
    assert!(store.try_prune().is_err());
    assert_eq!(store.len(), 1);
    assert!(LogStore::with_db(None, || 30).try_prune().is_err());
}

#[test]
fn migration_preserves_legacy_messages_and_leaves_task_unassigned() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT, data TEXT);
        INSERT INTO logs VALUES(42,1,'info','server','İΣ Ä 中文',NULL); PRAGMA user_version=16;").unwrap();
    crate::server::db::schema::migrate(&conn).unwrap();
    crate::server::db::schema::migrate(&conn).unwrap();
    let (message, search, task): (String, String, Option<String>) = conn
        .query_row(
            "SELECT message,message_search,task_id FROM logs WHERE id=42",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(message, "İΣ Ä 中文");
    assert_eq!(search, message.to_lowercase());
    assert!(task.is_none());
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_index_info('idx_logs_task_id')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}

#[tokio::test]
async fn concurrent_task_context_survives_spawn_and_never_leaks() {
    let (db, _temp) = TempDb::open("logs-task-context");
    let store = Arc::new(LogStore::with_db(Some(db), || 30));
    let a = store.clone();
    let b = store.clone();
    let one = logging::task_scope("zcodeAutoClaim", async move {
        tokio::task::yield_now().await;
        crate::spawn_task(async move {
            append(&a, "zcode child");
        })
        .await
        .unwrap();
    });
    let two = logging::task_scope("credentialMaintenance", async move {
        tokio::task::yield_now().await;
        crate::spawn_task(async move {
            append(&b, "maintenance child");
        })
        .await
        .unwrap();
    });
    tokio::join!(one, two);
    append(&store, "unassigned");
    assert!(logging::current_task_id().is_none());
    for (task, expected) in [
        ("zcodeAutoClaim", "zcode child"),
        ("credentialMaintenance", "maintenance child"),
    ] {
        let result = store.query(&Query {
            task_id: Some(task.into()),
            ..Query::default()
        });
        assert_eq!(result.matched, 1);
        assert_eq!(result.entries[0].message, expected);
        assert_eq!(result.entries[0].task_id.as_deref(), Some(task));
    }
    assert_eq!(
        store
            .query(&Query {
                task_id: Some("unknown".into()),
                ..Query::default()
            })
            .matched,
        0
    );
    let error: Result<(), &str> =
        logging::task_scope("zcodeAutoClaim", async { Err("fixture") }).await;
    assert!(error.is_err() && logging::current_task_id().is_none());
    let child = crate::spawn_task(logging::task_scope(
        "zcodeAutoClaim",
        std::future::pending::<()>(),
    ));
    child.abort();
    assert!(child.await.unwrap_err().is_cancelled());
    assert!(append(&store, "after cancellation").task_id.is_none());
}

#[test]
fn hundred_thousand_logs_page_filter_export_and_delete_without_full_materialization() {
    let (db, _temp) = TempDb::open("logs-100k");
    let store = LogStore::with_limits(Some(db.clone()), || 30, || 100_000);
    let now = logging::now_ms();
    db.with_mut(|conn| {
        let tx = conn.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO logs(id,ts,level,category,message,data,task_id,message_search) VALUES(?1,?2,?3,'server',?4,NULL,?5,?6)").unwrap();
            for id in 1..=100_000 {
                let message = format!("İΣ Ä 中文 {id}");
                insert.execute(rusqlite::params![id, now, if id % 2 == 0 { "error" } else { "info" }, message,
                    if id % 2 == 0 { Some("zcodeAutoClaim") } else { None }, message.to_lowercase()]).unwrap();
            }
        }
        tx.commit().unwrap();
    }).unwrap();
    let started = std::time::Instant::now();
    let first = store.query(&Query {
        limit: Some(50),
        ..Query::default()
    });
    for size in [500, 1000, 2000, 5000, 10000] {
        assert_eq!(
            store
                .query(&Query {
                    limit: Some(size),
                    ..Default::default()
                })
                .entries
                .len(),
            size
        );
    }
    assert_eq!(
        store
            .query(&Query {
                limit: Some(100000),
                ..Default::default()
            })
            .entries
            .len(),
        10000
    );
    assert_eq!(
        (
            first.total,
            first.matched,
            first.entries.len(),
            first.snapshot_id
        ),
        (100_000, 100_000, 50, 100_000)
    );
    let second_query = Query {
        limit: Some(50),
        offset: 50,
        snapshot_id: Some(first.snapshot_id),
        ..Query::default()
    };
    let second = store.query(&second_query);
    append(&store, "concurrent insert");
    assert_eq!(
        store
            .query(&second_query)
            .entries
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        second.entries.iter().map(|e| e.id).collect::<Vec<_>>()
    );
    let filtered = Query {
        task_id: Some("zcodeAutoClaim".into()),
        keyword: Some("İΣ ä 中".into()),
        level: Some("error".into()),
        category: Some("server".into()),
        start: Some(now),
        end: Some(now + 1),
        limit: Some(20),
        offset: 20_000,
        ..Query::default()
    };
    let result = store.query(&filtered);
    assert_eq!((result.matched, result.entries.len()), (50_000, 20));
    assert!(result.entries.iter().all(|e| e.level == "error"));
    let mut lines = 0;
    let mut largest = 0;
    let exported = store
        .export_jsonl(|batch| {
            let count = batch.lines().count();
            lines += count;
            largest = largest.max(count);
            for line in batch.lines() {
                serde_json::from_str::<Value>(line).unwrap();
            }
            Ok(())
        })
        .unwrap();
    assert_eq!((exported, lines, largest), (100_000, 100_000, 500));
    let (removed, stats) = store.clear_where(&filtered);
    assert_eq!((removed, stats.total), (50_000, 50_000));
    assert!(append(&store, "after filtered delete").id > 100_001);
    assert!(
        store
            .query(&Query {
                limit: Some(100_000),
                ..Query::default()
            })
            .entries
            .len()
            <= MAX_QUERY_LIMIT
    );
    eprintln!("100k query/export/delete elapsed: {:?}", started.elapsed());
}

#[test]
fn export_snapshot_survives_append_prune_and_clear_without_mixing_generations() {
    let (db, _temp) = TempDb::open("logs-export-snapshot");
    let max = Arc::new(AtomicUsize::new(2000));
    let current = max.clone();
    let store = LogStore::with_limits(Some(db), || 30, move || current.load(Ordering::SeqCst));
    for n in 0..1200 {
        append(&store, &format!("original {n}"));
    }
    let mut received = Vec::new();
    let count = store
        .export_jsonl(|batch| {
            let first = received.is_empty();
            received.extend(
                batch
                    .lines()
                    .map(|line| serde_json::from_str::<LogEntry>(line).unwrap().message),
            );
            if first {
                append(&store, "must not appear");
                max.store(1, Ordering::SeqCst);
                store.try_prune().unwrap();
                store.clear();
                append(&store, "reused id must not appear");
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 1200);
    assert_eq!(
        received,
        (0..1200)
            .map(|n| format!("original {n}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(store.len(), 1);
    assert!(store.export_jsonl(|_| Err("cancelled".into())).is_err());
    append(&store, "write after cancelled export");
}

#[test]
fn cleared_or_restored_log_ids_invalidate_history_page_snapshots() {
    let (db, _temp) = TempDb::open("logs-pagination-generation");
    let store = LogStore::with_db(Some(db), || 30);
    for _ in 0..100 {
        append(&store, "old generation");
    }
    let old = store.query(&Query::default());
    store.clear();
    for _ in 0..120 {
        append(&store, "new generation");
    }
    let page = store.query(&Query {
        snapshot_id: Some(old.snapshot_id),
        snapshot_epoch: Some(old.snapshot_epoch),
        offset: 50,
        limit: Some(50),
        ..Default::default()
    });
    assert!(page.snapshot_reset);
    assert_eq!(page.entries[0].id, 120);
    store.invalidate_snapshot();
    assert!(
        store
            .query(&Query {
                snapshot_epoch: Some(page.snapshot_epoch),
                ..Default::default()
            })
            .snapshot_reset
    );
}

#[test]
fn restore_boundary_prevents_append_and_prune_from_caching_old_limits() {
    let (db, _temp) = TempDb::open("logs-restore-boundary");
    let max = Arc::new(AtomicUsize::new(1000));
    let current = max.clone();
    let (observed, values) = std::sync::mpsc::channel();
    let store = Arc::new(LogStore::with_limits(
        Some(db.clone()),
        || 30,
        move || {
            let value = current.load(Ordering::SeqCst);
            observed.send(value).unwrap();
            value
        },
    ));
    // 与真实恢复相同的排他边界：阻止已排队的写入预先捕获旧容量。
    let guard = store.guard();
    let (started, ready) = std::sync::mpsc::channel();
    let handles = [true, false].map(|write| {
        let store = store.clone();
        let started = started.clone();
        std::thread::spawn(move || {
            started.send(()).unwrap();
            if write {
                append(&store, "after restore");
            } else {
                store.try_prune().unwrap();
            }
        })
    });
    ready.recv().unwrap();
    ready.recv().unwrap();
    assert!(values
        .recv_timeout(std::time::Duration::from_millis(100))
        .is_err());
    db.with_mut(|conn| {
        let tx = conn.transaction().unwrap();
        for id in 1..=1500 {
            tx.execute("INSERT INTO logs(id,ts,level,category,message,message_search) VALUES(?1,?2,'info','server','restored','restored')",
                rusqlite::params![id, logging::now_ms()]).unwrap();
        }
        tx.commit().unwrap();
    }).unwrap();
    max.store(100000, Ordering::SeqCst);
    store.invalidate_snapshot();
    drop(guard);
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(values.recv().unwrap(), 100000);
    assert_eq!(values.recv().unwrap(), 100000);
    assert_eq!(store.len(), 1501);
}
