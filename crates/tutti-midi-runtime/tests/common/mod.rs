//! A test sink for MIDI leaving a graph: one event tap per source port,
//! read like the ring of the node it replaces (doc 017, stage E3:
//! the hardware out is a tap, so a node that only copied events out is gone).

#![allow(dead_code)]

use tutti_graph::{Editor, EventOut, EventTapId, EventTapReader, NodeKey, TapAlign, TapEvent};
use tutti_ump::Ump;

/// The ring's capacity the deleted node had, in events.
pub const SINK_CAPACITY: usize = 1024;

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
