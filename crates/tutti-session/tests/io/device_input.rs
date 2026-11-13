//! The inputs a session adds, and what a restart does to them: reopened at
//! the new rate into the same node, with a failure reported typed. Device-free:
//! the session's inputs open through a test's own capture
//! (`SessionBuilder::input_opener`) and its output on a `ManualStreamDriver`.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tutti_cpal::{ManualStream, ManualStreamDriver, MicConfig, OutputSpec, StreamLatency};
use tutti_graph::{InletRing, TapWriter};
use tutti_session::{
    DeviceEvent, Error, InputConfig, InputOpener, LiveSession, OpenedInput, Phase, PhaseCx, Root,
    Session, SessionEvent,
};
use tutti_types::{ChannelLayout, SampleRate, Samples};

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(rate),
        ChannelLayout::STEREO,
        tutti_cpal::cpal::SampleFormat::F32,
    )
}

/// What the test's capture did, in order, and the handles it gave out.
#[derive(Default)]
struct Record {
    /// `open(rate)`, `closed` (a stream's owner dropped), in order.
    log: Vec<String>,
    /// The writer of each ring opened: the "capture callback" of each.
    writers: Vec<TapWriter>,
    /// The configs the opens were asked for.
    configs: Vec<MicConfig>,
    /// The latency cell the opens were handed.
    latency: Option<Arc<StreamLatency>>,
    /// Opens from this one on fail (`None`: none does).
    fail_from: Option<usize>,
}

/// Dropping it is the capture stream closing.
struct Stream(Arc<Mutex<Record>>);

impl Drop for Stream {
    fn drop(&mut self) {
        self.0.lock().unwrap().log.push("closed".into());
    }
}

#[derive(Debug)]
struct Capture(Arc<Mutex<Record>>);

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Record")
    }
}

impl InputOpener for Capture {
    fn open(&mut self, config: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput> {
        let mut rec = self.0.lock().unwrap();
        let n = rec.writers.len();
        rec.log.push(format!("open {}", rate.get()));
        if rec.fail_from.is_some_and(|from| n >= from) {
            return Err(tutti_cpal::Error::InvalidDevice("unplugged".into()));
        }
        let (ring, writer) = InletRing::pair(ChannelLayout::STEREO, Samples(256), rate);
        rec.writers.push(writer);
        rec.configs.push(config.clone());
        rec.latency = config.latency.clone();
        Ok(OpenedInput {
            ring,
            keep: Box::new(Stream(Arc::clone(&self.0))) as Box<dyn Any + Send>,
            lead: Samples(0),
        })
    }
}

/// A live session at 48 kHz whose inputs open through the test's capture.
fn live(record: &Arc<Mutex<Record>>) -> (LiveSession, ManualStream) {
    let (driver, stream) = ManualStreamDriver::new();
    let live = Session::builder()
        .input_opener(Capture(Arc::clone(record)))
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    (live, stream)
}

fn poll(session: &mut Session) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    session.run_phase(
        Phase::Poll,
        PhaseCx::at(std::time::Instant::now()),
        &mut out,
    );
    out
}

/// One block rendered, its first stereo frame.
fn first_frame(session: &mut Session, stream: &ManualStream) -> [f32; 2] {
    let _ = session.update();
    let block = stream.render_block(16).expect("the stream is open");
    [block[0], block[1]]
}

/// **A restart onto another rate reopens an added input at that rate into the
/// same node.** The input is monitored (wired to the root); after a restart from
/// 48 kHz to 44.1 kHz the capture was closed first, opened again at 44.1 kHz,
/// and what its new callback pushes is heard through the node the host holds,
/// which was never rewired. The old ring is no longer read.
#[test]
fn a_restart_reopens_an_added_input_into_the_same_node() {
    let record = Arc::new(Mutex::new(Record::default()));
    let (live, stream) = live(&record);
    let (mut session, mut device) = live.into_parts();

    let input = session
        .add_input(InputConfig::default().channels(&[0, 1]))
        .expect("opens");
    session.graph().connect(input.node(), Root).unwrap().hard();
    // The wire as the host declared it; the render below needs it in force.
    let wired = session.graph_ref().sources(Root::output(0)).unwrap();
    assert_eq!(wired.len(), 1, "monitored");

    record.lock().unwrap().writers[0].push_interleaved(&[0.25, -0.25].repeat(16));
    assert_eq!(first_frame(&mut session, &stream), [0.25, -0.25]);

    let (driver, new) = ManualStreamDriver::new();
    session
        .restart_on(&mut device, spec_at(44_100.0), driver, None)
        .expect("restarts");
    assert_eq!(
        record.lock().unwrap().log,
        ["open 48000", "closed", "open 44100"],
        "the old capture closed before the new one opened, at the new rate"
    );
    assert_eq!(
        record.lock().unwrap().configs[1].inputs,
        Some(vec![0, 1]),
        "the same config"
    );

    // The same node, the same wire: nothing was done to the graph but the swap.
    assert_eq!(session.graph_ref().sources(Root::output(0)).unwrap(), wired);
    {
        let mut rec = record.lock().unwrap();
        rec.writers[0].push_interleaved(&[0.9, 0.9].repeat(16));
        rec.writers[1].push_interleaved(&[0.5, -0.5].repeat(16));
    }
    // The swap crossfades in over 10 ms; give it its blocks.
    let mut last = [0.0; 2];
    for _ in 0..64 {
        last = first_frame(&mut session, &new);
        record.lock().unwrap().writers[1].push_interleaved(&[0.5, -0.5].repeat(16));
    }
    assert_eq!(
        last,
        [0.5, -0.5],
        "the new capture is heard, the old is not"
    );
    let _ = input;
}

