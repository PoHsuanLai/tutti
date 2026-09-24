//! The audio arena, and the disjoint borrow that hands a node its buffers —
//! without `unsafe`.
//!
//! One `Vec` of 64-byte [`Line`]s holds every slot; a slot is `stride` lines,
//! so every channel a node sees starts on a cache line and its length is
//! padded to a SIMD multiple (doc 013 §3 step 5, "one aligned `Vec<f32>` arena
//! per port kind").
//!
//! # Why no `unsafe`
//!
//! A node needs `&[f32]` inputs and `&mut [f32]` outputs **from the same
//! arena at once**. The obvious spelling is raw pointers plus a proof that the
//! colouring kept them disjoint — new non-FFI `unsafe`, which
//! `docs/design/012-unsafe-policy.md` asks to be avoided when a safe construct
//! will do. One will: [`borrow_sorted`] walks the requested slots in order
//! and peels them off with `split_at_mut`, so the borrow checker, not a
//! comment, proves disjointness. If the colouring were ever wrong and asked
//! for a written slot twice, the result is a panic naming the slot — never
//! aliasing. The reinterpretation of `[Line]` as `[f32]` is `bytemuck`'s
//! checked `Pod` cast.
//!
//! **Node calls pay no sort.** A node op's requests are sorted once, when the
//! plan is compiled (`NodeTables::lower` in `plan.rs`), and the verifier
//! checks each record's requests against its op. The walk still *checks*
//! the order it relies on: a slot behind the cursor is a panic, in every
//! build, never a wrong buffer. Ops whose requests are built per call (event
//! merges) go through [`borrow_disjoint`], which sorts first. The port tables
//! are stack arrays sized by a small set of const buckets (see `exec.rs`), so
//! a two-port node does not initialise 128 entries, and the commonest shapes
//! skip the walk entirely (`plan::Form`).

use bytemuck::{Pod, Zeroable};

/// Floats per line: 64 bytes.
pub(crate) const LANES: usize = 16;

/// One cache line of samples. The alignment is the type's, so every slot the
/// arena hands out starts 64-byte aligned.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C, align(64))]
pub(crate) struct Line([f32; LANES]);

/// A role in one borrow request.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Role {
    Read(u8),
    Write(u8),
}

