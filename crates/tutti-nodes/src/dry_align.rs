//! [`DryAlign`]: the dry path's delay, shared by the nodes that blend a
//! processed signal with their input.

use tutti_dsp::CircularBuffer;

/// The dry path's alignment delay: holds the input for exactly the wet path's
/// latency, so the blend adds the dry sample that arrived *with* the wet one.
///
/// Without it a node blends the undelayed input against a wet signal a block
/// late, while its declared latency covers the whole output — so after PDC the
/// dry half of any `mix < 1` leads the mix by `latency` samples, and the blend
/// itself comb-filters. The latency is fixed when the node is built or
/// prepared, so the ring is sized then and `process` allocates nothing. A
/// latency of zero needs no ring and passes the sample straight through. It is
/// the latency only, never a pre-delay: pre-delay is part of the wet sound.
#[derive(Clone, Debug)]
pub(crate) struct DryAlign {
    ring: Option<CircularBuffer<f32>>,
}

impl DryAlign {
    /// Allocates (a ring of `latency` samples, none for zero) — construction
    /// and `prepare` only. A latency of 1 or more never meets the one-slot clamp
    /// in [`CircularBuffer::new`], which would lengthen the delay.
    pub(crate) fn new(latency: usize) -> Self {
        Self {
            ring: (latency > 0).then(|| CircularBuffer::new(latency)),
        }
    }

    /// Whether the delay is the identity.
    pub(crate) fn is_pass_through(&self) -> bool {
        self.ring.is_none()
    }

    /// Push `input`, return the sample pushed `latency` calls ago.
    ///
    /// The ring holds exactly `latency` samples, so the oldest one — read
    /// before the push overwrites its slot — is `latency` pushes old.
    #[inline]
    pub(crate) fn step(&mut self, input: f32) -> f32 {
        match &mut self.ring {
            Some(ring) => ring.replace_oldest(input),
            None => input,
        }
    }

    pub(crate) fn clear(&mut self) {
        if let Some(ring) = &mut self.ring {
            ring.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A delay of `n` returns each sample `n` pushes later, and a delay of zero
    /// returns it at once.
    ///
    /// Mutation (run): sizing the ring at `latency + 1` returns it one push late
    /// and fails the `n = 3` row; dropping the `latency > 0` test builds a
    /// one-slot ring for zero and fails the first.
    #[test]
    fn it_delays_by_the_latency_it_was_built_for() {
        for n in [0usize, 1, 3] {
            let mut a = DryAlign::new(n);
            let out: Vec<f32> = (1..=6).map(|i| a.step(i as f32)).collect();
            let want: Vec<f32> = (1..=6)
                .map(|i| if i > n { (i - n) as f32 } else { 0.0 })
                .collect();
            let want = if n == 0 {
                (1..=6).map(|i| i as f32).collect()
            } else {
                want
            };
            assert_eq!(out, want, "latency {n}");
        }
    }
}
