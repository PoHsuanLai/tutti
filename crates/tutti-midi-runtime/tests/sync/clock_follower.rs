//! `ClockFollower`: what a transport that follows an outside clock does.
//!
//! The oracle is the spec's arithmetic, written here: a Timing Clock is 1/24 of
//! a quarter note (M1 p.30), so ticks `p` seconds apart are `60 / (24 p)` bpm; a
//! Song Position Pointer of `n` is `6n` clocks (M1 p.27), a sixteenth note, so
//! beat `n / 4`; a Quarter Frame cycle spells a position two frames before its
//! last piece (the MTC part of M1), at 24, 25, 29.97 or 30 frames a second.

use tutti_fixtures::midi;
use tutti_midi_runtime::{ClockEvent, ClockFollower, ClockKind, ClockPosition, MidiClockDecoder};
use tutti_types::{Bpm, Seconds};
use tutti_ump::{Message, MidiGroup, Sysex7Packets, System, U7, U14, Ump};

fn at(seconds: f64) -> Seconds {
    Seconds::new(seconds)
}

const G: MidiGroup = MidiGroup::FIRST;

fn system(message: System) -> Ump {
    Message::System(G, message).into()
}

/// What a follower said to the messages fed to it.
struct Rig {
    follower: ClockFollower,
    heard: Vec<ClockEvent>,
}

impl Rig {
    fn new(kind: ClockKind) -> Self {
        Self {
            follower: ClockFollower::new(kind),
            heard: Vec::new(),
        }
    }

    fn feed(&mut self, ump: Ump, seconds: f64) {
        let heard = &mut self.heard;
        self.follower.step(ump, at(seconds), &mut |e| heard.push(e));
    }

    fn poll(&mut self, seconds: f64) {
        let heard = &mut self.heard;
        self.follower.poll(at(seconds), &mut |e| heard.push(e));
    }

    fn take(&mut self) -> Vec<ClockEvent> {
        std::mem::take(&mut self.heard)
    }

    fn tempos(&self) -> Vec<Bpm> {
        self.heard
            .iter()
            .filter_map(|e| match e {
                ClockEvent::Tempo(t) => Some(*t),
                _ => None,
            })
            .collect()
    }
}

/// `count` ticks from `start` seconds, `period` apart; the time after the last.
fn ticks(rig: &mut Rig, start: f64, period: f64, count: usize) -> f64 {
    let mut t = start;
    for _ in 0..count {
        rig.feed(system(System::TimingClock), t);
        t += period;
    }
    t
}

fn period_of(bpm: f64) -> f64 {
    60.0 / (bpm * 24.0)
}

fn beat_locate(e: &ClockEvent) -> Option<f64> {
    match e {
        ClockEvent::Locate {
            position: ClockPosition::Beat(b),
            ..
        } => Some(b.get()),
        _ => None,
    }
}

/// **A steady clock gives exactly its tempo**: ticks `p` seconds apart are
/// `60 / (24 p)` bpm, at every tempo in the table, announced once on locking.
#[test]
fn a_steady_clock_gives_its_exact_bpm() {
    for bpm in [30.0, 60.0, 90.0, 120.0, 133.5, 180.0, 240.0, 300.0] {
        let mut rig = Rig::new(ClockKind::BeatClock);
        ticks(&mut rig, 0.0, period_of(bpm), 48);
        let tempos = rig.tempos();
        assert_eq!(tempos.len(), 1, "{bpm}: {tempos:?}");
        assert!(
            (tempos[0].get() - bpm).abs() < 1e-6,
            "{bpm}: {:?}",
            tempos[0]
        );
    }
}

/// **A quarter note of steady ticks locks, one fewer does not**: the follower
/// reports `Locked` on the `LOCK_TICKS`-th tick and not before.
#[test]
fn it_locks_on_the_lock_ticks_th_consistent_tick() {
    let mut rig = Rig::new(ClockKind::BeatClock);
    let p = period_of(120.0);
    ticks(&mut rig, 0.0, p, MidiClockDecoder::LOCK_TICKS - 1);
    assert!(!rig.follower.locked());
    assert!(rig.take().is_empty(), "nothing before the lock");
    rig.feed(
        system(System::TimingClock),
        p * (MidiClockDecoder::LOCK_TICKS - 1) as f64,
    );
    assert!(rig.follower.locked());
    assert_eq!(rig.take()[0], ClockEvent::Locked);
}

