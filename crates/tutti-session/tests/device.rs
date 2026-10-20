//! A session on a device, device-free: `build_device_on` and `restart_on`
//! over `ManualStreamDriver`s standing for the devices.

use tutti_core::{ChannelLayout, Db, Hz, SampleRate, Samples};
use tutti_cpal::{ManualStreamDriver, OutputSpec};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Osc;
use tutti_nodes::LimiterNode;
use tutti_session::{GraphSource, Session};

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate(rate),
        ChannelLayout::STEREO,
        tutti_cpal::cpal::SampleFormat::F32,
    )
}

/// **The session publishes the tap its callback feeds**, not a fresh
/// disconnected one: a host opens `Session::tap` and sees the frames the
/// callback renders. (Moved from `bevy-tutti`'s `engine::build`, where it
/// pinned `AudioTapRes`; its adapter half there now checks the resource is
/// the session's tap.)
///
/// Mutation (run): `SessionBuilder::build_on` handing the callback state
/// `AudioTap::new()` instead of the session's → the opened consumer sees
/// nothing.
#[test]
fn the_session_publishes_the_tap_its_callback_feeds() {
    let (driver, stream) = ManualStreamDriver::new();
    let (session, _device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let mut consumer = session.tap().open().expect("a fresh tap opens");
    stream.render_block(64).expect("the stream is open");
    let mut frames = 0;
    while consumer.try_pop().is_some() {
        frames += 1;
    }
    assert_eq!(frames, 64, "the callback's block reached the session's tap");
}

/// **A rerate from 48 kHz to 96 kHz keeps the published compensation in
/// step with the plan.** A 1 kHz sine dry on output 0 and through a
/// lookahead limiter on output 1: the dry channel pre-rolls by the
/// limiter's latency, a time, so in samples it doubles with the rate. After
/// the restart, before any block at the new rate, the published table and
/// total are the figures of the plan the audio side holds (the re-prepare's
/// resumed plan), and they are the limiter's new latency.
///
/// Mutations (run):
/// - the restart hook not calling `publish_sent` → the 48 kHz figures stay
///   published → fails;
/// - the hook not settling the engine and collecting (leaving the second
///   half to the next update's collect) → the hook's own check finds the
///   re-prepare still pending and the restart is refused → fails.
#[test]
fn a_rerate_keeps_the_published_compensation_in_step_with_the_plan() {
    let (driver, old) = ManualStreamDriver::new();
    let (mut s, mut device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
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
    old.render_block(480).expect("open");
    s.update();

    let in_step = |s: &Session, what: &str| -> Samples {
        let (channels, total) = s.graph_ref().sent_compensation().expect("a plan");
        let lim = s.graph_ref().node_latency(lim);
        assert_eq!(total, lim, "{what}: the plan compensates the limiter");
        assert_eq!(s.latency(), total, "{what}: the total is the plan's");
        assert_eq!(
            s.compensation_reader().lend().as_slice(),
            channels.as_slice(),
            "{what}: the table is the plan's"
        );
        assert_eq!(
            channels.first().copied(),
            Some(lim),
            "{what}: dry pre-rolls"
        );
        lim
    };
    let at48 = in_step(&s, "48 kHz");
    assert!(at48 > Samples(0), "a lookahead to compensate");

    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(96_000.0), driver, None, |_| {})
        .expect("restarts");
    assert!(!old.is_open() && new.is_open(), "on the new device");
    assert_eq!(s.graph_ref().prepared().sample_rate(), SampleRate(96_000.0));
    assert_eq!(s.transport().sample_rate(), SampleRate(96_000.0));
    let at96 = in_step(&s, "96 kHz, before the first block");
    assert_eq!(at96, Samples(2 * at48.0), "the lookahead is a time");

    for _ in 0..4 {
        new.render_block(960).expect("open");
        s.update();
    }
    in_step(&s, "96 kHz, after");
}
