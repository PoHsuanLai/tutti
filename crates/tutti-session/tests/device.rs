//! A session on a device, device-free: `build_device_on` and `restart_on`
//! over `ManualStreamDriver`s standing for the devices.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_cpal::cpal::StreamError;
use tutti_cpal::{
    DeviceId, DeviceSelector, Direction, ManualStreamDriver, OutputSpec, Snapshot, SnapshotSource,
};
use tutti_graph::ForkByClone;
use tutti_nodes::LimiterNode;
use tutti_nodes::testing::Osc;
use tutti_session::{
    Backoff, DevicePolicy, DeviceRestart, GraphSource, Phase, PhaseCx, Session, SessionEvent,
};
use tutti_types::{ChannelLayout, Db, Hz, SampleRate, Samples};

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

/// **The session's load meter is the one its callback's engine feeds**:
/// each block the device renders is one block measured.
///
/// Mutation (run): `build_on` leaving `Session::load` at the engine-less
/// `None` → `load_meter()` is `None` → fails; giving the session a fresh
/// `LoadMeter` instead of the engine's → no block measured → fails.
#[test]
fn the_session_reads_the_load_its_callback_measures() {
    let (driver, stream) = ManualStreamDriver::new();
    let (session, _device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let meter = std::sync::Arc::clone(session.load_meter().expect("a device's engine"));
    for _ in 0..3 {
        stream.render_block(64).expect("the stream is open");
    }
    assert_eq!(meter.take().blocks, 3);
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
        let sent = s.graph_ref().sent_compensation().expect("a plan");
        let (channels, total) = (sent.channels(), sent.total());
        let lim = s.graph_ref().node_latency(lim);
        assert_eq!(total, lim, "{what}: the plan compensates the limiter");
        assert_eq!(s.latency(), total, "{what}: the total is the plan's");
        assert_eq!(
            s.compensation().to_vec().as_slice(),
            channels,
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

// ---- the device watch (design 018) ----

/// A source the test feeds by hand.
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<VecDeque<Snapshot>>>);

impl Fake {
    fn show(&self, outputs: &[&str], default: Option<&str>) {
        self.0.lock().unwrap().push_back(Snapshot::from_names(
            outputs.iter().copied(),
            [],
            default,
            None,
        ));
    }
}

impl SnapshotSource for Fake {
    fn poll(&mut self) -> Option<Snapshot> {
        self.0.lock().unwrap().pop_front()
    }
}

fn id(name: &str) -> DeviceId {
    DeviceId::new(name)
}

/// Runs the `Poll` phase at `base + ms`, as an update would.
fn poll(session: &mut Session, base: Instant, ms: u64) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    session.run_phase(
        Phase::Poll,
        PhaseCx::at(base + Duration::from_millis(ms)),
        &mut out,
    );
    out
}

fn wants(events: &[SessionEvent]) -> Vec<DeviceSelector> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::RestartWanted(r) => Some(r.device.clone()),
            _ => None,
        })
        .collect()
}

fn policy() -> DevicePolicy {
    DevicePolicy {
        follow_default: true,
        reconnect: true,
        backoff: Backoff {
            base: Duration::from_millis(500),
            max: Duration::from_secs(4),
            settle: Duration::from_secs(10),
        },
        ..DevicePolicy::default()
    }
}

/// **Appearing, vanishing and a moved default are session events**, the same
/// headless and in an app, with no policy on: nothing restarts.
///
/// Mutation (run): `watch.poll` not called from `device_poll` → no event.
#[test]
fn the_watch_reports_what_the_host_lists() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut session, _device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        DevicePolicy::default(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    assert!(poll(&mut session, base, 0).is_empty(), "no snapshot yet");

    fake.show(&["A"], Some("A"));
    let events = poll(&mut session, base, 10);
    assert!(
        matches!(
            events.as_slice(),
            [
                SessionEvent::DeviceAppeared {
                    direction: Direction::Output,
                    ..
                },
                SessionEvent::DefaultChanged {
                    direction: Direction::Output,
                    ..
                },
            ]
        ),
        "{events:?}"
    );

    fake.show(&["A", "B"], Some("B"));
    let events = poll(&mut session, base, 20);
    assert!(
        matches!(
            events.as_slice(),
            [SessionEvent::DeviceAppeared { id, .. }, SessionEvent::DefaultChanged { .. }] if id.name() == "B"
        ),
        "{events:?}"
    );
    assert!(wants(&events).is_empty(), "the policy is off");

    fake.show(&["B"], Some("B"));
    let events = poll(&mut session, base, 30);
    assert!(
        matches!(
            events.as_slice(),
            [SessionEvent::DeviceLost { id, .. }] if id.name() == "A"
        ),
        "{events:?}"
    );
}