/// The worst the fit can be off: arrival errors `e_i` with `|e_i| <= E` move the
/// least-squares slope `sum d_i t_i / sum d_i^2` by `E sum |d_i| / sum d_i^2` at
/// most, for centred indices `d_i`; for a window of `n` that is `E (n^2 / 4) /
/// (n (n^2 - 1) / 12)`.
fn worst_case_period_error(window: usize, max_jitter: f64) -> f64 {
    let n = window as f64;
    let sum_abs = n * n / 4.0;
    let sum_sq = n * (n * n - 1.0) / 12.0;
    max_jitter * sum_abs / sum_sq
}

/// A deterministic uniform value in -1..1.
fn noise(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
}

/// **A jittered clock converges within the least-squares bound**: arrivals off
/// by up to 2 ms at 120 bpm (a tenth of a period) give a tempo within the
/// analytic worst case for a 48-tick window, 0.72 bpm, for every seed; and the
/// error over seeds is the fit's standard deviation, `sigma * sqrt(12 / (n (n^2 -
/// 1)))` for a uniform error of `sigma = E / sqrt(3)`, whose bpm is 0.07, so the
/// root mean square stays under 0.1.
#[test]
fn a_jittered_clock_converges_within_the_bound() {
    let (bpm, jitter, window) = (120.0, 0.002, MidiClockDecoder::WINDOW_TICKS);
    let p = period_of(bpm);
    let bound_bpm = bpm * worst_case_period_error(window, jitter) / p;
    assert!((bound_bpm - 0.72).abs() < 0.01, "{bound_bpm}");
    let mut squares = 0.0;
    let seeds = 200u64;
    for seed in 0..seeds {
        let mut state = seed + 1;
        let mut decoder = MidiClockDecoder::new();
        for i in 0..window {
            decoder.tick(at(1.0 + i as f64 * p + jitter * noise(&mut state)));
        }
        let error = decoder.tempo_bpm().expect("a window of ticks").get() - bpm;
        assert!(
            error.abs() <= bound_bpm,
            "seed {seed}: {error} > {bound_bpm}"
        );
        squares += error * error;
    }
    let rms = (squares / seeds as f64).sqrt();
    assert!(rms < 0.1, "rms {rms} bpm");
}

/// **A tempo step is followed**: a source that goes from 120 to 150 bpm is
/// announced again, once a quarter note of ticks has passed since the last and
/// the fit has moved by more than `TEMPO_STEP`, and a steady source is announced
/// once only.
#[test]
fn a_tempo_change_is_announced_once_the_fit_has_moved() {
    let mut rig = Rig::new(ClockKind::BeatClock);
    let t = ticks(&mut rig, 0.0, period_of(120.0), 96);
    assert_eq!(rig.tempos().len(), 1, "steady: announced once");
    ticks(&mut rig, t, period_of(150.0), 96);
    let tempos = rig.tempos();
    assert!(tempos.len() > 1, "{tempos:?}");
    let last = tempos.last().unwrap().get();
    assert!((last - 150.0).abs() < 1e-6, "{last}");
    for pair in tempos.windows(2) {
        assert!(
            Bpm::differs_from(pair[1], pair[0], ClockFollower::TEMPO_STEP),
            "re-announced a move under the step: {pair:?}"
        );
    }
}

/// **A missed tick unlocks**: a gap of two periods is outside the tolerance, so
/// the lock is released at it and taken again after a quarter note more.
#[test]
fn a_missed_tick_unlocks_and_the_clock_locks_again() {
    let mut rig = Rig::new(ClockKind::BeatClock);
    let p = period_of(120.0);
    let t = ticks(&mut rig, 0.0, p, 30);
    assert!(rig.follower.locked());
    rig.take();
    rig.feed(system(System::TimingClock), t + p);
    assert!(!rig.follower.locked());
    assert_eq!(rig.take(), [ClockEvent::Unlocked]);
    ticks(&mut rig, t + 2.0 * p, p, MidiClockDecoder::LOCK_TICKS);
    assert!(rig.follower.locked());
    assert!(rig.heard.contains(&ClockEvent::Locked));
}

