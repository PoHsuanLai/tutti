use super::*;
use crate::{ClockTransportState, MidiClockDecoder, MtcDecoder};
use std::sync::Arc;
use std::sync::atomic::AtomicBool as StdAtomicBool;
use tutti_graph::UmpAt;
use tutti_types::Bpm;
use tutti_ump::{Message, System, decode};

/// The System message a packet carries.
fn system(ump: Ump) -> Option<System> {
    match decode(ump) {
        Ok(Message::System(_, system)) => Some(system),
        _ => None,
    }
}

/// Test transport: tempo + beat + playing under a switch.
struct TestTransport {
    beat: AtomicF64,
    tempo: Bpm,
    playing: StdAtomicBool,
    looping: std::sync::Mutex<Option<tutti_graph::LoopRange>>,
}

impl TestTransport {
    fn new(tempo: Bpm) -> Self {
        Self {
            beat: AtomicF64::new(0.0),
            tempo,
            playing: StdAtomicBool::new(false),
            looping: std::sync::Mutex::new(None),
        }
    }
    fn set_loop(&self, start: f64, end: f64) {
        *self.looping.lock().unwrap() = Some(tutti_graph::LoopRange {
            start: Beat(start),
            end: Beat(end),
        });
    }
    fn set_beat(&self, b: f64) {
        self.beat.store(b, Ordering::Release);
    }
    fn set_playing(&self, p: bool) {
        self.playing.store(p, Ordering::Release);
    }
    fn now(&self) -> Transport {
        Transport::new(
            self.playing.load(Ordering::Acquire),
            self.tempo,
            Beat(self.beat.load(Ordering::Acquire)),
            *self.looping.lock().unwrap(),
        )
    }
}

/// A master ticked from a [`TestTransport`], emitting into a mailbox.
struct Rig {
    cm: ClockMaster,
    transport: Arc<TestTransport>,
    out: crate::MidiSender,
}

impl std::ops::Deref for Rig {
    type Target = ClockMaster;
    fn deref(&self) -> &ClockMaster {
        &self.cm
    }
}

impl Rig {
    fn tick(&self, block_size: usize) {
        self.cm.tick(&self.transport.now(), block_size, &mut |e| {
            let _ = self.out.queue(&[e]);
        });
    }
}

fn master(
    tempo: Bpm,
    sample_rate: impl Into<SampleRate>,
) -> (Rig, Arc<TestTransport>, crate::MidiReceiver) {
    let transport = Arc::new(TestTransport::new(tempo));
    let (out, receiver) = crate::MidiMailbox::with_capacity(4096);
    let cm = ClockMaster::new(sample_rate);
    cm.set_enabled(true);
    let rig = Rig {
        cm,
        transport: Arc::clone(&transport),
        out,
    };
    (rig, transport, receiver)
}

/// Drains a receiver fully into a `Vec`.
fn drain_all(receiver: &crate::MidiReceiver) -> Vec<UmpAt> {
    let mut out = Vec::new();
    let mut buf = [(0, Ump([0; 4])); 256];
    loop {
        let n = receiver.poll_into(&mut buf);
        // The ring's stamps here are block offsets (the rig queues them so).
        out.extend(
            buf[..n]
                .iter()
                .map(|&(stamp, ump)| (Offset::new(stamp as usize, Samples(1 << 20)).unwrap(), ump)),
        );
        if n < buf.len() {
            break;
        }
    }
    out
}

fn is_status(ev: &UmpAt, status: u8) -> bool {
    let w0 = ev.1.0[0];
    (w0 >> 28) & 0x0F == 0x1 && ((w0 >> 16) & 0xFF) as u8 == status
}

#[test]
fn emits_24_ticks_per_beat_at_120bpm() {
    let sr = 48_000.0;
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_playing(true);
    // 120 BPM → 0.5s/beat → 24000 samples/beat. Process one beat's worth of
    // audio in blocks of 512 and count 0xF8 ticks.
    let samples_per_beat = sr * 0.5;
    let block = 512usize;
    let blocks = (samples_per_beat / block as f64).ceil() as usize;

    let mut clock_ticks = 0;
    let mut beat = 0.0f64;
    let beats_per_sample = 120.0 / 60.0 / sr;
    for _ in 0..blocks {
        transport.set_beat(beat);
        cm.tick(block);
        beat += block as f64 * beats_per_sample;
    }
    let events = drain_all(&cons);
    for ev in &events {
        if is_status(ev, 0xF8) {
            clock_ticks += 1;
        }
    }
    // Over ~1 beat, 24 ticks are expected (allow ±1 for boundary rounding).
    assert!(
        (clock_ticks as i64 - 24).abs() <= 1,
        "expected ~24 clock ticks over one beat, got {clock_ticks}"
    );
}

