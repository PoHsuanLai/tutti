//! The beat signal of a native graph: [`EnvClock`], a `tutti-graph` node
//! that emits on [`BEAT_PORTS`](super::BEAT_PORTS) what a
//! [`TransportClock`](super::TransportClock) emits in a `Net`, computed from
//! the block's [`Env`](tutti_graph::Env) instead of the transport's atomics.
//!
//! Doc 013, Phase 3 gap 5: the engine drives its own `TransportClock`,
//! and there is no second one ([`Engine::new`]: two clocks would both
//! consume a seek and both write the playhead, so a transport hands its
//! playhead-writing links out once, and a second engine over it is
//! refused), yet
//! `ClickNode`, the LFO and automation beat inputs, and every host that
//! wires a node to the engine's clock read the beat as a signal. This is
//! that signal on the native graph, and it touches nothing shared: no seek
//! to consume, no playhead to write back, so any number of them can sit in
//! one graph.
//!
//! [`Engine::new`]: crate::Engine::new

use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_types::{Beat, ChannelLayout, Latency, Samples, Tail};

use super::clock::split_beat;
use super::state::{LoopRange, BEAT_PORTS};

/// The beat generator of a native graph: no inputs, [`BEAT_PORTS`] outputs
/// (whole beats, then the fraction), read from each block's
/// [`Env`](tutti_graph::Env).
///
/// **The same samples as [`TransportClock`](super::TransportClock).** Per
/// piece of the block ([`Env::segments`](tutti_graph::Env::segments), cut at
/// each transport change), it rebuilds the host's clock from the piece's
/// transport — its [origin](tutti_graph::Transport::origin), the frame count
/// the host's beat is derived from, which on the graph engine *is* its
/// driven `TransportClock`'s — and walks it frame by frame with that clock's
/// own code (`FrameClock`): emit, then roll a frame, the beat in closed form
/// from the segment, wrapping on the frame that reaches the loop's end (and
/// not at all for a loop armed behind the playhead), held while stopped. The
/// split is the clock's own `split_beat`. So on the graph engine the ports
/// carry, bit for bit, what a `TransportClock` in a `Net` carries under the
/// same transport — a seek, a loop wrap, a start or stop, a tempo step on
/// its frame — and a consumer written against
/// [`beat_from_ports`](super::beat_from_ports) needs no change.
///
/// It does **not** use [`Env::transport_at`](tutti_graph::Env::transport_at),
/// which wraps a loop by the unwrapped position where the clock starts a new
/// segment on the wrap's frame: the two agree to rounding past a wrap, and
/// the ports must agree to the bit.
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
        beats(cx.env, |i, beat| {
            let (whole, frac) = split_beat(beat);
            io.output(0)[i] = whole;
            io.output(1)[i] = frac;
        });
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// The beat of every frame of `env`'s block, in order, handed to `emit` with
/// its index: what the ports carry before the split. Piece by piece, each
/// walked with the host's clock rebuilt from the piece's transport
/// ([`Transport::clock`](tutti_graph::Transport::clock)).
fn beats(env: &tutti_graph::Env, mut emit: impl FnMut(usize, Beat)) {
    for (start, piece) in env.segments() {
        let t = piece.transport;
        let from = start.index();
        let to = from + piece.block_len.get();
        let mut clock = t.clock(piece.sample_rate);
        debug_assert_eq!(
            clock.beat(),
            t.beat(),
            "the clock a transport describes starts on its beat"
        );
        let region = t.looping.and_then(|l| LoopRange::new(l.start, l.end));
        for i in from..to {
            emit(i, clock.beat());
            if t.playing {
                clock.advance(Samples(1), region);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bpm, FrameClock, SampleRate};
    use tutti_graph::{Env, Transport, TransportChanges};
    use tutti_types::Frame;

    const SR: SampleRate = SampleRate(48_000.0);

    /// The beats a block of `len` frames starting on `host`'s frame gets, in
    /// `f64`, before the `f32` split that would round a last-bit difference
    /// away.
    fn walked(host: &FrameClock, len: usize, looping: Option<tutti_graph::LoopRange>) -> Vec<Beat> {
        let env = Env {
            frame: Frame(0),
            sample_rate: SR,
            block_len: Samples(len),
            transport: Transport::counted(true, host.tempo(), host.origin(), looping),
            changes: TransportChanges::NONE,
        };
        let mut out = vec![Beat(f64::NAN); len];
        beats(&env, |i, b| out[i] = b);
        out
    }

    /// `EnvClock` continues the host's clock in `f64`, bit for bit, far into
    /// a segment at a tempo whose frame step is not representable, through
    /// loop wraps shorter than the block.
    ///
    /// Mutation (run): rebuild the clock from the transport's beat alone
    /// (`Transport::new(t.playing, t.tempo, t.beat(), t.looping).clock(..)`,
    /// dropping the origin) → the beats part from the host's in the last
    /// bits → fails. (The `f32` port split rounds that away, which is why
    /// this compares the `f64` beats.)
    #[test]
    fn env_clock_is_the_host_clock_in_f64() {
        for (tempo, looping) in [
            (97.0, None),
            (133.3, None),
            (97.0, crate::LoopRange::new(1_000.0, 1_000.004)),
        ] {
            let mut host = FrameClock::new(Beat(0.25), Bpm(tempo), SR);
            host.advance(Samples(987_654_321), None);
            if let Some(l) = looping {
                host.seat(Beat(1_000.0));
                host.advance(Samples(3), Some(l));
            }
            let graph_loop = looping.map(|l| tutti_graph::LoopRange {
                start: l.start(),
                end: l.end(),
            });
            let got = walked(&host, 512, graph_loop);
            for (i, b) in got.iter().enumerate() {
                assert_eq!(
                    b.get().to_bits(),
                    host.beat().get().to_bits(),
                    "{tempo} BPM, loop {looping:?}: frame {i}"
                );
                host.advance(Samples(1), looping);
            }
        }
    }
}