/// **A dropout unlocks**: silence for `DROPOUT` releases the lock and, for a
/// transport the source had started, stops it; one tick under the timeout does
/// neither, and the unlock is said once.
#[test]
fn a_dropout_unlocks_and_stops_a_started_transport() {
    let mut rig = Rig::new(ClockKind::BeatClock);
    let p = period_of(120.0);
    rig.feed(system(System::Start), 0.0);
    let t = ticks(&mut rig, 0.0, p, 48);
    let last = t - p;
    assert!(rig.follower.locked());
    rig.take();
    rig.poll(last + MidiClockDecoder::DROPOUT.get() - 0.001);
    assert!(rig.follower.locked());
    assert!(rig.take().is_empty());
    rig.poll(last + MidiClockDecoder::DROPOUT.get() + 0.001);
    assert!(!rig.follower.locked());
    assert_eq!(rig.take(), [ClockEvent::Unlocked, ClockEvent::Stop]);
    rig.poll(last + 10.0);
    assert!(rig.take().is_empty(), "said once");
}

/// **Song Position Pointer locates to its sixteenths** (M1 p.27): `n` sixteenth
/// notes is `n / 4` beats, over the 14-bit range.
#[test]
fn song_position_pointer_sixteenths_map_to_beats() {
    for (sixteenths, beats) in [
        (0u16, 0.0),
        (1, 0.25),
        (4, 1.0),
        (10, 2.5),
        (0x0123, 72.75),
        (0x3FFF, 4095.75),
    ] {
        let mut rig = Rig::new(ClockKind::BeatClock);
        rig.feed(system(System::SongPosition(U14::of(sixteenths))), 0.0);
        let located: Vec<f64> = rig.heard.iter().filter_map(beat_locate).collect();
        assert_eq!(located, [beats], "pointer {sixteenths}");
        assert_eq!(rig.heard.len(), 1, "a locate and no play");
    }
}

/// **Start plays from 0, Continue plays from where the source is, Stop stops**
/// (M1 pp.27 to 31): a Continue after a pointer and some clocks resumes at the
/// pointer plus the clocks counted, and a Start after it begins at 0 again.
#[test]
fn start_continue_and_stop_drive_play_state() {
    let mut rig = Rig::new(ClockKind::BeatClock);
    rig.feed(system(System::SongPosition(U14::of(16))), 0.0);
    rig.take();
    rig.feed(system(System::Continue), 0.1);
    let located: Vec<f64> = rig.heard.iter().filter_map(beat_locate).collect();
    assert_eq!(located, [4.0]);
    assert_eq!(rig.heard.last(), Some(&ClockEvent::Play));
    let t = ticks(&mut rig, 0.2, period_of(120.0), 24);
    rig.take();
    rig.feed(system(System::Stop), t);
    assert_eq!(rig.take(), [ClockEvent::Stop]);
    rig.feed(system(System::Continue), t + 1.0);
    let located: Vec<f64> = rig.heard.iter().filter_map(beat_locate).collect();
    assert_eq!(located, [5.0], "a beat of 24 ticks past the pointer's 4");
    rig.take();
    rig.feed(system(System::Start), t + 2.0);
    let located: Vec<f64> = rig.heard.iter().filter_map(beat_locate).collect();
    assert_eq!(located, [0.0]);
    assert_eq!(rig.heard.last(), Some(&ClockEvent::Play));
}

/// A message the kind does not read says nothing: a note, a Quarter Frame to a
/// Beat Clock follower, a Timing Clock to an MTC one.
#[test]
fn messages_of_the_other_kind_are_ignored() {
    let mut clock = Rig::new(ClockKind::BeatClock);
    clock.feed(
        midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000),
        0.0,
    );
    clock.feed(quarter(0, 0), 0.0);
    assert!(clock.heard.is_empty());
    let mut mtc = Rig::new(ClockKind::Mtc);
    ticks(&mut mtc, 0.0, period_of(120.0), 48);
    mtc.feed(system(System::Start), 2.0);
    assert!(mtc.heard.is_empty());
}

// --- MTC ---

fn quarter(piece: u8, nibble: u8) -> Ump {
    Ump::from(tutti_ump::Message::System(
        G,
        System::TimeCode(U7::of(piece << 4 | nibble)),
    ))
}

/// The frame rates of the MTC rate code (two bits) and their frames a second.
const RATES: [(u8, f64); 4] = [(0, 24.0), (1, 25.0), (2, 30_000.0 / 1001.0), (3, 30.0)];

/// The eight quarter frames of `h:m:s:f` at rate code `rate`, and the time of
/// each: piece `k` is `k / 4` frames after piece 0, which is at `start`.
fn cycle(h: u8, m: u8, s: u8, f: u8, rate: u8, fps: f64, start: f64) -> Vec<(Ump, f64)> {
    let nibbles = [
        f & 0xF,
        f >> 4,
        s & 0xF,
        s >> 4,
        m & 0xF,
        m >> 4,
        h & 0xF,
        (h >> 4) | rate << 1,
    ];
    nibbles
        .iter()
        .enumerate()
        .map(|(k, n)| (quarter(k as u8, *n), start + k as f64 / (4.0 * fps)))
        .collect()
}