#[test]
fn play_at_zero_emits_start_not_continue() {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    assert!(
        events.iter().any(|e| is_status(e, 0xFA)),
        "expected Start (0xFA)"
    );
    assert!(
        !events.iter().any(|e| is_status(e, 0xFB)),
        "no Continue at beat 0"
    );
}

#[test]
fn play_mid_song_emits_continue_and_song_position() {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    transport.set_beat(8.0); // 2 bars in
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    assert!(
        events.iter().any(|e| is_status(e, 0xFB)),
        "expected Continue (0xFB)"
    );
    assert!(
        events.iter().any(|e| is_status(e, 0xF2)),
        "expected Song Position (0xF2)"
    );
    assert!(
        !events.iter().any(|e| is_status(e, 0xFA)),
        "no Start mid-song"
    );
}

#[test]
fn stop_emits_stop_message() {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    transport.set_playing(true);
    cm.tick(512);
    let _ = drain_all(&cons);
    transport.set_playing(false);
    cm.tick(512);
    let events = drain_all(&cons);
    assert!(
        events.iter().any(|e| is_status(e, 0xFC)),
        "expected Stop (0xFC)"
    );
}

#[test]
fn disabled_master_emits_nothing() {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    cm.set_enabled(false);
    transport.set_playing(true);
    cm.tick(512);
    assert!(
        drain_all(&cons).is_empty(),
        "disabled master must be silent"
    );
}

/// Ticks land on their frames, in order, and a start on a tick boundary
/// sends that boundary's tick on frame 0.
///
/// This pinned two ticks (1000, 2000) until the start tick was fixed:
/// MIDI 1.0 (MMA, "System Real Time Messages", Start) has a receiver
/// begin on the first Timing Clock after Start, so the clock of beat 0
/// must be sent, or every receiver runs one tick behind.
#[test]
fn ticks_are_sample_accurate_within_block() {
    // A block that spans three clock ticks, the first on its first
    // frame, places them at distinct, ordered frame offsets.
    let sr = 48_000.0;
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_playing(true);
    transport.set_beat(0.0);
    // One beat = 24000 samples; 1/24 beat = 1000 samples. A 2500-sample
    // block starting at beat 0 covers tick boundaries at 0, 1000 and 2000.
    cm.tick(2500);
    let events: Vec<_> = drain_all(&cons)
        .into_iter()
        .filter(|e| is_status(e, 0xF8))
        .collect();
    assert_eq!(events.len(), 3, "expected 3 ticks in the block");
    assert!(
        events[0].0.get() < events[1].0.get() && events[1].0.get() < events[2].0.get(),
        "ticks ordered"
    );
    assert_eq!(events[0].0.get(), 0, "the start beat's tick");
    assert!((events[1].0.get() as i64 - 1000).abs() < 4);
    assert!((events[2].0.get() as i64 - 2000).abs() < 4);
}

/// Where the first Timing Clock goes, and in what order, for a block that
/// does not continue the previous one: the transport message(s), then an
/// F8 on frame 0 when the beat sits on a tick boundary, else the first F8
/// on the next boundary.
fn first_clock(events: &[UmpAt]) -> (usize, u32) {
    let i = events
        .iter()
        .position(|e| is_status(e, 0xF8))
        .expect("a timing clock");
    (i, events[i].0.get())
}

/// Start at beat 0, Continue at an on-tick beat, and a locate while
/// playing to an on-tick beat: each sends an F8 on frame 0, queued after
/// its Start / Continue / Song Position. MIDI 1.0 (MMA, "System Real
/// Time Messages", Start, Continue): a receiver begins on the first
/// Timing Clock after Start / Continue.
///
/// Mutation (run): skip a tick on frame 0 of a block that does not
/// continue the last (`k == 0 && (is_edge || jumped)` → `continue`, the
/// old rule) → no F8 at offset 0 → fails.
#[test]
fn an_on_tick_start_continue_or_locate_clocks_its_first_frame() {
    let sr = 48_000.0;
    // Start at beat 0.
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    let start = events
        .iter()
        .position(|e| is_status(e, 0xFA))
        .expect("Start");
    let (clock, offset) = first_clock(&events);
    assert_eq!(offset, 0, "Start at beat 0: F8 on frame 0");
    assert!(start < clock, "the F8 follows Start in the queue");

    // Continue at beat 8 (tick 192).
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_beat(8.0);
    transport.set_playing(true);
    cm.tick(512);
    let events = drain_all(&cons);
    let spp = events.iter().position(|e| is_status(e, 0xF2)).expect("SPP");
    let cont = events
        .iter()
        .position(|e| is_status(e, 0xFB))
        .expect("Continue");
    let (clock, offset) = first_clock(&events);
    assert_eq!(offset, 0, "Continue on a tick: F8 on frame 0");
    assert!(
        spp < clock && cont < clock,
        "the F8 follows SPP and Continue"
    );

    // A locate while playing to beat 4 (tick 96).
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let _ = drain_all(&cons);
    transport.set_beat(4.0);
    cm.tick(512);
    let events = drain_all(&cons);
    let spp = events.iter().position(|e| is_status(e, 0xF2)).expect("SPP");
    let (clock, offset) = first_clock(&events);
    assert_eq!(offset, 0, "a locate on a tick: F8 on frame 0");
    assert!(spp < clock, "the F8 follows Song Position");
}

