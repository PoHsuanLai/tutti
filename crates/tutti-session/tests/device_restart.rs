//! A restart of the output device, at whatever rate it now runs, through a
//! session on `ManualStreamDriver`s standing for the devices: the graph and
//! the transport are re-rated and every scheduled command keeps its wall-clock
//! time, a refused restart keeps playing, a wider device widens the root, a
//! graph that no longer compiles leaves the stream stopped but recoverable,
//! and a crossfade asked for right after a restart lands at once.
//!
//! These are the session halves of `bevy-tutti`'s `engine::restart` tests,
//! moved here when the adapter was parked. What stayed with the adapter is
//! its glue: `AudioConfig` and `GraphLatency` republished after the hook, the
//! disk streamer's `set_sample_rate` in the hook's follow-up, and the device
//! going back into the world when a restart panics. The plugin half is
//! `plugin_latency.rs`'s, the MIDI clock's is `device_midi_clock.rs`'s.

use tutti_cpal::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_graph::{CrossfadeCurve, ForkByClone};
use tutti_nodes::LimiterNode;
use tutti_nodes::testing::{Osc, Through};
use tutti_session::{Crossfade, Error, GraphSource, Session};
use tutti_transport::MotionEvent;
use tutti_types::{At, ChannelLayout, Db, Frame, Hz, NodeKey, SampleRate, Samples, Seconds};

const OLD: f64 = 44_100.0;
const NEW: f64 = 48_000.0;

fn spec_at(rate: f64) -> OutputSpec {
    spec_with(rate, 2)
}

fn spec_with(rate: f64, channels: usize) -> OutputSpec {
    OutputSpec::new(
        SampleRate(rate),
        ChannelLayout::try_from(channels).expect("a test width is small"),
        tutti_cpal::cpal::SampleFormat::F32,
    )
}

/// A session built at 44.1 kHz on a manual stream: a 1 kHz sine on output 0
/// and, through a lookahead limiter, on output 1, so the dry channel
/// pre-rolls by the limiter's latency.
fn engine() -> (
    Session,
    tutti_session::Device,
    ManualStream,
    NodeKey,
    NodeKey,
) {
    let (driver, stream) = ManualStreamDriver::new();
    let (mut s, device) = Session::builder()
        .build_device_on(spec_at(OLD), driver)
        .expect("builds with no device");
    let (osc, _) = s.graph().add(ForkByClone(Osc::sine(Hz(1_000.0))));
    let (lim, _) = s.graph().add(LimiterNode::with_channels(
        ChannelLayout::MONO,
        Db(0.0),
        Db(0.0),
    ));
    s.graph().set_source(lim, 0, GraphSource::Node(osc, 0));
    s.graph().set_output_source(0, GraphSource::Node(osc, 0));
    s.graph().set_output_source(1, GraphSource::Node(lim, 0));
    s.update();
    (s, device, stream, osc, lim)
}

/// Render `blocks` device blocks of `frames` on `stream`, an update of the
/// session after each (as a host's frames interleave with its callbacks): the
/// left channel.
fn play(s: &mut Session, stream: &ManualStream, frames: usize, blocks: usize) -> Vec<f32> {
    let mut left = Vec::new();
    for _ in 0..blocks {
        let block = stream.render_block(frames).expect("the stream is open");
        left.extend(block.iter().step_by(2));
        s.update();
    }
    left
}

/// Mean period, in frames, of the rising zero crossings in `x` (interpolated
/// between the two frames that straddle each).
fn period(x: &[f32]) -> f64 {
    let crossings: Vec<f64> = (1..x.len())
        .filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0)
        .map(|i| (i - 1) as f64 + f64::from(x[i - 1] / (x[i - 1] - x[i])))
        .collect();
    assert!(crossings.len() > 10, "a tone to measure");
    (crossings[crossings.len() - 1] - crossings[0]) / (crossings.len() - 1) as f64
}

