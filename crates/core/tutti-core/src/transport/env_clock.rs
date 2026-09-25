//! The beat signal of a native graph: [`EnvClock`], a `tutti-graph` node
//! that emits on [`BEAT_PORTS`](super::BEAT_PORTS) what a
//! [`TransportClock`](super::TransportClock) emits in a `Net`, computed from
//! the block's [`Env`](tutti_graph::Env) instead of the transport's atomics.
//!
//! Doc 013, Phase 3 gap 5: a graph engine drives its own `TransportClock`
//! and forbids a second one in the graph ([`Engine::with_graph`]), yet
//! `ClickNode`, the LFO and automation beat inputs, and every host that
//! wires a node to the engine's clock read the beat as a signal. This is
//! that signal on the graph backend, and it touches nothing shared: no seek
//! to consume, no playhead to write back, so any number of them can sit in
//! one graph.
//!
//! [`Engine::with_graph`]: crate::Engine::with_graph

use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, Latency, Tail};

use super::clock::split_beat;
use super::state::{beats_per_sample, LoopRange, BEAT_PORTS};

/// The beat generator of a native graph: no inputs, [`BEAT_PORTS`] outputs
/// (whole beats, then the fraction), read from each block's
/// [`Env`](tutti_graph::Env).
///
/// **The same samples as [`TransportClock`](super::TransportClock).** Per
/// piece of the block ([`Env::segments`](tutti_graph::Env::segments), cut at
/// each transport change), it starts from the piece's beat — the host's own
/// figure, which on the graph engine *is* its driven `TransportClock`'s
/// position — and walks it frame by frame with that clock's arithmetic:
/// emit, then add [`beats_per_sample`] of the piece's tempo, wrapped by
/// [`LoopRange::advance`] (so a loop armed behind the playhead does not
/// jump), held while stopped. The split is the clock's own `split_beat`. So
/// on the graph engine the ports carry, bit for bit, what a `TransportClock`
/// in a `Net` carries under the same transport — a seek, a loop wrap, a
/// start or stop, a tempo step on its frame — and a consumer written against
/// [`beat_from_ports`](super::beat_from_ports) needs no change.
///
/// It does **not** use [`Env::transport_at`](tutti_graph::Env::transport_at):
/// that is a closed form, which agrees with an accumulated clock to rounding,
/// not to the bit. The `f32` split at the ports usually rounds the
/// difference away; accumulating makes the ports equal by construction.
///
/// **Arrival.** A node reads the transport at its compiled arrival
/// ([`Cx::arrival`]). This one has no inputs, so its arrival is zero by
/// construction (the compiler's arrival is the latest departure of a node's
/// predecessors, and it has none): it emits the beat of the block's own
/// frames. A consumer behind a latent path is aligned by the compiler, which
/// delays the clock's edge into it like any other source.
///
/// Never skipped: a generator with [`Tail::Unbounded`].
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvClock;

impl EnvClock {
    /// A beat generator.
    pub const fn new() -> Self {
        Self
    }
}

impl Node for EnvClock {
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::EMPTY,
            ChannelLayout::from_count(BEAT_PORTS as u16),
        )
        .with_tail(Tail::Unbounded)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let env = cx.env;
        for (start, piece) in env.segments() {
            let t = piece.transport;
            let from = start.index();
            let to = from + piece.block_len.get();
            let mut beat = t.beat;
            if !t.playing {
                let (whole, frac) = split_beat(beat);
                io.output(0)[from..to].fill(whole);
                io.output(1)[from..to].fill(frac);
                continue;
            }
            let step = beats_per_sample(t.tempo, piece.sample_rate);
            let region = t.looping.and_then(|l| LoopRange::new(l.start, l.end));
            for i in from..to {
                let (whole, frac) = split_beat(beat);
                io.output(0)[i] = whole;
                io.output(1)[i] = frac;
                beat = match region {
                    Some(r) => r.advance(beat, beat + step),
                    None => beat + step,
                };
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
