//! The hardware out is sent at the time each event's frame is heard, not when
//! the host happens to drain it: a session on `ManualStreamDriver`s, whose
//! callbacks report the audio output's latency as they are told to.
//!
//! Every figure is arithmetic on the MIDI spec's tick (24 per quarter note,
//! 1/48 s at 120 BPM, so 1 000 frames at 48 kHz) and on `Instant`s the test
//! reads around its own calls.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_cpal::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_graph::UmpAt;
use tutti_midi_runtime::{MidiOut, Outgoing, OutputLatency, OutputOffset, SendAt};
use tutti_session::{Session, UmpOut};
use tutti_transport::MotionEvent;
use tutti_types::{ChannelLayout, SampleRate, Seconds};
use tutti_ump::{Message, MidiGroup, System, Ump};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

/// Every event the output was told to send, with when.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Outgoing>>>);

impl MidiOut for Wire {
    fn queue(&self, events: &[UmpAt]) -> usize {
        events.len()
    }

    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        self.0.lock().unwrap().extend_from_slice(events);
        events.len()
    }
}

impl Wire {
    fn take(&self) -> Vec<Outgoing> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// The send times of the ticks among `events`, in order; panics on a tick
    /// sent `Now`.
    fn tick_times(events: &[Outgoing]) -> Vec<Instant> {
        events
            .iter()
            .filter(|e| e.ump == Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock)))
            .map(|e| match e.send {
                SendAt::At(at) => at,
                SendAt::Now => panic!("a tick was sent now"),
            })
            .collect()
    }
}

fn spec() -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(RATE),
        ChannelLayout::STEREO,
        tutti_cpal::cpal::SampleFormat::F32,
    )
}

/// A session on a manual stream with its clock running into `wire`; the device
/// is returned because the stream stops when it drops.
fn rig(wire: &Wire) -> (Session, ManualStream, tutti_session::Device) {
    let (driver, stream) = ManualStreamDriver::new();
    let (mut s, device) = Session::builder()
        .build_device_on(spec(), driver)
        .expect("builds with no device")
        .into_parts();
    let _ = s.update();
    s.midi().set_hardware_out(Some(UmpOut::new(
        Box::new(wire.clone()),
        SampleRate::of(RATE),
    )));
    s.midi_nodes().unwrap().clock_master.set_enabled(true);
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    (s, stream, device)
}

/// Renders `blocks` blocks reporting `delay` as the audio output's latency, the
/// last one after reading `Instant::now()` into the return.
fn render(stream: &ManualStream, blocks: usize, delay: Option<Duration>) -> Instant {
    let one = || match delay {
        Some(delay) => stream.render_block_delayed(BLOCK, delay),
        None => stream.render_block(BLOCK),
    };
    for _ in 1..blocks {
        one().expect("the stream is open");
    }
    let last = Instant::now();
    one().expect("the stream is open");
    last
}

/// **Events reach the output at the instants their frames are heard, spaced by
/// the frames.** With the stream reporting 1.5 s of audio latency and a second
/// of clock rendered, every tick is in the future, and ticks 1 000 frames apart
/// are 20.833 ms apart in `SendAt::At`, not all `Now`.
#[test]
fn the_clock_is_sent_at_the_time_of_its_frames() {
    let wire = Wire::default();
    let (mut s, stream, _device) = rig(&wire);
    let latency = Duration::from_millis(1500);
    let last_block = render(&stream, 100, Some(latency));
    let _ = s.update();
    let sent = wire.take();
    let ticks = Wire::tick_times(&sent);
    assert!(ticks.len() > 20, "ticks to measure: {}", ticks.len());
    for pair in ticks.windows(2) {
        let gap = pair[1].duration_since(pair[0]).as_secs_f64();
        assert!((gap - 1_000.0 / RATE).abs() < 1e-5, "ticks {gap} s apart");
    }
    // The last block began at `last_block` (within the call's own few ms), and
    // every frame of it is heard its offset after, plus the latency.
    let newest = *ticks.last().unwrap();
    let expected = last_block + latency;
    assert!(
        newest >= expected && newest <= expected + Duration::from_millis(11 + 5),
        "the newest tick is {:?} after the last block began, plus the latency",
        newest.checked_duration_since(last_block)
    );
}

/// **An output's offset shifts every event by it.** The same clock in two
/// sessions, one with a +0.25 s offset: the newest tick leaves 250 ms later
/// relative to the instant its block began, which the test read the same way
/// for both.
#[test]
fn an_output_offset_shifts_the_send_times() {
    let latency = Some(Duration::from_millis(1500));
    let newest_after_block = |offset: f64| {
        let wire = Wire::default();
        let (mut s, stream, _device) = rig(&wire);
        s.midi().set_output_latency(
            OutputLatency::new(Duration::ZERO, OutputOffset::new(Seconds(offset)).unwrap())
                .unwrap(),
        );
        let last_block = render(&stream, 100, latency);
        let _ = s.update();
        let ticks = Wire::tick_times(&wire.take());
        ticks
            .last()
            .unwrap()
            .duration_since(last_block)
            .as_secs_f64()
    };
    let shift = newest_after_block(0.25) - newest_after_block(0.0);
    assert!((shift - 0.25).abs() < 0.02, "shifted by {shift} s");
}

/// **While the audio latency is unreported, events are timed with the terms
/// known, and the terms say so.** Before the stream's first callback and after
/// one that reports nothing, `output_terms` is incomplete and ticks leave at
/// the instant their frames are rendered (none a second out); once a callback
/// reports 40 ms the term is in.
#[test]
fn an_unreported_audio_latency_is_left_out_and_reported() {
    let wire = Wire::default();
    let (mut s, stream, _device) = rig(&wire);
    let terms = s.midi().output_terms().expect("a device session has terms");
    assert_eq!(terms.audio_output, None);
    assert!(!terms.is_complete());

    let last_block = render(&stream, 100, None);
    let _ = s.update();
    let sent = wire.take();
    assert!(!s.midi().output_terms().unwrap().is_complete());
    let scheduled: Vec<Instant> = sent
        .iter()
        .filter_map(|e| match e.send {
            SendAt::At(at) => Some(at),
            SendAt::Now => None,
        })
        .collect();
    assert!(!scheduled.is_empty(), "the newest block's events are ahead");
    assert!(
        scheduled
            .iter()
            .all(|at| *at <= last_block + Duration::from_millis(40)),
        "with no audio latency known, nothing is scheduled beyond its own frame"
    );

    let last_block = render(&stream, 2, Some(Duration::from_millis(40)));
    let terms = s.midi().output_terms().unwrap();
    assert_eq!(terms.audio_output, Some(Duration::from_millis(40)));
    assert!(terms.is_complete());
    let _ = s.update();
    let scheduled: Vec<Instant> = wire
        .take()
        .iter()
        .filter_map(|e| match e.send {
            SendAt::At(at) => Some(at),
            SendAt::Now => None,
        })
        .collect();
    assert!(
        scheduled
            .iter()
            .any(|at| *at >= last_block + Duration::from_millis(40)),
        "the reported 40 ms is in the send time"
    );
}