/// **A restart onto a 48 kHz device re-rates everything.** One second at
/// 44.1 kHz, rolling at 120 BPM with a stop scheduled at `Frame(66 150)`
/// (1.5 s at 44.1 kHz); then the restart, and one second on the new device:
///
/// - the 1 kHz sine is 48 frames a cycle, not 44.1 (the graph runs at the
///   device's rate);
/// - the first block on the new device plays: the re-prepare finished in the
///   restart's hook, so there is no silent checked-out block;
/// - the beat carries on from 2.0 without a jump: 2.02 after the first
///   480-frame block;
/// - the stop keeps its wall-clock time: it lands 0.5 s after the restart, on
///   frame 72 000 (beat 3.0 to the frame);
/// - a play scheduled *after* the restart, at 48 kHz frame 84 000, is not
///   rescaled a second time when the engine adopts the rate: it lands on its
///   frame (beat 3.5 half a beat later, to the frame);
/// - the graph's rate, the transport's rate, and the PDC figures (the
///   published total and the dry channel's pre-roll) are the new ones
///   **before the first block**: the limiter's lookahead is a time, so its
///   latency in samples moved, and a disk source seeking in that block
///   pre-rolls by the new figure.
///
/// Mutations (each run, each failed): `Session::rerate` not re-preparing the
/// graph (the `reprepare` call removed); `rerate` not setting the transport's
/// rate (`transport.set_sample_rate` removed: the transport's-rate
/// assertion); `rerate` not settling the engine and collecting
/// (`apply_pending` and `collect` removed, leaving the second half to the next
/// update) -> the re-prepare is refused or the first block silent.
///
/// Not pinned (the mutation survives, verified): `Schedule::rescale`
/// ignoring the `mark_rate_change` boundary, which `bevy-tutti`'s version of
/// this test named. The engine adopts the rate (`follow_rate`, rescaling the
/// commands in flight) inside the restart's hook, before the host's thread can
/// send anything, so a command sent "after the mark" cannot be in flight at
/// the rescale on one thread: the play at 84 000 below is asserted to land
/// on its frame, but it would also land there without the boundary.
#[test]
fn a_restart_at_a_new_rate_re_rates_the_graph_and_everything_on_it() {
    let (mut s, mut device, old, _, lim) = engine();
    let t = s.transport().clone();
    t.motion.try_send(MotionEvent::Play).expect("room");
    t.motion
        .schedule(At::Frame(Frame(66_150)), MotionEvent::stop_unfaded())
        .expect("room");
    let before = play(&mut s, &old, 441, 100);
    assert!(
        (period(&before[4_410..]) - 44.1).abs() < 0.01,
        "1 kHz at 44.1 kHz"
    );
    assert!((t.settings.beat().get() - 2.0).abs() < 1e-9, "one second");
    let old_latency = s.graph_ref().node_latency(lim);
    assert_eq!(s.latency(), old_latency, "the 44.1 kHz figure is published");

    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(NEW), driver, None, |_| {})
        .expect("restarts");
    assert!(!old.is_open() && new.is_open(), "on the new device");
    assert_eq!(s.graph_ref().prepared().sample_rate(), SampleRate(NEW));
    assert_eq!(t.sample_rate(), SampleRate(NEW), "the transport's rate");
    // Before the first block: the re-prepare finished inside the hook, and
    // the figures a disk source pre-rolls by are the new rate's.
    let new_latency = s.graph_ref().node_latency(lim);
    assert_ne!(new_latency, old_latency, "the lookahead moved in samples");
    assert_eq!(
        s.latency(),
        new_latency,
        "the new figure is published before the first block"
    );
    assert_eq!(
        s.compensation().to_vec().first().copied(),
        Some(new_latency),
        "and the dry channel's pre-roll with it"
    );

    // Sent after the restart, before the first block at the new rate: already
    // in 48 kHz frames (1.75 s of wall clock), so the rate's adoption must
    // leave it where it is.
    t.motion
        .schedule(At::Frame(Frame(84_000)), MotionEvent::Play)
        .expect("room");

    let mut after = play(&mut s, &new, 480, 1);
    assert!(
        after.iter().any(|x| x.abs() > 0.5),
        "the first block at the new rate plays the graph, not the silence \
         of a re-prepare with its units out"
    );
    assert!(
        (t.settings.beat().get() - 2.02).abs() < 1e-9,
        "the beat carries on from 2.0 at the new rate, not from {}",
        t.settings.beat().get()
    );
    after.extend(play(&mut s, &new, 480, 54));
    // One frame of 48 kHz is 1/24 000 of a beat: 1e-9 pins the frame.
    assert!(t.motion.is_stopped(), "the stop landed");
    assert!(
        (t.settings.beat().get() - 3.0).abs() < 1e-9,
        "the stop lands on 1.5 s of wall clock, frame 72 000 (beat 3), not at beat {}",
        t.settings.beat().get()
    );
    after.extend(play(&mut s, &new, 480, 45));
    assert!(t.motion.is_playing(), "the later play landed");
    assert!(
        (t.settings.beat().get() - 3.5).abs() < 1e-9,
        "rolling again from frame 84 000, 12 000 frames to go (beat 3.5), not beat {}",
        t.settings.beat().get()
    );
    assert!(
        (period(&after[4_800..]) - 48.0).abs() < 0.01,
        "1 kHz at 48 kHz, measured {}",
        period(&after[4_800..])
    );

    let new_latency = s.graph_ref().node_latency(lim);
    assert_ne!(new_latency, old_latency, "the lookahead moved in samples");
    assert_eq!(s.latency(), new_latency);
    assert_eq!(
        s.compensation().to_vec().first().copied(),
        Some(new_latency),
        "the dry channel pre-rolls by the new latency"
    );
}

