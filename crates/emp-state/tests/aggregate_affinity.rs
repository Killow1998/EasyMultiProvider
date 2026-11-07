use emp_state::usage::{ledger::UsageLedger, pricing::PriceCatalog};
use std::sync::Arc;

#[test]
fn account_affinity_survives_reordering_revocation_and_restart_without_stealing_existing_turns() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let original = vec!["account-a".into(), "account-b".into(), "account-c".into()];
    let reordered = vec!["account-c".into(), "account-a".into(), "account-b".into()];
    let ledger = UsageLedger::new(path.clone(), Arc::clone(&prices));
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "first", "auto/model", &original, true)
            .unwrap(),
        0
    );
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "first", "auto/model", &reordered, true)
            .unwrap(),
        1
    );
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "second", "auto/model", &original, true)
            .unwrap(),
        1
    );
    drop(ledger);
    let ledger = UsageLedger::new(path, prices);
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "third", "auto/model", &original, true)
            .unwrap(),
        2
    );
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "first", "auto/model", &reordered, false)
            .unwrap(),
        1
    );
    // A different caller using the same conversation identifier gets its own binding.
    assert_eq!(
        ledger
            .bind_aggregate("laptop", "first", "auto/model", &reordered, false)
            .unwrap(),
        0
    );
    // Exhausted/unavailable owners are omitted by routing; only that conversation moves.
    let available = vec!["account-b".into(), "account-c".into()];
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "first", "auto/model", &available, false)
            .unwrap(),
        0
    );
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "first", "auto/model", &original, false)
            .unwrap(),
        1
    );
    assert_eq!(
        ledger
            .bind_aggregate("desktop", "third", "auto/model", &original, false)
            .unwrap(),
        2
    );
}