/// Off a tick boundary, the first F8 after a locate or a Continue is the
/// next boundary's, on its frame; nothing goes out on frame 0.
///
/// Mutation (run): send a tick behind the block's start on frame 0
/// (`k < 0` → `k = 0` instead of skipping it) → an F8 at offset 0 →
/// fails.
#[test]
fn an_off_tick_locate_clocks_the_next_boundary() {
    let sr = 48_000.0;
    // Half a tick past beat 4: 500 frames to the next boundary at 120
    // BPM / 48 kHz (a tick is 1000 frames).
    let off = 4.0 + 0.5 / PPQN;
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_beat(0.0);
    transport.set_playing(true);
    cm.tick(512);
    let _ = drain_all(&cons);
    transport.set_beat(off);
    cm.tick(2048);
    let events = drain_all(&cons);
    assert!(events.iter().any(|e| is_status(e, 0xF2)), "a locate");
    let (_, offset) = first_clock(&events);
    assert_eq!(offset, 500, "the next boundary's frame");

    // The same off-tick beat as a Continue.
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_beat(off);
    transport.set_playing(true);
    cm.tick(2048);
    let (_, offset) = first_clock(&drain_all(&cons));
    assert_eq!(offset, 500, "Continue off a tick: the next boundary");
}

/// Run `blocks` blocks of `block` frames at `rate`, the transport rolling
/// at 120 BPM from `*beat`, and return every event with its absolute
/// frame (counted from `*frame`). Advances both.
fn run_at(
    cm: &Rig,
    transport: &TestTransport,
    cons: &crate::MidiReceiver,
    rate: f64,
    (block, blocks): (usize, usize),
    (beat, frame): (&mut f64, &mut u64),
) -> Vec<(UmpAt, u64)> {
    let mut at = Vec::new();
    for _ in 0..blocks {
        transport.set_beat(*beat);
        cm.tick(block);
        at.extend(
            drain_all(cons)
                .into_iter()
                .map(|e| (e, *frame + u64::from(e.0.get()))),
        );
        *beat += block as f64 * 2.0 / rate;
        *frame += block as u64;
    }
    at
}

/// The frames of the events in `events` with `status`.
fn of(events: &[(UmpAt, u64)], status: u8) -> Vec<u64> {
    events
        .iter()
        .filter(|(e, _)| is_status(e, status))
        .map(|&(_, at)| at)
        .collect()
}

/// Mean spacing, in frames, of the events at `at`.
fn spacing(at: &[u64]) -> f64 {
    assert!(at.len() > 10, "events to measure, got {}", at.len());
    (at[at.len() - 1] - at[0]) as f64 / (at.len() - 1) as f64
}

/// **A rate change moves the tick spacing to the new rate's frames.** At
/// 120 BPM a 24-PPQN tick is 1/48 s: 918.75 frames at 44.1 kHz, 1 000 at
/// 48 kHz. A master built at 44.1 kHz and re-rated to 48 kHz (a device
/// restart) must space its ticks 1 000 frames apart from the next block,
/// or the receiving gear runs ~8.8% fast. (Offsets are whole frames, so
/// each tick sits up to one frame early; the mean over ~48 ticks is within
/// 0.05 of the exact spacing.)
///
/// Mutation (run): `set_sample_rate` not storing the rate → the ticks
/// after it stay 918.75 frames apart → fails.
#[test]
fn a_rate_change_moves_the_tick_spacing() {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(44_100.0));
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let run = (&mut beat, &mut frame);
    let before = of(
        &run_at(&cm, &transport, &cons, 44_100.0, (441, 100), run),
        0xF8,
    );
    assert!(
        (spacing(&before) - 918.75).abs() < 0.05,
        "{}",
        spacing(&before)
    );

    cm.set_sample_rate(SampleRate::new(48_000.0));
    assert_eq!(cm.sample_rate(), SampleRate(48_000.0));
    let run = (&mut beat, &mut frame);
    let after = of(
        &run_at(&cm, &transport, &cons, 48_000.0, (480, 100), run),
        0xF8,
    );
    assert!(
        (spacing(&after) - 1_000.0).abs() < 0.05,
        "{}",
        spacing(&after)
    );
}

