use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

fn item(shop: &str, id: &str, product: &str, kind: &str) -> Item {
    Item {
        shop: shop.into(),
        id: id.into(),
        kind: kind.into(),
        product_ids: vec![product.into()],
        scope: "product".into(),
        sku: "0123".into(),
        model: "同名".into(),
        question: "如何申请退款服务".into(),
        answer: "联系店铺".into(),
        source: String::new(),
        enabled: true,
    }
}

fn upsert(item: Item) -> Change {
    Change {
        shop: item.shop.clone(),
        id: item.id.clone(),
        item: Some(item),
    }
}

fn delete(shop: &str, id: &str) -> Change {
    Change {
        shop: shop.into(),
        id: id.into(),
        item: None,
    }
}

#[test]
fn exact_lookup_is_scoped_and_keeps_ambiguous_models() -> Result<()> {
    let e = Engine::open(Path::new(":memory:"))?;
    let big = "000184467440737095516160";
    let a = item("a", "1", big, "sku");
    let mut b = item("a", "2", big, "sku");
    b.sku = "0124".into();
    let mut disabled = item("a", "3", big, "sku");
    disabled.enabled = false;
    e.apply(vec![
        upsert(a.clone()),
        upsert(b.clone()),
        upsert(disabled),
        upsert(item("b", "1", big, "sku")),
        upsert(item("a", "4", "another", "sku")),
    ])?;
    assert_eq!(e.lookup("a", big, "", "同名").1, vec![a.clone(), b]);
    assert_eq!(e.lookup("a", big, "0123", " 同名 ").1, vec![a.clone()]);
    assert!(e.lookup("a", big, "0123", "different").1.is_empty());
    assert!(
        e.lookup("a", big.trim_start_matches('0'), "0123", "")
            .1
            .is_empty()
    );
    assert_eq!(e.lookup("b", big, "", "").1.len(), 1);
    let mut rebound = a.clone();
    rebound.product_ids = vec!["other".into()];
    e.apply(vec![upsert(rebound)])?;
    assert!(e.lookup("a", big, "0123", "").1.is_empty());
    assert_eq!(e.lookup("a", "other", "0123", "").1.len(), 1);
    e.apply(vec![delete("a", "1")])?;
    assert!(e.lookup("a", "other", "", "").1.is_empty());
    Ok(())
}

#[test]
fn tokenized_search_filters_before_top_k_and_replaces_old_text() -> Result<()> {
    let e = Engine::open(Path::new(":memory:"))?;
    let own = item("a", "ok", "p", "answer");
    let mut shared = item("a", "shared", "p", "answer");
    shared.scope = "shop".into();
    shared.product_ids.clear();
    let mut disabled = item("a", "disabled", "p", "answer");
    disabled.enabled = false;
    let mut batch = vec![
        upsert(own.clone()),
        upsert(shared.clone()),
        upsert(disabled),
    ];
    for i in 0..25 {
        let mut foreign = item("b", &format!("f{i}"), "p", "answer");
        foreign.question = "退款".repeat(20);
        batch.push(upsert(foreign));
        batch.push(upsert(item("a", &format!("wrong{i}"), "another", "answer")));
    }
    e.apply(batch)?;
    let (_, visible) = e.search("a", "p", "退款", 2);
    let ids: std::collections::BTreeSet<_> = visible.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["ok", "shared"].into_iter().collect());
    assert_eq!(e.search("a", "", "退款", 10).1, vec![shared]);
    assert!(e.search("a", "p", "", 10).1.is_empty());
    assert!(e.search("a", "p", "退款", 0).1.is_empty());
    let mut replacement = own;
    replacement.question = "怎么清洁耳机".into();
    e.apply(vec![upsert(replacement.clone()), delete("a", "shared")])?;
    assert!(e.search("a", "p", "退款", 10).1.is_empty());
    assert_eq!(e.search("a", "p", "洁耳", 2).1, vec![replacement]);
    Ok(())
}

