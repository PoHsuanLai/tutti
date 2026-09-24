//! Buffer colouring over the **partial** order (doc 013 §3 step 5).
//!
//! Serial liveness colouring — "a slot is free once its last reader in the
//! serial order has run" (JUCE) — is wrong the moment two ops can run at once:
//! the serial order says reader `r` ran before writer `w`, but a parallel
//! executor may run them together, and `w` clobbers what `r` is reading.
//!
//! The rule here is stated over the op DAG instead. A value occupies its slot
//! from its writer to its last reader. Value `u` may hand its slot to a later
//! value `v` only if **every op touching `u` happens-before `v`'s writer** —
//! reachability, not position — so no schedule consistent with the DAG can
//! overlap them. Reachability is a bitset per op, which is `n²/64` words:
//! about 16 k for a thousand ops, well inside what a control-thread recompile
//! can afford.
//!
//! **In-place aliasing** is the one exception: an op may write `v` into `u`'s
//! slot when it is itself a reader of `u` and every *other* reader of `u`
//! happens-before it. The op reads before it writes, so nothing is lost —
//! which is why it is offered only to ops that promise to (PDC delay rings,
//! and nodes whose `Shape::in_place` is set).

use std::collections::BTreeMap;

use super::Val;

/// Strict reachability over the op DAG.
pub(crate) struct Reach {
    words: usize,
    bits: Vec<u64>,
}

impl Reach {
    /// `succ[i]` are op `i`'s successors; every edge must point forward.
    pub(crate) fn new(succ: &[Vec<u32>]) -> Self {
        let n = succ.len();
        let words = n.div_ceil(64);
        let mut bits = vec![0u64; n * words];
        for a in (0..n).rev() {
            for &b in &succ[a] {
                let b = b as usize;
                debug_assert!(b > a, "op DAG edges point forward");
                bits[a * words + b / 64] |= 1 << (b % 64);
                for w in 0..words {
                    let from_b = bits[b * words + w];
                    bits[a * words + w] |= from_b;
                }
            }
        }
        Self { words, bits }
    }

    /// Whether `a` happens-before `b` (strictly: `a != b` and a path exists).
    pub(crate) fn before(&self, a: u32, b: u32) -> bool {
        let (a, b) = (a as usize, b as usize);
        (self.bits[a * self.words + b / 64] >> (b % 64)) & 1 == 1
    }
}

/// A colouring: slot per value, and how many slots.
pub(crate) struct Colouring {
    pub(crate) slot: Vec<u32>,
    pub(crate) count: u32,
}

/// Whether every op touching `u` happens-before `w`.
fn finished_before(u: &Val, w: u32, reach: &Reach) -> bool {
    reach.before(u.writer, w) && u.readers.iter().all(|&r| reach.before(r, w))
}

/// Greedy colouring in writer order. `in_place[v] = u` offers `v` the slot of
/// `u`, taken when legal.
pub(crate) fn colour(vals: &[Val], reach: &Reach, in_place: &BTreeMap<u32, u32>) -> Colouring {
    let mut slot = vec![u32::MAX; vals.len()];
    // Last occupant of each slot. Values are visited in writer order, and a
    // value can only follow an occupant whose every op happens-before its
    // writer — so the occupants of one slot form a chain, and checking the
    // last one checks them all.
    let mut last: Vec<u32> = Vec::new();

    for (v, val) in vals.iter().enumerate() {
        debug_assert!(
            v == 0 || vals[v - 1].writer <= val.writer,
            "values are created in writer order"
        );
        let w = val.writer;

        if let Some(&u) = in_place.get(&(v as u32)) {
            let src = &vals[u as usize];
            let s = slot[u as usize];
            let legal = last[s as usize] == u
                && src.readers.contains(&w)
                && src.readers.iter().all(|&r| r == w || reach.before(r, w));
            if legal {
                slot[v] = s;
                last[s as usize] = v as u32;
                continue;
            }
        }

        let reuse = last
            .iter()
            .position(|&occ| finished_before(&vals[occ as usize], w, reach));
        let s = match reuse {
            Some(s) => s,
            None => {
                last.push(0);
                last.len() - 1
            }
        };
        slot[v] = s as u32;
        last[s] = v as u32;
    }

    Colouring {
        count: last.len() as u32,
        slot,
    }
}
