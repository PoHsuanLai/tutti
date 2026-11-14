//! The session follows a MIDI clock a fake device sends: Beat Clock drives the
//! tempo and the play state, a Song Position Pointer locates, and MTC drives the
//! position; the follower's lock is reported and a dropout unlocks it.
//!
//! The oracles are arithmetic: 24 Timing Clocks to a quarter note (M1 p.30), a
//! pointer of `n` is beat `n / 4` (M1 p.27), an MTC position in seconds is a
//! beat through the tempo map (2 s at 120 bpm is beat 4).

use crate::common::blocks;
use std::time::{Duration, Instant};

use tutti_midi_hardware::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_midi_runtime::{ClockKind, MidiClockDecoder};
use tutti_session::{
    FollowClock, FollowTempo, MidiError, MidiEvent, Session, SessionEvent, TempoError,
};
use tutti_transport::MotionEvent;
use tutti_types::{At, Bpm, ChannelLayout, MaxBlock, SampleRate, Samples};
use tutti_ump::{Message, MidiGroup, Sysex7Packets, System, U7, U14, Ump};

const RATE: f64 = 48_000.0;
const BLOCK: f64 = 512.0;
const KEYS: u64 = 1;
const G: MidiGroup = MidiGroup::FIRST;

fn system(message: System) -> Ump {
    Message::System(G, message).into()
}

fn rig() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![endpoint(KEYS, "Keys")], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend)
        .build_offline(
            SampleRate::of(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let _ = s.update();
    s.midi().connect_input("keys").expect("connects");
    let _ = s.update();
    (s, fake)
}

fn follow(s: &mut Session, kind: ClockKind) {
    s.midi()
        .follow_clock(FollowClock { group: G, kind })
        .expect("follows");
    let _ = s.update();
}

fn send(fake: &FakeCounters, ump: Ump, at: Instant) {
    assert!(fake.send_input(EndpointId::from_raw(KEYS), ump, at));
}

/// Renders one block and runs one update; the MIDI events it reported.
fn pump(s: &mut Session) -> Vec<MidiEvent> {
    blocks(s, 1);
    s.update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(e) => Some(e),
            _ => None,
        })
        .collect()
}

/// Pumps until `done` holds of what an update reported, at most `limit` times.
fn until(s: &mut Session, limit: usize, done: impl Fn(&[MidiEvent]) -> bool) -> Vec<MidiEvent> {
    let mut all = Vec::new();
    for _ in 0..limit {
        all.extend(pump(s));
        if done(&all) {
            return all;
        }
    }
    panic!("not done in {limit} updates: {all:?}");
}

/// Timing Clocks `count` of them, `period` apart from `start`, sent at once.
fn send_ticks(fake: &FakeCounters, start: Instant, period: Duration, count: u32) {
    for i in 0..count {
        send(fake, system(System::TimingClock), start + period * i);
    }
}

fn quarter(piece: u8, nibble: u8) -> Ump {
    system(System::TimeCode(U7::of(piece << 4 | nibble)))
}

/// A cycle at 25 fps (rate code 1) spelling `00:00:ss:ff`, from `start`.
fn send_cycle(fake: &FakeCounters, s: u8, f: u8, start: Instant) {
    let nibbles = [f & 0xF, f >> 4, s & 0xF, s >> 4, 0, 0, 0, 1 << 1];
    for (k, n) in nibbles.iter().enumerate() {
        let at = start + Duration::from_secs_f64(k as f64 / 100.0);
        send(fake, quarter(k as u8, *n), at);
    }
}

/// Pumps until the playhead's rolling state is `rolling`, at most `limit` times.
fn until_rolling(s: &mut Session, rolling: bool, limit: usize) {
    for _ in 0..limit {
        pump(s);
        if s.playhead().playing() == rolling {
            return;
        }
    }
    panic!("the transport never became rolling = {rolling}");
}

/// **Start plays and Stop stops**: a Start from the followed input rolls the
/// transport from beat 0, and the Stop after it stops it where it stands.
#[test]
fn start_plays_from_zero_and_stop_stops() {
    let (mut s, fake) = rig();
    follow(&mut s, ClockKind::BeatClock);
    assert!(!s.playhead().playing());
    send(&fake, system(System::Start), Instant::now());
    until_rolling(&mut s, true, 50);
    assert!(s.playhead().beat().get() < 0.5, "{:?}", s.playhead().beat());
    send(&fake, system(System::Stop), Instant::now());
    until_rolling(&mut s, false, 50);
}

/// **Off by default**: without a follow a Start changes nothing.
#[test]
fn without_a_follow_a_start_changes_nothing() {
    let (mut s, fake) = rig();
    send(&fake, system(System::Start), Instant::now());
    for _ in 0..40 {
        pump(&mut s);
        assert!(!s.playhead().playing());
    }
}

/// **A Song Position Pointer locates a stopped transport to its sixteenths**:
/// a pointer of 20 sixteenths is beat 5, and the transport stays stopped.
#[test]
fn a_song_position_pointer_locates_to_its_sixteenths() {
    let (mut s, fake) = rig();
    follow(&mut s, ClockKind::BeatClock);
    send(
        &fake,
        system(System::SongPosition(U14::of(20))),
        Instant::now(),
    );
    for _ in 0..40 {
        pump(&mut s);
        if (s.playhead().beat().get() - 5.0).abs() < 1e-6 {
            assert!(!s.playhead().playing());
            return;
        }
    }
    panic!("beat {:?}, not 5", s.playhead().beat());
}