/// **The MTC quarter-frame due across a rate change lands on its
/// wall-clock time.** At 25 fps a quarter-frame is 1/100 s: 441 frames at
/// 44.1 kHz, 480 at 48 kHz. After 20 blocks of 512 at 44.1 kHz (10 240
/// frames) the last quarter-frame was at 10 143 and the next is due 441
/// frames later, 344 old frames after the restart: 374.4 new frames, and
/// 480 apart from there. And the restart is not a locate: no Song
/// Position goes out, and the phase is not reset.
///
/// Mutations (run):
/// - `set_sample_rate` not rescaling the carried phase → the first one
///   lands 344 frames after the restart, ~30 early → fails;
/// - the seek check comparing the beat's move with *this* block's
///   advance → the old-rate block's 0.0232 beats against the
///   new rate's 0.0213 is past the 0.001 epsilon → a Song Position, and
///   the phase reset to the restart's frame → fails.
#[test]
fn a_rate_change_keeps_the_mtc_quarter_frame_on_time() {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(44_100.0));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(SmpteFrameRate::Fps25);
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let run = (&mut beat, &mut frame);
    let before = of(
        &run_at(&cm, &transport, &cons, 44_100.0, (512, 20), run),
        0xF1,
    );
    assert_eq!(
        before.last(),
        Some(&10_143),
        "setup: every 441 frames from 0"
    );

    cm.set_sample_rate(SampleRate::new(48_000.0));
    let run = (&mut beat, &mut frame);
    let events = run_at(&cm, &transport, &cons, 48_000.0, (512, 20), run);
    assert_eq!(
        of(&events, 0xF2),
        Vec::<u64>::new(),
        "the restart is not a seek"
    );
    let after = of(&events, 0xF1);
    assert_eq!(after[0], 10_240 + 374, "344 old frames are 374.4 new ones");
    assert!(
        (spacing(&after) - 480.0).abs() < 0.05,
        "{}",
        spacing(&after)
    );
}

/// **The MTC grid is closed form.** Quarter-frame `k` goes out on frame
/// `floor(k × rate / (4 × fps))`, however many blocks it took to get
/// there: at 29.97 fps and 44.1 kHz a quarter-frame is 367.867… frames,
/// which binary cannot represent, and a carried phase that adds it
/// block after block drifts off the grid.
///
/// Mutation (run): carry the phase (quarter-frame `k + 1` due at `k`'s
/// position plus one quarter-frame's length, accumulated) → some
/// quarter-frame whose exact position is a hair past a whole frame lands
/// a frame early → fails.
#[test]
fn the_mtc_grid_is_closed_form() {
    let rate = 44_100.0;
    let fps = SmpteFrameRate::Fps2997Ndf;
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(rate));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(fps);
    transport.set_playing(true);
    let (mut beat, mut frame) = (0.0, 0);
    let events = run_at(
        &cm,
        &transport,
        &cons,
        rate,
        (512, 6_000),
        (&mut beat, &mut frame),
    );
    let quarters = of(&events, 0xF1);
    assert!(quarters.len() > 8_000, "{} quarter-frames", quarters.len());
    for (k, &at) in quarters.iter().enumerate() {
        let due = ((k as f64 * rate) / (fps.fps() * 4.0)).floor() as u64;
        assert_eq!(at, due, "quarter-frame {k}");
    }
}

#[test]
fn generated_clock_round_trips_through_decoder() {
    // The strongest check: feed this master's own output into the decoder and confirm
    // it recovers the tempo and beat — the in-repo loopback from the plan.
    let sr = 48_000.0;
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    transport.set_playing(true);

    let block = 256usize;
    let beats_per_sample = 120.0 / 60.0 / sr;
    let us_per_sample = 1_000_000.0 / sr;
    let mut beat = 0.0f64;
    let mut sample_clock = 0u64;

    let mut decoder = MidiClockDecoder::new();
    // Run ~4 beats so the decoder's tempo window fills.
    for _ in 0..((sr * 2.0 / block as f64) as usize) {
        transport.set_beat(beat);
        cm.tick(block);
        for ev in drain_all(&cons) {
            let ts_us = ((sample_clock + ev.0.get() as u64) as f64 * us_per_sample) as u64;
            decoder.feed(&ev.1, ts_us);
        }
        beat += block as f64 * beats_per_sample;
        sample_clock += block as u64;
    }

    assert_eq!(decoder.transport_state(), ClockTransportState::Playing);
    let tempo = decoder.tempo_bpm().expect("decoder derived a tempo");
    assert!(
        !tempo.differs_from(Bpm::of(120.0), 2.0),
        "round-trip tempo ~120, got {tempo:?}"
    );
}

