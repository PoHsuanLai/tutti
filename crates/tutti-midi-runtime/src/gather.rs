//! A block's MIDI event input as the `(Offset, Ump)` run a MIDI instrument walks.

use tutti_graph::UmpAt;
use tutti_graph::{EventKind, SortedEvents};
use tutti_ump::Ump;

/// Copies the MIDI events of `events` into `out`, in their (sorted) order, and
/// returns how many it wrote. Stops when `out` is full; non-MIDI events are
/// skipped. Each event keeps its block-relative frame as its [`Offset`](tutti_graph::Offset).
///
/// Writes into existing slots, so it does not allocate.
#[inline]
pub fn gather_midi(events: SortedEvents<'_>, out: &mut [UmpAt]) -> usize {
    let mut n = 0;
    for e in events {
        if n == out.len() {
            break;
        }
        if let EventKind::Midi(Ump(data)) = e.kind {
            out[n] = (e.offset, Ump(data));
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests;
