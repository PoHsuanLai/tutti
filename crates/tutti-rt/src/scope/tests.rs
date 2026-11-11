use super::*;

/// The mark lasts exactly as long as the guard, nests, and is per thread.
#[test]
fn the_mark_is_scoped_nested_and_per_thread() {
    assert!(!RtScope::is_inside());
    {
        let _outer = RtScope::enter();
        assert!(RtScope::is_inside());
        {
            let _inner = RtScope::enter();
            assert!(RtScope::is_inside());
        }
        assert!(RtScope::is_inside(), "the inner guard ends only itself");
        let other = std::thread::spawn(RtScope::is_inside)
            .join()
            .expect("thread ran");
        assert!(!other, "another thread is not marked");
    }
    assert!(!RtScope::is_inside());
}