#[test]
fn mtc_quarter_frames_round_trip_to_a_timecode() {
    let sr = 48_000.0;
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(sr));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(SmpteFrameRate::Fps25);
    transport.set_playing(true);

    // 25 fps → 100 quarter-frames/sec → one every 480 samples. Run enough
    // blocks to emit at least 8 consecutive quarter-frames (2 full frames).
    let block = 480usize;
    let beats_per_sample = 120.0 / 60.0 / sr;
    let mut beat = 0.0f64;
    let mut decoder = MtcDecoder::new();
    let mut got_tc = false;
    for _ in 0..40 {
        transport.set_beat(beat);
        cm.tick(block);
        for ev in drain_all(&cons) {
            if is_status(&ev, 0xF1) {
                // The quarter-frame data byte is the 2nd wire byte; decode
                // via the message view's TimeCode.
                if let Some(System::TimeCode(code)) = system(ev.1) {
                    decoder.feed(code.get());
                    if decoder.timecode().is_some() {
                        got_tc = true;
                    }
                }
            }
        }
        beat += block as f64 * beats_per_sample;
    }
    assert!(got_tc, "MTC quarter-frames should assemble into a timecode");
}

/// Plays 5 s of MTC from 58 s plus `offset` samples (so 1:00 is crossed) into a decoder, and
/// checks every assembled timecode against the clock the master read.
fn mtc_loopback(rate: f64, fps: SmpteFrameRate, offset: f64) {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(rate));
    cm.set_send_mtc(true);
    cm.set_mtc_fps(fps);
    transport.set_playing(true);
    let start = 58.0 * rate + offset;
    let (mut beat, mut frame) = (start * 2.0 / rate, 0);
    let events = run_at(
        &cm,
        &transport,
        &cons,
        rate,
        (480, (rate * 5.0 / 480.0) as usize),
        (&mut beat, &mut frame),
    );
    let mut decoder = MtcDecoder::new();
    let (mut piece0_at, mut last_count, mut cycles) = (0.0, None, 0u32);
    for (ev, at) in events.iter().filter(|(e, _)| is_status(e, 0xF1)) {
        let Some(System::TimeCode(code)) = system(ev.1) else {
            panic!("not a quarter-frame");
        };
        let code = code.get();
        if code >> 4 == 0 {
            piece0_at = (start + *at as f64) / rate;
        }
        decoder.feed(code);
        if code >> 4 != 7 {
            continue;
        }
        let tc = decoder.timecode().expect("a full cycle");
        assert!(
            u64::from(tc.frames) < tc.frame_rate.labels_per_second(),
            "{rate} Hz {fps:?}: {tc}"
        );
        // Piece 0 sits on an even frame of the timeline, and the label is
        // that frame: they differ by under the sample the event is placed
        // on.
        let lag = piece0_at - tc.to_seconds().get();
        assert!(
            lag.abs() < 2.0 / rate,
            "{rate} Hz {fps:?}: {tc} is {lag} s from piece 0"
        );
        let count = tc.frame_count();
        if let Some(prev) = last_count {
            assert_eq!(count, prev + 2, "{rate} Hz {fps:?}: {tc}");
        }
        last_count = Some(count);
        cycles += 1;
    }
    assert!(
        f64::from(cycles) > 5.0 * fps.fps() / 2.0 - 3.0,
        "{cycles} cycles"
    );
    assert_eq!(
        decoder.timecode().map(|t| t.minutes),
        Some(1),
        "crossed 1:00"
    );
}

/// **One cycle carries one position.** Each assembled timecode is valid
/// (frames below the rate), monotonic, and equal to the even frame piece 0
/// went out on, across the minute rollover, at every rate and start offset.
///
/// Mutations (run): compute the timecode at every piece (`piece == 0`
/// made `true`, without the even-frame floor) → pieces 4-7 carry the next
/// frame, and a rollover between the halves assembles a torn value →
/// fails. Skip the alignment in `tick_mtc` (no `mtc.lead` store) → a start
/// off an even frame sends piece 0 up to two frames from its label →
/// fails.
#[test]
fn outbound_mtc_assembles_whole_valid_timecodes_across_a_minute() {
    for (rate, fps) in [
        (48_000.0, SmpteFrameRate::Fps24),
        (48_000.0, SmpteFrameRate::Fps25),
        (48_000.0, SmpteFrameRate::Fps2997Df),
        (48_000.0, SmpteFrameRate::Fps30),
        (44_100.0, SmpteFrameRate::Fps2997Df),
        (44_100.0, SmpteFrameRate::Fps24),
    ] {
        // Offsets that put 1:00 on, and in the middle of, a cycle.
        for offset in [0.0, 1_000.0, 5_000.0] {
            mtc_loopback(rate, fps, offset);
        }
    }
}

