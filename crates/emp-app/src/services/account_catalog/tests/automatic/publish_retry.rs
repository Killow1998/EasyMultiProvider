use super::*;

#[test]
fn failed_merged_catalog_write_recovers_from_unchanged_source_cache() {
    let upstream = RefreshCatalogFixture::start(false);
    let directory = tempfile::Builder::new()
        .prefix("emp-auto-catalog-publish-retry-")
        .tempdir()
        .expect("temporary directory");
    let (server, auth_path) = start_server(&directory, &upstream.address, "0.159.2");
    let generated_path = crate::services::catalog::generated_catalog_path(&server.state);
    std::fs::remove_file(&generated_path).expect("remove initial generated catalog");
    std::fs::create_dir(&generated_path).expect("block generated catalog replacement");

    crate::services::account_catalog::request_refresh(&server.state, false);
    let request = upstream.take_request();
    assert!(request.contains("Bearer selected-secret"));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    let source_cache: Value = serde_json::from_slice(
        &std::fs::read(auth_path.parent().unwrap().join("models_cache.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(source_cache["models"][0]["slug"], "first-account-model");
    assert!(generated_path.is_dir());
    assert!(server.state.catalog_refresh.catalog_publication_pending());

    std::fs::remove_dir(&generated_path).expect("unblock generated catalog replacement");
    crate::services::account_catalog::request_refresh(&server.state, false);
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    assert!(
        upstream.requests.try_recv().is_err(),
        "retry uses cached source data"
    );
    assert!(generated_path.is_file());
    assert!(!server.state.catalog_refresh.catalog_publication_pending());
    let generated: Value = serde_json::from_slice(&std::fs::read(generated_path).unwrap()).unwrap();
    assert!(
        generated["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "demo/first-account-model")
    );
    server.shutdown().expect("shutdown");
}