/// **A block past the engine's capacity is refused before the stream stops**:
/// the old device plays on, at the old configuration, and the host gets the
/// graph's reason. The capacity is the engine's (`DEFAULT_BLOCK_CAPACITY`,
/// 8 192 frames). Within it, the same restart goes through.
///
/// Mutation (run, failed): `Session::check_restart` not calling
/// `check_reprepare` -> the stream stops first and the refusal comes from the
/// re-prepare in the hook, with the old device stopped.
#[test]
fn a_restart_past_the_block_capacity_is_refused_and_keeps_playing() {
    let (mut s, mut device, old, _, _) = engine();
    let (driver, new) = ManualStreamDriver::new();
    let err = s
        .restart_on(
            &mut device,
            spec_at(NEW),
            driver,
            Some(Samples(16_384)),
            |_| {},
        )
        .expect_err("past the capacity");
    assert!(
        matches!(
            err,
            Error::Reprepare(tutti_graph::CommitError::BlockTooLong {
                max_block: 16_384,
                limit: 8_192
            })
        ),
        "{err}"
    );
    assert!(old.is_open() && !new.is_open(), "the old device plays on");
    assert_eq!(device.spec().sample_rate, SampleRate(OLD));
    assert_eq!(device.graph_rate(), SampleRate(OLD));
    assert!(play(&mut s, &old, 441, 10).iter().any(|&x| x != 0.0));

    // Within it, the same restart goes through (the widening onto the 5.1
    // device it picks is `a_restart_onto_a_wider_device_widens_the_root`'s).
    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(
        &mut device,
        spec_with(NEW, 6),
        driver,
        Some(Samples(2_048)),
        |_| {},
    )
    .expect("at a block the engine holds");
    assert!(new.is_open());
}

/// **A restart onto a wider device widens the root, as the build does; onto a
/// narrower one it keeps it.** Stereo at 44.1 kHz, then a 5.1 device at
/// 48 kHz: the root is six wide (the device a floor), so a host routing to the
/// surround channels finds them there rather than zero-filled by the fold,
/// the device reports its six, and the audio side renders six: the
/// compensation table, which the committed graph's figures fill, has an entry
/// per root channel. Then back onto a stereo device: the root keeps its six
/// and the engine folds.
///
/// Mutation (run, failed): `Session::rerate` not widening (the widening
/// branch skipped) -> the root stays two.
#[test]
fn a_restart_onto_a_wider_device_widens_the_root() {
    let (mut s, mut device, _old, _, _) = engine();
    assert_eq!(s.graph_ref().outputs(), 2);

    let (driver, surround) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_with(NEW, 6), driver, None, |_| {})
        .expect("restarts");
    assert_eq!(
        s.graph_ref().outputs(),
        6,
        "the root takes the device's six"
    );
    assert_eq!(device.spec().channels, ChannelLayout::from(6u16));
    for _ in 0..4 {
        let block = surround.render_block(480).expect("the stream is open");
        assert_eq!(block.len(), 480 * 6);
        s.update();
    }
    assert_eq!(
        s.compensation().to_vec().len(),
        6,
        "the committed root is six wide"
    );

    let (driver, stereo) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(NEW), driver, None, |_| {})
        .expect("restarts");
    assert_eq!(s.graph_ref().outputs(), 6);
    assert_eq!(device.spec().channels, ChannelLayout::STEREO);
    let out = play(&mut s, &stereo, 480, 20);
    assert!(
        (period(&out[4_800..]) - 48.0).abs() < 0.01,
        "the six fold to the stereo device, the sine on the left"
    );
}

