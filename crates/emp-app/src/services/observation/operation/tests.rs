use super::*;

#[test]
fn unavailable_journal_and_mismatches_never_change_or_repeat_execution() {
    let root = emp_test_support::fixture_dir().unwrap();
    std::fs::write(root.path().join("logs"), b"fixture").unwrap();
    let diagnostics = crate::tests::attached_diagnostics(root.path());
    let mut executions = 0;
    let result: Result<usize, &str> = observe(&diagnostics, None, "fixture", |receipt| {
        receipt.check("expected_matches_actual", Some(false));
        receipt.step("execute_once", || {
            executions += 1;
            Err("private-failure")
        })
    });
    assert_eq!(result, Err("private-failure"));
    assert_eq!(executions, 1);
    let result = observe(&diagnostics, None, "fixture", |receipt| {
        receipt.check("expected_matches_actual", Some(false));
        receipt.step("execute_once", || {
            executions += 1;
            Ok::<_, ()>(42)
        })
    });
    assert_eq!(result, Ok(42));
    assert_eq!(executions, 2);
}
