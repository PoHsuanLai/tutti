//! A restart re-rates the MIDI clock master, through a session on
//! `ManualStreamDriver`s (the session half of `bevy-tutti`'s
//! `engine::restart` test `a_restart_re_rates_the_midi_clock`, moved here when
//! the adapter was parked).

use std::sync::{Arc, Mutex};

use tutti_cpal::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_fixtures::midi;
use tutti_graph::UmpAt;
use tutti_midi_runtime::MidiOut;
use tutti_session::{Session, UmpOut};
use tutti_transport::MotionEvent;
use tutti_types::{ChannelLayout, SampleRate};
use tutti_ump::{Message, MidiGroup, System, Ump, decode};

const OLD: f64 = 44_100.0;
const NEW: f64 = 48_000.0;

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate(rate),
        ChannelLayout::STEREO,
        tutti_cpal::cpal::SampleFormat::F32,
    )
}

/// Every batch the hardware out sent to its output.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Vec<UmpAt>>>>);

impl MidiOut for Wire {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.0.lock().unwrap().push(events.to_vec());
        events.len()
    }
}

/// Render `blocks` blocks of `frames` on `stream`, then update once: the
/// hardware out hands `wire` the events the clock sent, as one batch whose
/// offsets are their frames' spacing (stamping is off).
fn clock(
    s: &mut Session,
    wire: &Wire,
    stream: &ManualStream,
    (frames, blocks): (usize, usize),
) -> Vec<(Ump, u64)> {
    wire.0.lock().unwrap().clear();
    for _ in 0..blocks {
        stream.render_block(frames).expect("the stream is open");
    }
    let _ = s.update();
    let batches = std::mem::take(&mut *wire.0.lock().unwrap());
    assert_eq!(batches.len(), 1, "one drain, one batch");
    batches[0]
        .iter()
        .map(|&(at, ump)| (ump, u64::from(at.get())))
        .collect()
}

/// **A restart re-rates the MIDI clock master.** At 120 BPM a 24-PPQN tick is
/// 1/48 s: 918.75 frames at 44.1 kHz, 1 000 at 48 kHz. Through the engine's
/// own clock node, the ticks are 918.75 frames apart before the restart and
/// 1 000 after it, and the restart sends no Song Position (it is not a
/// locate: 512-frame blocks move the beat further at 44.1 kHz than at 48,
/// which a seek check with a fixed epsilon would take for a locate).
///
/// The hardware out is the tap on that clock, and it follows the restart: its
/// frames are on the new rate from the hook on, and what it carries is the
/// ticks' spacing on the new grid.
#[test]
fn a_restart_re_rates_the_midi_clock() {
    let (driver, old) = ManualStreamDriver::new();
    let (mut s, mut device) = Session::builder()
        .build_device_on(spec_at(OLD), driver)
        .expect("builds with no device")
        .into_parts();
    let _ = s.update();
    let wire = Wire::default();
    s.midi()
        .set_hardware_out(Some(UmpOut::new(Box::new(wire.clone()), SampleRate(OLD))));
    assert_eq!(s.midi().hardware_out_rate(), Some(SampleRate(OLD)));
    s.midi_nodes()
        .expect("the engine's MIDI nodes")
        .clock_master
        .set_enabled(true);
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let ticks = |events: &[(Ump, u64)]| -> f64 {
        let at: Vec<u64> = events
            .iter()
            .filter(|(ump, _)| *ump == midi::timing_clock(MidiGroup::FIRST))
            .map(|&(_, at)| at)
            .collect();
        assert!(at.len() > 20, "ticks to measure");
        (at[at.len() - 1] - at[0]) as f64 / (at.len() - 1) as f64
    };

    let before = clock(&mut s, &wire, &old, (512, 100));
    assert!((ticks(&before) - 918.75).abs() < 0.05, "{}", ticks(&before));

    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(NEW), driver, None, |_| {})
        .expect("restarts");
    assert_eq!(
        s.midi().hardware_out_rate(),
        Some(SampleRate(NEW)),
        "the hardware out's frames follow the graph to its new rate"
    );
    let _ = s.update(); // what the old rate left in the tap
    let after = clock(&mut s, &wire, &new, (512, 100));
    assert!((ticks(&after) - 1_000.0).abs() < 0.05, "{}", ticks(&after));
    assert!(
        !after.iter().any(|(ump, _)| matches!(
            decode(*ump),
            Ok(Message::System(_, System::SongPosition(_)))
        )),
        "the restart is not a locate"
    );
}