/// **After a hook fails, a restart onto the old device and rate plays again.**
/// The hook fails with the stream stopped (here: an edit the update has not
/// committed feeds the limiter from a node it feeds, so the graph no longer
/// compiles and its re-prepare refuses); the device keeps the old spec and
/// graph rate. Then the wiring is put back, and a restart at 44.1 kHz moves
/// nothing and renders the 1 kHz sine at 44.1 frames a cycle.
///
/// Mutation (run, failed): the driver's `rerate_or_restore` not putting the
/// old spec back on failure (`set_spec(old)` removed) -> the device claims
/// 48 kHz.
#[test]
fn a_restart_after_a_failed_hook_recovers_on_the_old_device() {
    let (mut s, mut device, old, osc, lim) = engine();
    let (loop_back, ()) = s.graph().insert(Through::mono());
    s.graph()
        .set_source(loop_back, 0, GraphSource::Node(lim, 0));
    s.graph()
        .set_source(lim, 0, GraphSource::Node(loop_back, 0));
    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(NEW), driver, None, |_| {})
        .expect_err("the graph no longer compiles");
    assert!(!old.is_open() && !new.is_open(), "left stopped");
    assert_eq!(device.graph_rate(), SampleRate(OLD));
    assert_eq!(device.spec().sample_rate, SampleRate(OLD));

    // The declared wiring writes the limiter's input back.
    s.graph().set_source(lim, 0, GraphSource::Node(osc, 0));
    s.graph().remove(loop_back);
    s.update();
    let (driver, back) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(OLD), driver, None, |_| {})
        .expect("recovers");
    assert!(back.is_open());
    let out = play(&mut s, &back, 441, 20);
    assert!(
        (period(&out[4_410..]) - 44.1).abs() < 0.01,
        "plays again at 44.1 kHz"
    );
}

/// **A crossfade asked for right after a restart lands at once, at the new
/// rate**: the restart's re-prepare finished inside the hook, so the graph is
/// not between its halves and nothing waits. The incoming 2 kHz sine plays at
/// the new rate: 24 frames a cycle.
///
/// The waiting case (a crossfade while a graph re-prepares) is pinned by
/// `graph.rs`'s `a_crossfade_refused_busy_during_a_re_prepare_lands_on_a_later_update`.
///
/// Mutation (run, failed): `Session::rerate` not settling the engine and
/// collecting (`apply_pending` and `collect` removed) -> the re-prepare is not
/// resumed when the crossfade is asked for, and it waits (or the restart is
/// refused).
#[test]
fn a_crossfade_after_the_restart_lands_at_once() {
    let (mut s, mut device, _old, osc, _) = engine();
    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(NEW), driver, None, |_| {})
        .expect("restarts");
    let landed = s
        .graph()
        .crossfade(
            osc,
            ForkByClone(Osc::sine(Hz(2_000.0))),
            Seconds(0.005),
            CrossfadeCurve::EqualAmplitude,
        )
        .expect("the crossfade is accepted");
    assert!(
        matches!(landed, Crossfade::Landed(_)),
        "nothing waits: the re-prepare is done"
    );
    assert_eq!(s.graph_ref().waiting_crossfades(), 0);
    let out = play(&mut s, &new, 480, 50);
    assert_eq!(s.graph_ref().waiting_crossfades(), 0);
    assert!(
        (period(&out[4_800..]) - 24.0).abs() < 0.01,
        "2 kHz at 48 kHz, measured {}",
        period(&out[4_800..])
    );
}