/// **The default moving restarts a stream that follows it, and the restart's
/// outcome says the rate moved** (the host reopens its mic on it).
///
/// Mutation (run): `note_restart` pushing no `DeviceRestarted` → fails;
/// `rate_changed` always true → the second half fails.
#[test]
fn following_the_default_restarts_and_reports_the_rate() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut session, mut device) = Session::builder()
        .build_device_on(spec_at(44_100.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    assert!(wants(&poll(&mut session, base, 0)).is_empty());

    fake.show(&["A", "B"], Some("B"));
    let events = poll(&mut session, base, 100);
    assert_eq!(wants(&events), [DeviceSelector::Default]);

    // The host runs it (here on a device-free driver at 48 kHz).
    let (driver, new) = ManualStreamDriver::new();
    session
        .restart_on(&mut device, spec_at(48_000.0), driver, None, |_| {})
        .unwrap();
    assert!(new.is_open());
    let events = poll(&mut session, base, 110);
    let [SessionEvent::DeviceRestarted(r)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert!(r.rate_changed());
    assert_eq!(
        (r.previous_rate, r.rate),
        (SampleRate(44_100.0), SampleRate(48_000.0))
    );

    // Another restart at the same rate is not a rate change.
    let (driver, _s) = ManualStreamDriver::new();
    session
        .restart_on(&mut device, spec_at(48_000.0), driver, None, |_| {})
        .unwrap();
    let events = poll(&mut session, base, 120);
    let [SessionEvent::DeviceRestarted(r)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert!(!r.rate_changed());
}

/// **The session never overrides an explicit choice.** A stream the host
/// put on "A" stays silent while "A" is gone, whatever else is the default,
/// and is asked back onto "A" only when it returns.
///
/// Mutation (run): the policy's `Id` goal made `Default` → the second poll
/// wants a restart onto the default, and this fails.
#[test]
fn an_explicit_device_is_not_replaced_by_the_default() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut session, _device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        policy(),
        DeviceSelector::named("A"),
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("B"));
    assert!(wants(&poll(&mut session, base, 0)).is_empty());
    fake.show(&["B"], Some("B"));
    assert!(
        wants(&poll(&mut session, base, 5_000)).is_empty(),
        "A gone, B is not a substitute"
    );
    fake.show(&["A", "B"], Some("B"));
    assert_eq!(
        wants(&poll(&mut session, base, 10_000)),
        [DeviceSelector::named("A")]
    );
}

/// **A disconnect fault is a loss the policy answers**, once.
#[test]
fn a_disconnect_fault_asks_for_a_restart_once() {
    let (driver, stream) = ManualStreamDriver::new();
    let (mut session, _device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);
    stream.fail(StreamError::DeviceNotAvailable);
    let events = poll(&mut session, base, 10);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::DeviceFault(_)))
    );
    assert_eq!(wants(&events), [DeviceSelector::Default]);
    assert!(
        wants(&poll(&mut session, base, 20)).is_empty(),
        "held by the backoff"
    );
}

/// **A flapping device cannot cause a restart storm** through the session.
#[test]
fn a_flapping_device_is_not_restarted_on_every_change() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut session, _device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        policy(),
        DeviceSelector::named("A"),
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A"], Some("A"));
    poll(&mut session, base, 0);
    let mut asked = 0;
    for k in 0..100u64 {
        if k % 2 == 0 {
            fake.show(&[], None);
        } else {
            fake.show(&["A"], Some("A"));
        }
        asked += wants(&poll(&mut session, base, 100 * (k + 1))).len();
    }
    assert!(asked >= 1, "it did come back");
    assert!(asked <= 6, "{asked} restarts in 10 s of flapping");
}

/// **A restart that fails leaves the old spec and graph rate**, is reported,
/// and the policy asks again only after its backoff.
///
/// Mutation (run): `note_restart` not reporting `RestartFailed` → the event
/// assertion fails; the policy half of a failure is the next test's.
#[test]
fn a_refused_restart_keeps_the_old_spec_and_is_retried_later() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut session, mut device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);
    fake.show(&["A", "B"], Some("B"));
    assert_eq!(wants(&poll(&mut session, base, 100)).len(), 1);

    // The restart is refused (a zero max block), before the stream stops.
    let (driver, _s) = ManualStreamDriver::new();
    let err = session.restart_on(
        &mut device,
        spec_at(96_000.0),
        driver,
        Some(Samples(0)),
        |_| {},
    );
    assert!(err.is_err());
    assert_eq!(
        device.spec().sample_rate,
        SampleRate(48_000.0),
        "old spec kept"
    );
    assert_eq!(device.graph_rate(), SampleRate(48_000.0));
    let events = poll(&mut session, base, 150);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::RestartFailed(_))),
        "{events:?}"
    );
    assert!(wants(&events).is_empty(), "inside the backoff");
    // Still on A while the default is B: asked again once the wait is over.
    assert_eq!(wants(&poll(&mut session, base, 700)).len(), 1);
}

/// **A restart that fails after the stream stopped is not forgotten**: the
/// stream is dead, so when the default is back on the device it was on, the
/// policy asks again (the device it "runs on" is not playing).
///
/// Mutation (run): `note_restart` not telling the policy of the failure →
/// the policy still believes the stream plays on A, and nothing is asked.
#[test]
fn a_restart_that_stopped_the_stream_is_retried() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut session, mut device) = Session::builder()
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);
    fake.show(&["A", "B"], Some("B"));
    assert_eq!(wants(&poll(&mut session, base, 100)).len(), 1);

    // This device-free session has no card to resolve: the restart stops the
    // stream and then fails, leaving the old spec.
    let err = session.restart(&mut device, DeviceRestart::default());
    assert!(err.is_err());
    assert!(!device.is_running(), "stopped");
    assert_eq!(
        device.spec().sample_rate,
        SampleRate(48_000.0),
        "old spec kept"
    );
    assert_eq!(device.graph_rate(), SampleRate(48_000.0));
    let events = poll(&mut session, base, 150);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::RestartFailed(_))),
        "{events:?}"
    );

    // The default goes back to A, which the dead stream was on.
    fake.show(&["A", "B"], Some("A"));
    assert_eq!(wants(&poll(&mut session, base, 700)).len(), 1);
}