#[test]
fn invalid_batch_rolls_back_and_retries_do_not_bump_version() -> Result<()> {
    let e = Engine::open(Path::new(":memory:"))?;
    let a = item("a", "1", "p", "sku");
    let mut bad = item("a", "2", "p", "answer");
    bad.product_ids.clear();
    assert!(e.apply(vec![upsert(a.clone()), upsert(bad)]).is_err());
    assert_eq!(e.lookup("a", "p", "", ""), (0, vec![]));
    let mut dup = a.clone();
    dup.product_ids.push("p".into());
    assert_eq!(e.apply(vec![upsert(dup)])?, 1);
    assert_eq!(
        e.apply(vec![
            delete("a", "unknown"),
            delete("a", "1"),
            upsert(a.clone())
        ])?,
        1
    );
    assert_eq!(e.lookup("a", "p", "", "").1, vec![a]);
    assert!(serde_json::from_str::<Change>(r#"{"shop":"a","id":"1"}"#).is_err());
    assert!(
        serde_json::from_str::<Item>(r#"{"shop":"a","id":"1","kind":"answer","scope":"shop"}"#)
            .is_err()
    );
    Ok(())
}

#[test]
fn durable_version_survives_restart() -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kb-engine-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    let path = dir.join("store.sqlite");
    let a = item("a", "1", "p", "answer");
    {
        let e = Engine::open(&path)?;
        assert_eq!(e.apply(vec![upsert(a.clone())])?, 1);
    }
    {
        let e = Engine::open(&path)?;
        assert_eq!(e.search("a", "p", "退款", 1), (1, vec![a]));
        e.apply(vec![delete("a", "1")])?;
    }
    {
        let e = Engine::open(&path)?;
        assert_eq!(e.search("a", "p", "退款", 1), (2, vec![]));
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn readers_observe_one_generation_during_concurrent_writes() -> Result<()> {
    let engine = Arc::new(Engine::open(Path::new(":memory:"))?);
    let mut a = item("a", "one", "p", "answer");
    a.answer = "1".into();
    engine.apply(vec![upsert(a.clone())])?;
    let first = engine.view.load_full();
    let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reader = {
        let e = engine.clone();
        let active = active.clone();
        std::thread::spawn(move || {
            loop {
                let (v, records) = e.search("a", "p", "退款", 1);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].answer, v.to_string());
                if !active.load(Ordering::Acquire) {
                    break;
                }
            }
        })
    };
    for v in 2..=5 {
        a.answer = v.to_string();
        assert_eq!(engine.apply(vec![upsert(a.clone())])?, v);
    }
    active.store(false, Ordering::Release);
    reader.join().unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(first.text.find("a", "p", "退款", 1)?, vec!["one"]);
    assert_eq!(
        first
            .catalog
            .items
            .get(&("a".into(), "one".into()))
            .unwrap()
            .answer,
        "1"
    );
    Ok(())
}

#[test]
fn failed_sql_commit_never_exposes_uncommitted_index_and_next_write_recovers() -> Result<()> {
    let e = Engine::open(Path::new(":memory:"))?;
    let good = item("a", "ok", "p", "answer");
    e.apply(vec![upsert(good.clone())])?;
    // A deferred constraint moves the error to COMMIT, after Tantivy has
    // published privately. Verify both the pre-failure and recovered views.
    e.writer.lock().unwrap().db.execute_batch(
        "
        PRAGMA foreign_keys=ON;
        CREATE TABLE allowed(id INTEGER PRIMARY KEY);
        CREATE TABLE dependent(id INTEGER REFERENCES allowed(id) DEFERRABLE INITIALLY DEFERRED);
        CREATE TRIGGER fail_commit AFTER INSERT ON kb_items WHEN NEW.id='bad'
        BEGIN INSERT INTO dependent VALUES(1); END;",
    )?;
    assert!(
        e.apply(vec![
            upsert(item("a", "bad", "p", "answer")),
            delete("a", "ok")
        ])
        .is_err()
    );
    assert_eq!(e.search("a", "p", "退款", 3), (1, vec![good.clone()]));
    e.apply(vec![upsert(item("z", "ok", "p", "answer"))])?;
    assert_eq!(e.search("a", "p", "退款", 3), (2, vec![good]));
    Ok(())
}