// A position on a frame boundary, or a float's width short of it, reads as
// that frame, floored to an even one.
// Mutations: `frames & !1` → `frames` fails the 1920 case; `.round()` →
// `.floor()` fails every "- 1e-7" case.
#[test]
fn timecode_rounds_to_the_nearest_even_frame() {
    let at = |pos: f64, fps| timecode_at(pos, 48_000.0, fps).to_string();
    let f25 = SmpteFrameRate::Fps25;
    assert_eq!(at(3_840.0, f25), "00:00:00:02");
    assert_eq!(at(3_840.0 - 1e-7, f25), "00:00:00:02");
    assert_eq!(at(1_920.0, f25), "00:00:00:00");
    assert_eq!(at(7_680.0 - 1e-7, f25), "00:00:00:04");
    // Drop-frame count 1800 starts at sample 2_882_880 (60.06 s).
    let df = SmpteFrameRate::Fps2997Df;
    assert_eq!(at(2_882_880.0, df), "00:01:00:02");
    assert_eq!(at(2_882_880.0 - 1e-7, df), "00:01:00:02");
    assert_eq!(at(2_882_880.0 - 1_601.6, df), "00:00:59:28");
    let t30 = SmpteFrameRate::Fps30;
    assert_eq!(at(2_880_000.0, t30), "00:01:00:00");
    assert_eq!(at(2_880_000.0 - 1e-7, t30), "00:01:00:00");
}

// MTC has no non-drop 29.97 code: Ndf is sent with the drop-frame one.
// Mutation: give Ndf its own code (3) and the equality fails.
#[test]
fn non_drop_2997_goes_out_with_the_29_97_rate_code() {
    let tc = SmpteTimecode::from_frame_count(5, SmpteFrameRate::Fps2997Ndf);
    let ndf = mtc_cycle_nibbles(&tc, SmpteFrameRate::Fps2997Ndf);
    assert_eq!((ndf >> 28) & 0x06, 2 << 1);
    let df = mtc_cycle_nibbles(&tc, SmpteFrameRate::Fps2997Df);
    assert_eq!(ndf, df);
}

/// A rig at 120 BPM and 48 kHz (24 000 frames a beat, a tick every 1 000)
/// looping beats 1.0..1.1, so a block starting on beat 1.0 wraps on frame
/// 2 400. Returns the rig, its transport and its receiver, rolling.
fn looping_rig() -> (Rig, Arc<TestTransport>, crate::MidiReceiver) {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    transport.set_loop(1.0, 1.1);
    transport.set_beat(1.0);
    transport.set_playing(true);
    (cm, transport, cons)
}

/// The `(offset, ...)` of the events with `status`.
fn offsets(events: &[UmpAt], status: u8) -> Vec<u32> {
    events
        .iter()
        .filter(|e| is_status(e, status))
        .map(|e| e.0.get())
        .collect()
}

/// **A loop wrap inside a block moves the ticks onto the next pass.** A
/// 4 000-frame block starting on the loop's start (beat 1.0, which is on
/// tick 24) wraps on frame 2 400. Its first pass holds the ticks on frames
/// 0, 1 000 and 2 000; the second starts again on the loop's start beat, so
/// its ticks are on 2 400 (the wrap's own frame) and 3 400. Ticks that ran
/// on linearly past the loop's end would be on 0, 1 000, 2 000 and 3 000.
/// The next block starts where this one ended (1 600 frames into the loop),
/// which is no locate: no Song Position, and its tick on frame 400.
///
/// Mutation (run): place the ticks from the block's first beat, one pass for
/// the whole block (the unmodified `tick`) → ticks on 0, 1 000, 2 000, 3 000
/// → fails. Mutation (run): store the beat the block would reach without
/// the wrap as `prev.end` (the old `beat + advance`) → a Song Position on
/// the second block's frame 0 → fails.
#[test]
fn a_loop_wrap_inside_a_block_moves_the_ticks_onto_the_next_pass() {
    let (cm, transport, cons) = looping_rig();
    cm.tick(4_000);
    let first = drain_all(&cons);
    assert_eq!(offsets(&first, 0xF8), [0, 1_000, 2_000, 2_400, 3_400]);

    // The block ended 1 600 frames into the loop: beat 1 + 1600 / 24000. The
    // next one starts there, short of the wrap: no locate, and the tick on
    // beat 1 + 2000 / 24000.
    transport.set_beat(1.0 + 1_600.0 / 24_000.0);
    cm.tick(700);
    let second = drain_all(&cons);
    assert_eq!(offsets(&second, 0xF8), [400]);
    assert!(
        offsets(&second, 0xF2).is_empty(),
        "the block start is no locate"
    );
}

/// **The Song Position goes out on the wrap's frame**, carrying the loop's
/// start (beat 1.0 is MIDI beat 4; the end, 1.5, is 6) and before the tick
/// on that frame. A loop of 1.0..1.5 wraps on frame 12 000; the play edge's
/// own Song Position is on frame 0.
///
/// Mutation (run): send the wrap's Song Position at offset 0 → none on
/// 12 000 → fails. Mutation (run): send the beat the pass would have
/// reached (the loop's end) → position 6 → fails. Mutation (run): emit the
/// pass's ticks before its Song Position → the F8 precedes the F2 on frame
/// 12 000 → fails.
#[test]
fn the_song_position_lands_on_the_wraps_frame() {
    let (cm, transport, cons) = looping_rig();
    transport.set_loop(1.0, 1.5);
    cm.tick(13_000);
    let events = drain_all(&cons);
    let positions: Vec<_> = events
        .iter()
        .filter_map(|e| match system(e.1) {
            Some(System::SongPosition(position)) => Some((e.0.get(), position.get())),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(0, 4), (12_000, 4)]);
    let at_wrap: Vec<_> = events.iter().filter(|e| e.0.get() == 12_000).collect();
    assert!(
        is_status(at_wrap[0], 0xF2) && is_status(at_wrap[1], 0xF8),
        "the Song Position, then the wrapped pass's first tick"
    );
}