/// Feeds `cycles` consecutive cycles from `h:m:s:f` (non-drop, within the
/// second); returns the time after the last piece's slot.
fn run_mtc(rig: &mut Rig, rate: (u8, f64), s: u8, f: u8, start: f64, cycles: u8) -> f64 {
    let (code, fps) = rate;
    let mut t = start;
    for c in 0..cycles {
        for (ump, at) in cycle(0, 0, s, f + 2 * c, code, fps, t) {
            rig.feed(ump, at);
        }
        t += 2.0 / fps;
    }
    t
}

fn time_locates(rig: &Rig) -> Vec<f64> {
    rig.heard
        .iter()
        .filter_map(|e| match e {
            ClockEvent::Locate {
                position: ClockPosition::Time(s),
                ..
            } => Some(s.get()),
            _ => None,
        })
        .collect()
}

/// **Quarter Frames give the position at each frame rate**: a cycle spelling
/// `00:00:01:04` plays at frame `fps + 4 + 2` (the position is two frames old
/// when its last piece arrives), at the rate's own frame length, 1001/30000 s for
/// 29.97.
#[test]
fn quarter_frames_give_the_position_at_every_frame_rate() {
    for rate in RATES {
        let mut rig = Rig::new(ClockKind::Mtc);
        // 1 s, frames 4, then two cycles on: the third locks.
        run_mtc(&mut rig, rate, 1, 4, 0.0, 3);
        let located = time_locates(&rig);
        assert_eq!(located.len(), 1, "{rate:?}: {located:?}");
        // The third cycle spells frame 4 + 4 = 8, and its position is two on.
        let frames = rate.1.round() + 8.0 + 2.0;
        let expected = match rate.0 {
            2 => (30.0 + 8.0 + 2.0) * 1001.0 / 30_000.0,
            _ => frames / rate.1,
        };
        // Drop-frame never drops at second 1, so labels count at face value.
        assert!(
            (located[0] - expected).abs() < 1e-9,
            "{rate:?}: {} vs {expected}",
            located[0]
        );
        assert_eq!(
            rig.heard.last(),
            Some(&ClockEvent::Play),
            "{rate:?}: locates, then plays"
        );
        assert!(rig.heard.contains(&ClockEvent::Locked));
    }
}

/// **Three consistent cycles lock, two do not**: the first two make no event.
#[test]
fn mtc_locks_after_three_consistent_cycles() {
    let mut rig = Rig::new(ClockKind::Mtc);
    run_mtc(
        &mut rig,
        RATES[1],
        0,
        0,
        0.0,
        ClockFollower::MTC_LOCK_CYCLES as u8 - 1,
    );
    assert!(rig.heard.is_empty());
    assert!(!rig.follower.locked());
    run_mtc(&mut rig, RATES[1], 0, 4, 0.08 * 2.0, 1);
    assert!(rig.follower.locked());
}

/// **A jump in the running position locates**: a cycle that is not two frames
/// after the last, while playing, is a locate to its position; a steady run
/// makes none.
#[test]
fn a_jump_while_playing_locates_and_a_steady_run_does_not() {
    let rate = RATES[1];
    let mut rig = Rig::new(ClockKind::Mtc);
    let t = run_mtc(&mut rig, rate, 0, 0, 0.0, 6);
    assert_eq!(time_locates(&rig).len(), 1, "the lock's own");
    rig.take();
    // Ten seconds on.
    run_mtc(&mut rig, rate, 10, 0, t, 1);
    let located = time_locates(&rig);
    assert_eq!(located.len(), 1);
    assert!(
        (located[0] - (10.0 + 2.0 / 25.0)).abs() < 1e-9,
        "{located:?}"
    );
    assert!(rig.follower.locked(), "a jump is not a dropout");
}

fn full_frame(h: u8, m: u8, s: u8, f: u8, rate: u8) -> Vec<Ump> {
    Sysex7Packets::from_payload(G, &[0x7F, 0x7F, 0x01, 0x01, rate << 5 | h, m, s, f])
        .expect("a 7-bit payload")
        .collect()
}

