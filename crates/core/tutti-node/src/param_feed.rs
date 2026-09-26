//! [`ParamFeed`]: per-frame values for an `AudioUnit`'s modulatable params,
//! handed in by the native graph's compiler-owned modulation.
//!
//! Design doc 013, "Rewrite order" item 6. The graph owns param modulation:
//! a node declares which of its params are modulatable, and the compiler
//! fuses each one's sources (`base + Σ shaped offsets`, clamped) into a
//! per-frame slice the node reads. A native `tutti_graph::Node` reads that
//! slice from its `Io`. An `AudioUnit` has no `Io`: its `process` takes
//! buffers and nothing else, and before this bridge the only way to hand it a
//! per-frame param was an extra **input channel**, fixed when it was built
//! (the old `with_param_inputs` / `mod_*` flags), fed by a sub-graph of base,
//! shaper and sum nodes.
//!
//! This is the minimum an `AudioUnit` needs instead: a buffer per
//! modulatable param that the `Legacy` adapter fills before each 64-frame
//! call, and a *live* bit per param saying whether it did. A param that is
//! not live this call is read from the unit's own control cell, exactly as
//! if nothing modulated it, so an unmodulated param costs one branch per
//! call — and the unit's arity never changes, so modulation can be connected
//! and disconnected by a commit without rebuilding the unit.
//!
//! It goes with `Legacy` (doc 013 Phase 5): a node ported natively reads its
//! params from `Io` and needs none of this.

use tutti_types::UnitParam;

use crate::MAX_BUFFER_SIZE;

/// Most params one [`ParamFeed`] can carry — the width of its live mask. The
/// graph's own bound on declared param ports is no larger.
pub const MAX_FED_PARAMS: usize = 8;

/// A unit's per-frame param buffers, and which of them are live for the
/// next `process` call. See the module docs (`src/param_feed.rs`).
///
/// A unit embeds one, built from the params it can read per frame, and
/// answers [`AudioUnit::param_feed`](crate::AudioUnit::param_feed) and
/// [`AudioUnit::param_base`](crate::AudioUnit::param_base) for it. In
/// `process` it asks [`get`](Self::get) for each param: `None` means read
/// the control cell (the fast path, unchanged); `Some(frames)` means use
/// these values, one per frame of the call.
#[derive(Clone, Debug)]
pub struct ParamFeed {
    params: &'static [UnitParam],
    live: u8,
    frames: Vec<[f32; MAX_BUFFER_SIZE]>,
}

impl ParamFeed {
    /// A feed for `params`, in port order: the order the graph declares
    /// them in the unit's shape, and the index every other method takes.
    /// Nothing is live. Allocates the buffers, so build it with the unit,
    /// off the audio thread.
    ///
    /// # Panics
    ///
    /// If `params` holds more than [`MAX_FED_PARAMS`], or one param twice.
    pub fn new(params: &'static [UnitParam]) -> Self {
        assert!(
            params.len() <= MAX_FED_PARAMS,
            "{} params past the feed's {MAX_FED_PARAMS}",
            params.len()
        );
        for (i, p) in params.iter().enumerate() {
            assert!(!params[..i].contains(p), "{p:?} is declared twice");
        }
        Self {
            params,
            live: 0,
            frames: vec![[0.0; MAX_BUFFER_SIZE]; params.len()],
        }
    }

    /// The params this feed carries, in port order.
    pub fn params(&self) -> &'static [UnitParam] {
        self.params
    }

    /// Param `k`'s values for the next call's first `size` frames, if it is
    /// live; `None` when the unit reads its own control instead.
    #[inline]
    pub fn get(&self, k: usize, size: usize) -> Option<&[f32]> {
        (self.live >> k & 1 == 1).then(|| &self.frames[k][..size])
    }

    /// Whether any param is live — the one test an unmodulated unit pays.
    #[inline]
    pub fn any_live(&self) -> bool {
        self.live != 0
    }

    /// Make param `k` live with `values` (at most [`MAX_BUFFER_SIZE`] of
    /// them): what the unit reads for the next call's first `values.len()`
    /// frames. The `Legacy` adapter's side.
    ///
    /// # Panics
    ///
    /// If `k` is not a param of this feed, or `values` is longer than a call.
    #[inline]
    pub fn feed(&mut self, k: usize, values: &[f32]) {
        self.frames[k][..values.len()].copy_from_slice(values);
        self.live |= 1 << k;
    }

    /// Make param `k` not live: the unit reads its own control for it.
    #[inline]
    pub fn clear(&mut self, k: usize) {
        self.live &= !(1 << k);
    }

    /// Make every param not live.
    #[inline]
    pub fn clear_all(&mut self) {
        self.live = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A param reads as live exactly between `feed` and `clear`, and only its
    /// own values.
    ///
    /// Mutation: `feed` setting bit 0 whatever `k` → param 1 never goes live
    /// and param 0 does → fails.
    #[test]
    fn a_param_is_live_between_feed_and_clear() {
        static P: [UnitParam; 2] = [UnitParam::Cutoff, UnitParam::Q];
        let mut f = ParamFeed::new(&P);
        assert!(!f.any_live());
        assert_eq!(f.get(1, 4), None);
        f.feed(1, &[1.0, 2.0, 3.0, 4.0]);
        assert!(f.any_live());
        assert_eq!(f.get(0, 4), None, "only the fed param is live");
        assert_eq!(f.get(1, 3), Some(&[1.0, 2.0, 3.0][..]));
        f.clear(1);
        assert_eq!(f.get(1, 4), None);
        assert!(!f.any_live());
    }

    /// Declaring a param twice is refused: two ports for one param would
    /// leave which one the unit reads to its match order.
    #[test]
    #[should_panic(expected = "declared twice")]
    fn a_param_declared_twice_is_refused() {
        static P: [UnitParam; 2] = [UnitParam::Drive, UnitParam::Drive];
        let _ = ParamFeed::new(&P);
    }
}
