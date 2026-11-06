use super::*;

/// A zeroed event is the "nothing set" starting point the send path fills
/// in; if `Default` ever stopped being all-zero, a stale `dest` would send
/// events to the wrong port.
#[test]
fn a_default_event_is_entirely_zero() {
    let ev = snd_seq_ump_event_t::default();
    assert_eq!(ev.type_, 0);
    assert_eq!(ev.ump, [0; 4]);
    assert_eq!(ev.source, snd_seq_addr_t::default());
    assert_eq!(ev.dest, snd_seq_addr_t::default());
}
