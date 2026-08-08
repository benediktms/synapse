use daemon::single_instance;

#[test]
fn single_instance_holds_and_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let first = single_instance::acquire(dir.path()).expect("first daemon acquires");
    // A second acquire on the same lock file must fail while the first is held.
    assert!(single_instance::acquire(dir.path()).is_err());
    drop(first);
    // After the first is released, a new daemon can acquire.
    assert!(single_instance::acquire(dir.path()).is_ok());
}