/// **A steady clock locks and sets the tempo; a silent one unlocks**: ticks at
/// 90 bpm spacing lock the follower, publish 90 bpm into the tempo map (reported
/// as `TempoFollowed`), and half a second of graph time without one unlocks it.
#[test]
fn a_steady_clock_locks_and_sets_the_tempo_and_a_dropout_unlocks() {
    let (mut s, fake) = rig();
    follow(&mut s, ClockKind::BeatClock);
    let period = Duration::from_secs_f64(60.0 / (90.0 * 24.0));
    // The engine's block clock must have a reading to place arrivals on frames.
    for _ in 0..8 {
        pump(&mut s);
    }
    send_ticks(&fake, Instant::now(), period, 60);
    let events = until(&mut s, 400, |e| {
        e.iter().any(|e| matches!(e, MidiEvent::ClockLocked { .. }))
            && e.iter()
                .any(|e| matches!(e, MidiEvent::TempoFollowed { .. }))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        MidiEvent::ClockLocked {
            kind: ClockKind::BeatClock
        }
    )));
    let tempo = events
        .iter()
        .find_map(|e| match e {
            MidiEvent::TempoFollowed { tempo, .. } => Some(*tempo),
            _ => None,
        })
        .unwrap();
    assert!(
        !tempo.differs_from(Bpm::of(90.0), Bpm::of(0.5)),
        "{tempo:?} for ticks at 90 bpm"
    );
    // The ticks end; a dropout's worth of blocks later the follower unlocks.
    let blocks_in_dropout = (MidiClockDecoder::DROPOUT.get() * RATE / BLOCK).ceil() as usize;
    let mut events = Vec::new();
    for _ in 0..(2 * blocks_in_dropout + 200) {
        events.extend(pump(&mut s));
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            MidiEvent::ClockUnlocked {
                kind: ClockKind::BeatClock
            }
        )),
        "{events:?}"
    );
}

/// **A Full Message locates**: `00:00:02:00` is 2 s, which is beat 4 at the
/// project's 120 bpm; the transport is stopped and stays so.
#[test]
fn an_mtc_full_message_locates() {
    let (mut s, fake) = rig();
    follow(&mut s, ClockKind::Mtc);
    for ump in Sysex7Packets::from_payload(G, &[0x7F, 0x7F, 0x01, 0x01, 1 << 5, 0, 2, 0])
        .expect("a 7-bit payload")
    {
        send(&fake, ump, Instant::now());
    }
    for _ in 0..40 {
        pump(&mut s);
        if (s.playhead().beat().get() - 4.0).abs() < 1e-6 {
            assert!(!s.playhead().playing());
            return;
        }
    }
    panic!("beat {:?}, not 4", s.playhead().beat());
}

/// **Quarter Frames lock and drive the playhead**: three consistent cycles at
/// 25 fps lock the follower and start the transport near the position they
/// spell (`00:00:10:00` plus the cycles' own 2 s ... 10 s is beat 20 at 120 bpm).
#[test]
fn mtc_quarter_frames_lock_and_play_from_their_position() {
    let (mut s, fake) = rig();
    follow(&mut s, ClockKind::Mtc);
    let start = Instant::now();
    for c in 0..4u8 {
        send_cycle(
            &fake,
            10,
            2 * c,
            start + Duration::from_millis(80 * u64::from(c)),
        );
    }
    let events = until(&mut s, 400, |e| {
        e.iter().any(|e| matches!(e, MidiEvent::ClockLocked { .. }))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        MidiEvent::ClockLocked {
            kind: ClockKind::Mtc
        }
    )));
    until_rolling(&mut s, true, 50);
    let beat = s.playhead().beat().get();
    assert!((beat - 20.0).abs() < 2.0, "near 10 s = beat 20, at {beat}");
}

/// **A Beat Clock follower owns the tempo; MTC does not**: while it follows,
/// the session refuses a tempo set and `follow_tempo`, and an MTC follow
/// refuses neither.
#[test]
fn a_beat_clock_follower_owns_the_tempo_and_mtc_does_not() {
    let (mut s, _fake) = rig();
    follow(&mut s, ClockKind::Mtc);
    assert!(s.set_tempo(Bpm::of(100.0)).is_ok());
    s.midi()
        .follow_tempo(FollowTempo {
            group: G,
            meter: false,
        })
        .expect("MTC takes no tempo");
    s.midi().unfollow_tempo().unwrap();

    follow(&mut s, ClockKind::BeatClock);
    assert!(matches!(
        s.set_tempo(Bpm::of(100.0)),
        Err(TempoError::Following)
    ));
    let taken = s.midi().follow_tempo(FollowTempo {
        group: G,
        meter: false,
    });
    assert!(matches!(taken, Err(MidiError::TempoSourceTaken)));

    s.midi().unfollow_clock().expect("was following");
    assert!(matches!(
        s.midi().unfollow_clock(),
        Err(MidiError::NotFollowingClock)
    ));
    let events = s.update();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::ClockUnfollowed)))
    );
    assert!(s.set_tempo(Bpm::of(100.0)).is_ok());
}

/// **A command the transport refuses is counted, and reported**: with the
/// transport's list of timed commands full when a followed Start is placed, the
/// update says how many followed commands were refused (a Start is one command,
/// a locate that then plays).
#[test]
fn a_clock_command_the_transport_refuses_is_counted() {
    let (mut s, fake) = rig();
    follow(&mut s, ClockKind::BeatClock);
    send(&fake, system(System::Start), Instant::now());
    let mut refused = 0;
    for _ in 0..50 {
        blocks(&s, 1);
        // The list is full again before the update that places what the tap held.
        while s.schedule(At::NextBlock, MotionEvent::stop()).is_ok() {}
        for event in s.update() {
            if let SessionEvent::Midi(MidiEvent::ClockCommandsRefused(n)) = event {
                refused += n;
            }
        }
        if refused > 0 {
            break;
        }
    }
    assert_eq!(refused, 1, "the Start, placed as one locate-and-play");
}
