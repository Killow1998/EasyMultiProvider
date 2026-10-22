use super::*;

#[test]
fn parent_metadata_handles_ephemeral_review_sidechat_and_nested_agents_read_only() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state_5.sqlite");
    let database = rusqlite::Connection::open(&path).unwrap();
    database.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,source TEXT,thread_source TEXT);
        CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT);
        INSERT INTO threads VALUES('main','Saved title','\"vscode\"','user');
        INSERT INTO threads VALUES('review','Guardian review','{\"subagent\":{\"other\":\"guardian\"}}','guardian_review');
        INSERT INTO threads VALUES('child','Nested agent','{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"sidechat\"}}}',NULL);
        INSERT INTO threads VALUES('sidechat','Side task','\"vscode\"','user');
        INSERT INTO thread_spawn_edges VALUES('main','review'),('main','ephemeral');").unwrap();
    drop(database);
    let original = std::fs::read(&path).unwrap();
    std::fs::write(
        root.path().join("session_index.jsonl"),
        b"{\"id\":\"main\",\"thread_name\":\"Renamed main\"}\n",
    )
    .unwrap();
    let directory = SessionDirectory::default();
    let mut scope = SessionScope::default();
    for id in ["review", "child", "ephemeral", "orphan"] {
        scope.observe(id, "", "");
    }
    scope.observe("sidechat", "main", "sidechat");
    directory.enrich(root.path(), &mut scope);
    for id in ["review", "child", "ephemeral", "sidechat"] {
        assert_eq!(scope.root(id), Some("main"));
    }
    assert_eq!(scope.root("orphan"), None);
    assert_eq!(scope.0["main"].name, "Renamed main");
    assert_eq!(scope.0["review"].kind, "review");
    assert_eq!(scope.0["sidechat"].kind, "sidechat");
    assert_eq!(scope.0["child"].kind, "subagent");
    assert_eq!(scope.0["ephemeral"].kind, "subagent");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    // Parent edges can arrive while session_index stays unchanged.
    let database = rusqlite::Connection::open(&path).unwrap();
    database
        .execute("INSERT INTO thread_spawn_edges VALUES('main','orphan')", [])
        .unwrap();
    drop(database);
    directory.enrich(root.path(), &mut scope);
    assert_eq!(scope.root("orphan"), Some("main"));
}

#[test]
fn conflicting_edges_and_cycles_do_not_create_fake_main_conversations() {
    let root = tempfile::tempdir().unwrap();
    let database = rusqlite::Connection::open(root.path().join("state_5.sqlite")).unwrap();
    database
        .execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT);
        CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT);
        INSERT INTO threads VALUES('one','One'),('two','Two'),('review','Guardian review');
        INSERT INTO thread_spawn_edges VALUES('one','ambiguous'),('two','ambiguous'),
        ('cycle-two','cycle-one'),('cycle-one','cycle-two');",
        )
        .unwrap();
    drop(database);
    let mut scope = SessionScope::default();
    for id in ["one", "ambiguous", "cycle-one"] {
        scope.observe(id, "", "");
    }
    scope.observe("review", "", "review");
    SessionDirectory::default().enrich(root.path(), &mut scope);
    assert_eq!(scope.root("one"), Some("one"));
    assert_eq!(scope.root("ambiguous"), None);
    assert_eq!(scope.root("cycle-one"), None);
    assert_eq!(scope.root("review"), None);
}
