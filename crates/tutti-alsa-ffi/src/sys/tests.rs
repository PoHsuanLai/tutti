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

/// A real-time stamp reads as its two words; a tick stamp, or none, reads as
/// `None`.
#[test]
fn a_real_time_stamp_reads_from_the_time_blob() {
    let mut ev = snd_seq_ump_event_t::default();
    ev.time[..4].copy_from_slice(&7u32.to_ne_bytes());
    ev.time[4..].copy_from_slice(&500u32.to_ne_bytes());
    assert_eq!(ev.real_time(), None, "flags say tick");
    ev.flags = SND_SEQ_TIME_STAMP_REAL;
    assert_eq!(
        ev.real_time(),
        Some(snd_seq_real_time_t {
            tv_sec: 7,
            tv_nsec: 500
        })
    );
    ev.flags = 0b10; // relative-time mode, tick stamp
    assert_eq!(ev.real_time(), None);
}
