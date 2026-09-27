//! The beat walk the metronome shares with the graph: every frame's beat,
//! read from its block's [`Env`](tutti_graph::Env).
//!
//! It was also `EnvClock`'s, a node that emitted the walk on two beat ports
//! for nodes that took the beat as a signal. Every such node now reads its
//! block's `Env` (doc 013, "Legacy deleted"), so the node is gone and the
//! walk is `Env::for_each_beat`.

use tutti_types::Beat;

/// The beat of every frame of one piece of a block ([`Env::segments`]'s
/// `(start, piece)`), handed to `emit` with its index in the whole block —
/// [`Env::for_each_piece_beat`](tutti_graph::Env::for_each_piece_beat),
/// the walk the metronome (`ClickNode`, which gates each piece on its
/// transport) takes.
///
/// [`Env::segments`]: tutti_graph::Env::segments
pub(super) fn piece_beats(
    start: tutti_graph::Offset,
    piece: &tutti_graph::Env,
    emit: impl FnMut(usize, Beat),
) {
    piece.for_each_piece_beat(start, emit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bpm, FrameClock, SampleRate, Samples};
    use tutti_graph::{Env, Transport, TransportChanges};
    use tutti_types::Frame;

    const SR: SampleRate = SampleRate(48_000.0);

    /// The beats a block of `len` frames starting on `host`'s frame gets, in
    /// `f64`.
    fn walked(host: &FrameClock, len: usize, looping: Option<tutti_graph::LoopRange>) -> Vec<Beat> {
        let env = Env {
            frame: Frame(0),
            sample_rate: SR,
            block_len: Samples(len),
            transport: Transport::counted(true, host.tempo(), host.origin(), looping),
            changes: TransportChanges::NONE,
        };
        let mut out = vec![Beat(f64::NAN); len];
        env.for_each_beat(|i, b| out[i] = b);
        out
    }

    /// The walk continues the host's clock in `f64`, bit for bit, far into
    /// a segment at a tempo whose frame step is not representable, through
    /// loop wraps shorter than the block.
    ///
    /// Mutation (run): rebuild the clock from the transport's beat alone
    /// (`Transport::new(t.playing, t.tempo, t.beat(), t.looping).clock(..)`,
    /// dropping the origin) → the beats part from the host's in the last
    /// bits → fails. (An `f32` split rounds that away, which is why this
    /// compares the `f64` beats.)
    #[test]
    fn the_env_walk_is_the_host_clock_in_f64() {
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