/// **A Full Message locates** to its position exactly (no two-frame offset,
/// which is the Quarter Frames'), whether stopped or rolling, and makes no play.
#[test]
fn a_full_message_locates() {
    for (code, fps) in RATES {
        let mut rig = Rig::new(ClockKind::Mtc);
        for ump in full_frame(1, 2, 3, 4, code) {
            rig.feed(ump, 0.0);
        }
        let located = time_locates(&rig);
        let (num, den) = match code {
            0 => (24.0, 1.0),
            1 => (25.0, 1.0),
            2 => (30_000.0, 1001.0),
            _ => (30.0, 1.0),
        };
        let _ = fps;
        // 1:02:03 is 3723 s, and frame 4 of a second the rate counts: drop-frame
        // omits labels only at the start of minutes not divisible by ten, so
        // 02:03:04 is 123 s and 4 frames in, less the two labels dropped in
        // minute 1 and counted from there: frames elapsed = 3723 * 30 + 4 - 2 *
        // (62 - 6).
        let frames = match code {
            2 => 3723.0 * 30.0 + 4.0 - 2.0 * 56.0,
            0 => 3723.0 * 24.0 + 4.0,
            1 => 3723.0 * 25.0 + 4.0,
            _ => 3723.0 * 30.0 + 4.0,
        };
        let expected = frames * den / num;
        assert_eq!(located.len(), 1, "{code}");
        assert!(
            (located[0] - expected).abs() < 1e-9,
            "{code}: {} vs {expected}",
            located[0]
        );
        assert_eq!(rig.heard.len(), 1, "locate only");
    }
}

/// **A Full Message while playing keeps playing and the next cycle is not a
/// jump**: it locates once, and the cycle that follows adds no second locate.
#[test]
fn a_full_message_while_playing_locates_once() {
    let rate = RATES[1];
    let mut rig = Rig::new(ClockKind::Mtc);
    let t = run_mtc(&mut rig, rate, 0, 0, 0.0, 4);
    rig.take();
    for ump in full_frame(0, 0, 30, 0, 1) {
        rig.feed(ump, t);
    }
    assert_eq!(time_locates(&rig).len(), 1);
    run_mtc(&mut rig, rate, 30, 0, t + 0.01, 3);
    assert_eq!(time_locates(&rig).len(), 1, "no jump after a full message");
    assert!(!rig.heard.contains(&ClockEvent::Stop));
}

/// **MTC silence unlocks and stops**: no Quarter Frame for `MTC_DROPOUT` says
/// `Stop` then `Unlocked`, once; just under it says nothing.
#[test]
fn an_mtc_dropout_unlocks_and_stops() {
    let rate = RATES[3];
    let mut rig = Rig::new(ClockKind::Mtc);
    let t = run_mtc(&mut rig, rate, 0, 0, 0.0, 5);
    let last = t - 2.0 / 30.0 + 7.0 / 120.0;
    assert!(rig.follower.locked());
    rig.take();
    let d = ClockFollower::MTC_DROPOUT.get();
    rig.poll(last + d - 0.001);
    assert!(rig.take().is_empty());
    rig.poll(last + d + 0.001);
    assert_eq!(rig.take(), [ClockEvent::Stop, ClockEvent::Unlocked]);
    assert!(!rig.follower.locked());
    rig.poll(last + 5.0);
    assert!(rig.take().is_empty());
    // The source comes back: three cycles lock it and play again.
    run_mtc(&mut rig, rate, 5, 0, last + 6.0, 3);
    assert_eq!(rig.heard.last(), Some(&ClockEvent::Play));
}

/// Stepping allocates nothing, for either kind.
#[test]
fn stepping_a_follower_allocates_nothing() {
    let mut clock = ClockFollower::new(ClockKind::BeatClock);
    let mut mtc = ClockFollower::new(ClockKind::Mtc);
    let mut n = 0usize;
    let p = period_of(120.0);
    let tick = system(System::TimingClock);
    let pieces: Vec<(Ump, f64)> = cycle(0, 0, 1, 0, 1, 25.0, 0.0)
        .into_iter()
        .chain(cycle(0, 0, 1, 2, 1, 25.0, 0.08))
        .chain(cycle(0, 0, 1, 4, 1, 25.0, 0.16))
        .collect();
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            clock.step(tick, at(i as f64 * p), &mut |_| n += 1);
        }
        for &(ump, t) in &pieces {
            mtc.step(ump, at(t), &mut |_| n += 1);
        }
        mtc.poll(at(5.0), &mut |_| n += 1);
    });
    assert!(n > 0);
}
