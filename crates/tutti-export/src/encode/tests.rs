use super::*;

/// A temporary that cannot be removed is logged, not an error and not a
/// panic: the render's outcome (a cancel, say) is already decided. A
/// directory in the temporary's place stands for any removal that fails.
///
/// Mutation (run): `Staged::drop` unwrapping the removal -> the drop panics
/// on the directory; fails.
#[test]
fn a_temporary_that_cannot_be_removed_does_not_fail_the_drop() {
    let dir = tempfile::tempdir().unwrap();
    let staged = Staged::beside(&dir.path().join("a.wav"));
    std::fs::create_dir(staged.path()).unwrap();
    let path = staged.path().to_path_buf();
    drop(staged);
    assert!(
        path.is_dir(),
        "the leftover is still there, and was reported"
    );
}

/// The temporary sits beside its target (a rename within one directory) and
/// two stagings of one target do not share a name.
#[test]
fn the_temporary_is_beside_the_target_and_unique() {
    let target = std::path::Path::new("some/dir/a.wav");
    let (a, b) = (Staged::beside(target), Staged::beside(target));
    assert_eq!(a.path().parent(), target.parent());
    assert_ne!(a.path(), b.path());
}
