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