/// **An input the restart cannot reopen is reported, typed, and the restart
/// still succeeds.** The device is gone at the second open: the next update
/// carries `DeviceEvent::InputReopenFailed` with the capture's own error.
#[test]
fn an_input_that_cannot_be_reopened_is_reported_with_its_error() {
    let record = Arc::new(Mutex::new(Record::default()));
    let (live, _stream) = live(&record);
    let (mut session, mut device) = live.into_parts();
    let input = session.add_input(InputConfig::default()).expect("opens");
    record.lock().unwrap().fail_from = Some(1);

    let (driver, _new) = ManualStreamDriver::new();
    session
        .restart_on(&mut device, spec_at(44_100.0), driver, None)
        .expect("the output restarts whatever the input does");

    let events = poll(&mut session);
    let failed = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::Device(DeviceEvent::InputReopenFailed { input, error }) => {
                Some((input, error))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no InputReopenFailed in {events:?}"));
    assert_eq!(failed.0.node(), input.node());
    assert!(
        matches!(failed.1, Error::Device(tutti_cpal::Error::InvalidDevice(why)) if why == "unplugged"),
        "{:?}",
        failed.1
    );
}

/// **A restart that fails reaches `update()` as a device event whose source is
/// the typed error**, not a string: here the graph refuses a block past the
/// engine's capacity, and the event's error chain ends at that `CommitError`.
#[test]
fn a_failed_restart_is_a_device_event_with_the_typed_error_as_its_source() {
    let record = Arc::new(Mutex::new(Record::default()));
    let (live, _stream) = live(&record);
    let (mut session, mut device) = live.into_parts();
    let (driver, _new) = ManualStreamDriver::new();
    let refused = session.restart_on(
        &mut device,
        spec_at(96_000.0),
        driver,
        Some(tutti_types::MaxBlock::of(Samples(16_384))),
    );
    assert!(refused.is_err());

    let events = poll(&mut session);
    let Some(SessionEvent::Device(DeviceEvent::RestartFailed(e))) = events
        .iter()
        .find(|e| matches!(e, SessionEvent::Device(DeviceEvent::RestartFailed(_))))
    else {
        panic!("no RestartFailed in {events:?}");
    };
    let source = std::error::Error::source(e).expect("the event has a source");
    let session_error = source
        .downcast_ref::<Error>()
        .expect("the source is the session's Error");
    assert!(
        matches!(
            session_error,
            Error::Reprepare(tutti_graph::CommitError::BlockTooLong { .. })
        ),
        "{session_error:?}"
    );
}

/// **`Device::latency` is the stream's own timestamps in frames at the graph's
/// rate**: output 10 ms at 48 kHz is 480 frames and input 5 ms is 240, each
/// `None` before its stream has reported (not zero), and after a restart to
/// 44.1 kHz both are `None` again until the new streams report, 10 ms then
/// being 441 frames.
#[test]
fn device_latency_is_the_reported_delay_in_frames_and_none_before_a_report() {
    let record = Arc::new(Mutex::new(Record::default()));
    let (live, stream) = live(&record);
    let (mut session, mut device) = live.into_parts();
    let _input = session.add_input(InputConfig::default()).expect("opens");

    let none = device.latency();
    assert_eq!((none.input, none.output), (None, None), "nothing reported");

    stream
        .render_block_delayed(64, Duration::from_millis(10))
        .expect("open");
    let cell = record
        .lock()
        .unwrap()
        .latency
        .clone()
        .expect("handed a cell");
    cell.record(Some(Duration::from_millis(5)));
    let l = device.latency();
    assert_eq!(l.output, Some(Samples(480)));
    assert_eq!(l.input, Some(Samples(240)));

    let (driver, new) = ManualStreamDriver::new();
    session
        .restart_on(&mut device, spec_at(44_100.0), driver, None)
        .expect("restarts");
    let after = device.latency();
    assert_eq!(
        (after.input, after.output),
        (None, None),
        "the new streams have reported nothing"
    );
    new.render_block_delayed(64, Duration::from_millis(10))
        .expect("open");
    assert_eq!(device.latency().output, Some(Samples(441)));
}
