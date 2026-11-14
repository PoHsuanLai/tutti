//! What the runtime's integration tests share: a test sink for MIDI leaving a
//! graph (one event tap per source port), a wire between two event nodes, and the
//! clock of a clip at 120 BPM and 48 kHz.

#![allow(dead_code)]

use tutti_fixtures::midi;
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, EventTapId, EventTapReader, NodeKey, TapAlign, TapEvent,
    TransportPosition,
};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_types::{Beat, Bpm};
use tutti_ump::{MidiChannel, Ump};

/// Frames in a beat at 120 BPM and 48 kHz, so a frame is exact in beats.
pub const FRAMES_PER_BEAT: f64 = 24_000.0;

/// The beat `frame` frames in.
pub fn beat_of(frame: u64) -> Beat {
    Beat(frame as f64 / FRAMES_PER_BEAT)
}

/// Rolling at 120 BPM from `frame`.
pub fn rolling(frame: u64) -> TransportPosition {
    TransportPosition::new(true, Bpm::of(120.0), beat_of(frame), None)
}

/// `event` on `frame`.
pub fn at(frame: u64, event: Ump) -> TimedMidiEvent {
    TimedMidiEvent::new(beat_of(frame), event).unwrap()
}

/// A full-velocity Note On of key `n` on the first Group and Channel.
pub fn on(n: u8) -> Ump {
    midi::note_on(MidiChannel::FIRST, n, 0xFFFF)
}

/// A Note Off of key `n` on the first Group and Channel.
pub fn off(n: u8) -> Ump {
    midi::note_off(MidiChannel::FIRST, n, 0)
}

/// Wires event output 0 of node `from` to event input 0 of node `to`.
pub fn connect(ed: &mut Editor, from: u64, to: u64) {
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(to),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(from),
            port: 0,
        }),
    );
}

/// The ring's capacity the deleted node had, in events.
pub const SINK_CAPACITY: tutti_graph::EventCapacity = tutti_graph::EventCapacity::of(1024);

/// The readers of the taps [`sink`] opened.
pub struct Sink {
    taps: Vec<EventTapReader>,
    lost: u64,
}

/// Opens a raw tap of [`SINK_CAPACITY`] on each `(node, port)` of `sources`,
/// ids `first_id` upward, and returns their readers as one sink. Every source
/// node is in `ed` already; commit after.
pub fn sink(ed: &mut Editor, first_id: u32, sources: &[(u64, u16)]) -> Sink {
    let taps = sources
        .iter()
        .zip(first_id..)
        .map(|(&(node, port), id)| {
            ed.open_event_tap(
                EventTapId(id),
                EventOut {
                    node: NodeKey(node),
                    port,
                },
                TapAlign::Raw,
                SINK_CAPACITY,
            )
            .expect("the source is in the graph")
        })
        .collect();
    Sink { taps, lost: 0 }
}

impl Sink {
    /// Pops up to `out.len()` events, tap by tap, oldest first within a tap;
    /// each stamp is the event's frame on the graph's clock, truncated to
    /// 32 bits as the node's was. Allocates nothing.
    pub fn poll_into(&mut self, out: &mut [(u32, Ump)]) -> usize {
        let mut n = 0;
        let mut chunk = [TapEvent {
            frame: tutti_types::Frame::ZERO,
            ump: Ump([0; 4]),
        }; 64];
        for tap in &mut self.taps {
            while n < out.len() {
                let want = chunk.len().min(out.len() - n);
                let got = tap.pop_into(&mut chunk[..want]);
                for e in &chunk[..got] {
                    out[n] = (e.frame.get() as u32, e.ump);
                    n += 1;
                }
                if got < want {
                    break;
                }
            }
        }
        n
    }

    /// Events the taps refused for want of room, since the last call summed
    /// into the running total.
    pub fn dropped(&mut self) -> u64 {
        self.lost += self.taps.iter().map(EventTapReader::take_lost).sum::<u64>();
        self.lost
    }
}