/// **The MTC grid restarts on the wrap's frame**, as a locate restarts it:
/// piece 0 goes out on the wrap, the pieces before it on the first pass's
/// grid. At 25 fps and 48 kHz a quarter-frame is 480 frames, so the first
/// pass sends pieces 0..4 on 0, 480, ..., 1 920 and the second pieces 0..3
/// on 2 400, 2 880, 3 360, 3 840. Unrestarted, the grid runs on and piece 5
/// is on 2 400.
///
/// Mutation (run): drop `restart_mtc` on the wrap → pieces 5, 6, 7, 0 on
/// the second pass → fails.
#[test]
fn the_mtc_grid_restarts_on_the_wrap() {
    // The loop starts on beat 0, which is on an even frame, so a restarted
    // grid puts piece 0 on the wrap's own frame.
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    transport.set_loop(0.0, 0.1);
    transport.set_playing(true);
    cm.set_send_mtc(true);
    cm.tick(4_000);
    let events = drain_all(&cons);
    let qf: Vec<_> = events
        .iter()
        .filter_map(|e| match system(e.1) {
            Some(System::TimeCode(code)) => Some((e.0.get(), code.get() >> 4)),
            _ => None,
        })
        .collect();
    assert_eq!(
        qf,
        [
            (0, 0),
            (480, 1),
            (960, 2),
            (1_440, 3),
            (1_920, 4),
            (2_400, 0),
            (2_880, 1),
            (3_360, 2),
            (3_840, 3)
        ]
    );
}

/// Plays the transport (120 BPM, 48 kHz, rolling from `from`, looping
/// `region`) through blocks of `lens`, the host's clock giving each block its
/// start beat, and returns every event with its absolute frame, in the order
/// sent. `mtc` turns the quarter-frames on.
fn play_layout(from: f64, region: (f64, f64), mtc: bool, lens: &[usize]) -> Vec<(u64, UmpAt)> {
    let (cm, transport, cons) = master(Bpm::of(120.0), SampleRate::new(48_000.0));
    transport.set_loop(region.0, region.1);
    transport.set_playing(true);
    cm.set_send_mtc(mtc);
    let looping = tutti_types::LoopRange::new(
        tutti_types::Beat::new(region.0),
        tutti_types::Beat::new(region.1),
    );
    let mut host = tutti_types::FrameClock::new(Beat(from), Bpm::of(120.0), SampleRate(48_000.0));
    let mut frame = 0u64;
    let mut out = Vec::new();
    for &len in lens {
        transport.set_beat(host.beat().get());
        cm.tick(len);
        out.extend(
            drain_all(&cons)
                .into_iter()
                .map(|e| (frame + u64::from(e.0.get()), e)),
        );
        host.advance(Samples(len), looping);
        frame += len as u64;
    }
    out
}

/// Block lengths covering `total` frames in blocks of `size`, the last short.
fn fixed(total: usize, size: usize) -> Vec<usize> {
    let mut lens = vec![size; total / size];
    if !total.is_multiple_of(size) {
        lens.push(total % size);
    }
    lens
}

/// Block lengths cut at the frames in `cuts` (and `total`).
fn cut_at(total: usize, cuts: &[usize]) -> Vec<usize> {
    let mut at = 0;
    let mut lens = Vec::new();
    for &c in cuts.iter().chain(std::iter::once(&total)) {
        lens.push(c - at);
        at = c;
    }
    lens
}