/// Borrow the slots named by `reqs` out of `items` (slot `s` is
/// `items[s * stride .. (s + 1) * stride]`), handing each read to `read` and
/// each write to `write` along with its port index.
///
/// Several reads of one slot share it; a written slot may appear only once.
/// `reqs` is sorted in place, then walked by [`borrow_sorted`].
///
/// # Panics
///
/// If a written slot is requested twice, or read and written together. Either
/// means the plan is unsound; the verifier rejects such plans in debug builds.
pub(crate) fn borrow_disjoint<'a, T>(
    items: &'a mut [T],
    stride: usize,
    reqs: &mut [(u32, Role)],
    read: impl FnMut(u8, &'a [T]),
    write: impl FnMut(u8, &'a mut [T]),
) {
    reqs.sort_unstable();
    borrow_sorted(items, stride, reqs, read, write);
}

/// [`borrow_disjoint`] for requests that are **already sorted** — by the
/// compiler, for node ops.
///
/// # Panics
///
/// As [`borrow_disjoint`], and also if `reqs` is not sorted by slot: a slot
/// behind the cursor fails the `checked_sub` and panics, in every build
/// (a debug build names the order earlier, in a `debug_assert!`). Never a
/// wrong or aliased buffer.
#[inline]
pub(crate) fn borrow_sorted<'a, T>(
    items: &'a mut [T],
    stride: usize,
    reqs: &[(u32, Role)],
    mut read: impl FnMut(u8, &'a [T]),
    mut write: impl FnMut(u8, &'a mut [T]),
) {
    debug_assert!(
        reqs.windows(2).all(|w| w[0] <= w[1]),
        "unsorted borrow requests"
    );
    let mut rest: &'a mut [T] = items;
    let mut base = 0usize;
    let mut i = 0;
    while i < reqs.len() {
        let slot = reqs[i].0 as usize;
        let mut j = i + 1;
        while j < reqs.len() && reqs[j].0 as usize == slot {
            j += 1;
        }
        let tail = std::mem::take(&mut rest);
        // `checked_sub`: an unsorted list must panic here, not wrap to a
        // huge offset that happens to land on a real slot in release.
        let skip = slot.checked_sub(base).expect("borrow requests are sorted");
        let (_, tail) = tail.split_at_mut(skip * stride);
        let (cur, after) = tail.split_at_mut(stride);
        rest = after;
        base = slot + 1;

        let group = &reqs[i..j];
        if let Some(&(_, Role::Write(port))) = group.iter().find(|r| matches!(r.1, Role::Write(_)))
        {
            assert!(
                group.len() == 1,
                "slot {slot} is written and also borrowed elsewhere in one op"
            );
            write(port, cur);
        } else {
            let shared: &'a [T] = cur;
            for &(_, role) in group {
                if let Role::Read(port) = role {
                    read(port, shared);
                }
            }
        }
        i = j;
    }
}

/// The audio arena: `slots × stride` lines.
pub(crate) struct Arena {
    lines: Vec<Line>,
    stride: usize,
}

impl Arena {
    /// An arena of `slots` zeroed slots, each holding at least `max_block`
    /// samples. Control thread.
    pub(crate) fn new(slots: usize, max_block: usize) -> Self {
        let stride = max_block.div_ceil(LANES).max(1);
        Self {
            lines: vec![Line::zeroed(); slots * stride],
            stride,
        }
    }

    /// Slot `s`'s first `frames` samples.
    #[inline]
    pub(crate) fn slot(&self, s: u32, frames: usize) -> &[f32] {
        // `[start..][..stride]` rather than `[start..end]`: two length checks
        // and no `start <= end` one, on the hottest indexing in the executor.
        let lines = &self.lines[s as usize * self.stride..][..self.stride];
        &bytemuck::cast_slice::<Line, f32>(lines)[..frames]
    }

    /// Slot `s`'s first `frames` samples, mutably.
    #[inline]
    pub(crate) fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [f32] {
        let lines = &mut self.lines[s as usize * self.stride..][..self.stride];
        &mut bytemuck::cast_slice_mut::<Line, f32>(lines)[..frames]
    }

    /// `src` for reading and `dst` for writing, at once. They must differ.
    #[inline]
    pub(crate) fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[f32], &mut [f32]) {
        assert_ne!(src, dst, "pair() of one slot");
        let st = self.stride;
        let (lo, hi) = (src.min(dst) as usize, src.max(dst) as usize);
        let (a, b) = self.lines.split_at_mut(hi * st);
        let low = &mut a[lo * st..][..st];
        let high = &mut b[..st];
        let (s, d) = if src < dst { (low, high) } else { (high, low) };
        (
            &bytemuck::cast_slice::<Line, f32>(s)[..frames],
            &mut bytemuck::cast_slice_mut::<Line, f32>(d)[..frames],
        )
    }

    /// Copy slot `src` over slot `dst` (whole stride). Never allocates.
    pub(crate) fn copy_slot(&mut self, src: u32, dst: u32) {
        let st = self.stride;
        self.lines.copy_within(
            src as usize * st..(src as usize + 1) * st,
            dst as usize * st,
        );
    }

    /// Hand the sorted `reqs` to [`borrow_sorted`], as `f32` slices of
    /// `frames`.
    #[inline]
    pub(crate) fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [f32]],
        outs: &mut [&'a mut [f32]],
    ) {
        let stride = self.stride;
        borrow_sorted(
            &mut self.lines,
            stride,
            reqs,
            |port, lines| {
                ins[port as usize] = &bytemuck::cast_slice::<Line, f32>(lines)[..frames];
            },
            |port, lines| {
                outs[port as usize] = &mut bytemuck::cast_slice_mut::<Line, f32>(lines)[..frames];
            },
        );
    }

    /// The arena's samples, as one aligned slice — for tests asserting the
    /// alignment claim.
    #[cfg(test)]
    pub(crate) fn base(&self) -> *const f32 {
        self.lines.as_ptr().cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::MAX_PORTS;

    /// Every slot starts on a 64-byte boundary, whatever `max_block` is.
    ///
    /// Mutation: `#[repr(C, align(64))]` → `#[repr(C)]` on `Line` → the
    /// allocation is only 4-aligned and some slot misses → fails (usually on
    /// the base pointer itself).
    #[test]
    fn slots_are_cache_line_aligned() {
        for max_block in [1, 7, 64, 100, 1000] {
            let mut a = Arena::new(5, max_block);
            assert_eq!(a.base() as usize % 64, 0);
            for s in 0..5 {
                assert_eq!(a.slot_mut(s, 1).as_ptr() as usize % 64, 0, "slot {s}");
                assert!(a.stride * LANES >= max_block);
            }
        }
    }

    /// Reads share, writes are exclusive, and the right port gets the right
    /// slot.
    ///
    /// Mutation: in `borrow_sorted`, drop the `base = slot + 1` update → the
    /// second slot's offset is computed from 0 again → wrong slot → fails.
    #[test]
    fn borrow_hands_each_port_its_slot() {
        let mut a = Arena::new(6, 16);
        for s in 0..6u32 {
            a.slot_mut(s, 16).fill(s as f32);
        }
        let mut reqs = [
            (4, Role::Read(0)),
            (1, Role::Read(1)),
            (4, Role::Read(2)),
            (3, Role::Write(0)),
            (5, Role::Write(1)),
        ];
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        // As the compiler hands them over (`NodeTables::lower`).
        reqs.sort_unstable();
        a.borrow(16, &reqs, &mut ins, &mut outs);
        assert_eq!(ins[0][0], 4.0);
        assert_eq!(ins[1][0], 1.0);
        assert_eq!(ins[2][0], 4.0);
        assert_eq!(outs[0][0], 3.0);
        assert_eq!(outs[1][0], 5.0);
        outs[0][0] = 9.0;
        assert_eq!(a.slot(3, 1)[0], 9.0);
    }

    /// A slot requested for writing and reading in one op is refused, never
    /// aliased.
    ///
    /// Mutation: delete the `assert!` in `borrow_sorted` → the read is
    /// silently dropped and no panic happens → fails.
    #[test]
    #[should_panic(expected = "written and also borrowed")]
    fn borrow_refuses_a_write_that_aliases() {
        let mut a = Arena::new(3, 16);
        let reqs = [(2, Role::Read(0)), (2, Role::Write(0))];
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        a.borrow(16, &reqs, &mut ins, &mut outs);
    }

    /// Requests the compiler failed to sort are a panic, never a slot behind
    /// the cursor handed out as the wrong buffer.
    ///
    /// Mutation: in `borrow_sorted`, drop the `debug_assert!` and replace the
    /// `checked_sub(..).expect(..)` with a plain `slot - base` → the walk
    /// dies on an arithmetic overflow whose message does not name the order
    /// → fails.
    #[test]
    #[should_panic(expected = "sorted")]
    fn borrow_refuses_unsorted_requests() {
        let mut a = Arena::new(4, 16);
        let reqs = [(3, Role::Write(0)), (1, Role::Read(0))];
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        a.borrow(16, &reqs, &mut ins, &mut outs);
    }
}
