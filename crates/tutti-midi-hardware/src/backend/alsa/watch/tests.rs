use super::*;

#[test]
fn a_departed_client_names_all_its_ports_and_a_port_only_itself() {
    let id = |c: u64, p: u64| EndpointId::from_raw((c << 32) | p);
    assert!(Departed::Client(130).names(id(130, 0)));
    assert!(Departed::Client(130).names(id(130, 3)));
    assert!(!Departed::Client(130).names(id(131, 0)));
    assert!(Departed::Port(130, 1).names(id(130, 1)));
    assert!(!Departed::Port(130, 1).names(id(130, 0)));
    assert!(!Departed::Port(130, 1).names(id(131, 1)));
}

/// Errors only end a drain when they come in a row. Mutation: `read` not
/// resetting the count ends it after eight errors in total.
#[test]
fn scattered_errors_do_not_end_a_drain_but_a_run_of_them_does() {
    let mut errors = ErrorRun::default();
    for _ in 0..100 {
        assert!(!errors.failed());
        errors.read();
    }
    for _ in 0..MAX_DRAIN_ERRORS - 1 {
        assert!(!errors.failed());
    }
    assert!(errors.failed(), "the {MAX_DRAIN_ERRORS}th in a row ends it");
}
