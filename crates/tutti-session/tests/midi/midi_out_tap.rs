//! The session's hardware MIDI out is an event tap on the clock node's output
//! (doc 017, stage E3), drained by `Phase::MidiOut` into the output a host set:
//! what reaches the wire, in what order and on which frames, what a full tap
//! drops, and the tap following the graph to another rate.
//!
//! Every figure is the MIDI spec's: a 24-PPQN tick at 120 BPM is 1/48 s, so
//! 1 000 frames at 48 kHz.

use std::sync::{Arc, Mutex};
use tutti_types::MaxBlock;

use tutti_fixtures::midi;
use tutti_graph::{TapAlign, UmpAt};
use tutti_midi_runtime::MidiOut;
use tutti_transport::MotionEvent;
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_ump::{MidiGroup, Ump};

use tutti_session::{Session, SessionEvent, UmpOut};

const RATE: f64 = 48_000.0;

/// Every batch the output was handed, in order.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Vec<UmpAt>>>>);

impl MidiOut for Wire {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.0.lock().unwrap().push(events.to_vec());
        events.len()
    }
}

impl Wire {
    fn batches(&self) -> Vec<Vec<UmpAt>> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// A session with its clock enabled and a stamping-off output on `wire`.
fn rig(wire: &Wire) -> Session {
    let mut s = Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let _ = s.update();
    s.midi_nodes().unwrap().clock_master.set_enabled(true);
    s.midi()
        .set_hardware_out(Some(UmpOut::new(Box::new(wire.clone()), SampleRate(RATE))));
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    s
}

fn render(s: &Session, frames: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..frames.div_ceil(512) {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    }
}

fn ticks(batch: &[UmpAt]) -> Vec<u32> {
    batch
        .iter()
        .filter(|(_, ump)| *ump == midi::timing_clock(MidiGroup::FIRST))
        .map(|(at, _)| at.get())
        .collect()
}

/// **The clock's events reach the output in order, 1 000 frames apart.** Half a
/// second at 120 BPM, then one update: the drain hands the output one batch
/// whose offsets, in order, are the frames' spacing: Start first, then every
/// 24-PPQN tick exactly one tick (1/48 s, 1 000 frames) after the last.
#[test]
fn the_clock_reaches_the_output_in_order_on_its_frames() {
    let wire = Wire::default();
    let mut s = rig(&wire);
    render(&s, 24_000);
    let _ = s.update();
    let batches = wire.batches();
    assert_eq!(batches.len(), 1, "one drain, one batch");
    let batch = &batches[0];
    assert!(
        batch.windows(2).all(|w| w[0].0.get() <= w[1].0.get()),
        "in frame order"
    );
    let ticks = ticks(batch);
    assert!(ticks.len() >= 24, "half a second of ticks: {}", ticks.len());
    assert!(
        ticks.windows(2).all(|w| w[1] - w[0] == 1_000),
        "{ticks:?}: a tick every 1/48 s"
    );
}

/// **A tap that fills drops what it cannot hold and says so.** The tap holds
/// 1 024 events between two updates; the clock is left to send about 1 100
/// ticks with no update. The output then gets exactly 1 024 (the tap's
/// capacity, a non-note-off being refused past it) and the drop count is the
/// remainder, so what was sent is what arrived plus what was counted: the
/// total is read from a second tap with room.
#[test]
fn a_full_tap_counts_what_it_drops() {
    let wire = Wire::default();
    let mut s = rig(&wire);
    let clock = s.midi_nodes().unwrap().clock;
    let mut all = s
        .graph()
        .open_event_tap(clock.into(), TapAlign::Raw, 8_192)
        .expect("the clock is in the graph");
    let _ = s.update(); // commits the second tap
    render(&s, 1_100_000);
    let mut total = 0;
    let mut chunk = [tutti_graph::TapEvent {
        frame: tutti_types::Frame::ZERO,
        ump: Ump([0; 4]),
    }; 256];
    loop {
        let n = all.pop_into(&mut chunk);
        total += n;
        if n < chunk.len() {
            break;
        }
    }
    assert_eq!(all.take_lost(), 0, "the second tap had room");
    assert!(
        total > 1_024,
        "the clock sent more than a tap holds: {total}"
    );

    let events = s.update();
    let delivered: usize = wire.batches().iter().map(Vec::len).sum();
    assert_eq!(delivered, 1_024, "the tap's capacity reached the output");
    let dropped = s.midi().out_drops();
    assert_eq!(dropped as usize, total - 1_024, "the rest is counted");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::MidiOutDropped(n) if *n == dropped)),
        "and reported: {events:?}"
    );
}

/// **A graph re-rated outside a device restart is followed at the next
/// update.** `Graph::set_sample_rate` moves the graph's clock to another scale;
/// the hardware out's reader, on the rate it was opened at, is reopened by the
/// `MidiOut` phase, so its frames are on one scale again.
#[test]
fn a_re_rated_graph_reopens_the_hardware_tap_at_the_next_update() {
    let wire = Wire::default();
    let mut s = rig(&wire);
    assert_eq!(s.midi().hardware_out_rate(), Some(SampleRate(RATE)));
    s.graph().set_sample_rate(SampleRate(44_100.0));
    let _ = s.update();
    assert_eq!(s.midi().hardware_out_rate(), Some(SampleRate(44_100.0)));
}

/// **Reopening one tap supersedes that tap's reader and no other's.** Two taps
/// of a caller's on the clock; after the graph is re-rated, `reopen_event_tap`
/// of the first gives a reader on the new rate, and the second's reader is
/// still fed (`reopen_event_taps` would have replaced it).
#[test]
fn reopening_one_tap_leaves_the_others_fed() {
    let wire = Wire::default();
    let mut s = rig(&wire);
    let clock = s.midi_nodes().unwrap().clock;
    let mut first = s
        .graph()
        .open_event_tap(clock.into(), TapAlign::Raw, 64)
        .unwrap();
    let mut second = s
        .graph()
        .open_event_tap(clock.into(), TapAlign::Raw, 64)
        .unwrap();
    let _ = s.update();
    s.graph().set_sample_rate(SampleRate(44_100.0));
    let reopened = s
        .graph()
        .reopen_event_tap(first.id())
        .expect("the tap is open");
    assert_eq!(reopened.rate(), SampleRate(44_100.0));
    assert_eq!(reopened.id(), first.id());
    first = reopened;
    // The re-prepare's second half lands on a render, then the commit.
    let _ = s.update();
    render(&s, 4_096);
    let _ = s.update();
    render(&s, 4_096);
    let mut chunk = [tutti_graph::TapEvent {
        frame: tutti_types::Frame::ZERO,
        ump: Ump([0; 4]),
    }; 64];
    assert!(first.pop_into(&mut chunk) > 0, "the reopened tap is fed");
    assert!(second.pop_into(&mut chunk) > 0, "the other is still fed");
    assert_eq!(second.rate(), SampleRate(RATE), "and on its own rate");
}