/// **The stream does not depend on where the block boundaries fall.** The
/// loop is beat 0 to 11 999.3 frames on, at 120 BPM and 48 kHz (24 000 frames
/// a beat), so the first wrap is on frame 12 000 and, a fraction of a frame
/// over each time, the next on 23 999, 35 998, 47 998 and 59 997: the wrap frames are
/// fixed by the loop. The same 60 000 frames through one block, blocks of 512
/// and of 1 000, blocks cut exactly on each wrap (the wrap on a block's first
/// frame) and blocks cut one frame after each wrap, must send the same ticks,
/// Song Positions and MTC pieces on the same absolute frames; a Song Position
/// is on every wrap frame and none other, and so is a tick on the loop's
/// start.
///
/// Mutation (run): drop the carried-wrap Song Position (`|| wrapped` in
/// `tick`) → the layout cut on the wraps has no Song Position, nor a
/// restarted MTC grid, on them → fails.
#[test]
fn the_stream_does_not_depend_on_the_block_boundaries() {
    const TOTAL: usize = 60_000;
    let region = (0.0, 11_999.3 / 24_000.0);
    let whole = play_layout(0.0, region, true, &[TOTAL]);

    let wraps: Vec<u64> = whole
        .iter()
        .filter(|(_, e)| is_status(e, 0xF2))
        .map(|&(at, _)| at)
        .collect();
    assert_eq!(wraps, [12_000, 23_999, 35_998, 47_998, 59_997]);
    for &w in &wraps {
        assert!(
            whole.iter().any(|(at, e)| *at == w && is_status(e, 0xF8)),
            "a tick on the loop's start at {w}"
        );
    }
    assert!(whole.iter().any(|(_, e)| is_status(e, 0xF1)), "MTC is on");

    let cuts: Vec<usize> = wraps.iter().map(|&w| w as usize).collect();
    let after: Vec<usize> = cuts.iter().map(|c| c + 1).collect();
    for (name, lens) in [
        ("512", fixed(TOTAL, 512)),
        ("1000", fixed(TOTAL, 1_000)),
        ("on the wraps", cut_at(TOTAL, &cuts)),
        ("one past the wraps", cut_at(TOTAL, &after)),
    ] {
        assert_eq!(lens.iter().sum::<usize>(), TOTAL);
        let got = play_layout(0.0, region, true, &lens);
        let key = |v: &[(u64, UmpAt)]| {
            let mut v: Vec<_> = v.iter().map(|&(at, e)| (at, e.1.0[0])).collect();
            v.sort_by_key(|&(at, _)| at);
            v
        };
        assert_eq!(key(&got), key(&whole), "blocks {name}");
    }
}

/// **A bar-aligned loop on power-of-two blocks sends its Song Position.**
/// A loop of beats 0..8 at 120 BPM and 48 kHz is 192 000 frames, 375 blocks
/// of 512, so every wrap is on a block's first frame. Three cycles send a
/// Song Position (to beat 0) on each of the first two wraps, frames 192 000
/// and 384 000, and Start on frame 0 for the play edge.
///
/// Mutation (run): as above, drop the carried-wrap Song Position → none →
/// fails.
#[test]
fn a_loop_wrapping_on_every_block_edge_sends_a_song_position_each_time() {
    let events = play_layout(0.0, (0.0, 8.0), false, &fixed(3 * 192_000, 512));
    let positions: Vec<_> = events
        .iter()
        .filter_map(|(at, e)| match system(e.1) {
            Some(System::SongPosition(position)) => Some((*at, position.get())),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(192_000, 0), (384_000, 0)]);
}

/// **A real locate on the frame of a wrap sends one Song Position**, to
/// where it locates; and a re-prepare between the blocks (`prepare` re-rates
/// the master) keeps the wrap that ended the first.
///
/// Mutation (run): send the carried wrap's Song Position as well as the
/// locate's (`located || wrapped` split into two sends) → two on frame 0 →
/// fails. Mutation (run): clear the carried wrap in `set_sample_rate` → none
/// after the re-prepare → fails (the second half).
#[test]
fn a_wrap_on_the_block_edge_and_a_locate_are_one_song_position() {
    let (cm, transport, cons) = looping_rig();
    cm.tick(2_400);
    let _ = drain_all(&cons);
    transport.set_beat(1.5);
    cm.tick(100);
    let positions: Vec<_> = drain_all(&cons)
        .iter()
        .filter_map(|e| match system(e.1) {
            Some(System::SongPosition(position)) => Some((e.0.get(), position.get())),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(0, 6)], "one, to the locate");

    let (cm, transport, cons) = looping_rig();
    cm.tick(2_400);
    let _ = drain_all(&cons);
    cm.set_sample_rate(SampleRate::new(48_000.0));
    transport.set_beat(1.0);
    cm.tick(2_400);
    let second = drain_all(&cons);
    assert_eq!(offsets(&second, 0xF2), [0], "the wrap that ended block one");
    assert_eq!(offsets(&second, 0xF8), [0, 1_000, 2_000]);
}

/// A song position is the beat in sixteenths, floored at 0 (NaN and negatives
/// included) and stuck at the largest 14-bit value past the range.
/// Mutation: drop the `unwrap_or(U14::MAX)` for `U14::of(0)` and the huge
/// inputs fail.
#[test]
fn a_song_position_is_in_sixteenths_and_saturates() {
    let at = |beat: f64| beats_to_midi_beats(Beat(beat)).get();
    assert_eq!(at(2.0), 8);
    assert_eq!(at(0.0), 0);
    assert_eq!(at(-3.0), 0);
    assert_eq!(at(f64::NAN), 0);
    assert_eq!(at(0x3FFF as f64 / 4.0), 0x3FFF);
    assert_eq!(at(1e9), 0x3FFF);
    assert_eq!(at(f64::INFINITY), 0x3FFF);
}
