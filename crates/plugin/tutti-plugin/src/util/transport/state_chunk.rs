//! Splitting and reassembling a plugin state across control-socket frames.
//!
//! State is the one message whose size a *plugin* chooses rather than the
//! protocol, and it does not fit the frame cap: a sample-embedding instrument
//! or a convolution reverb carrying an impulse response routinely exceeds
//! 64 MiB. Sending it as one frame forced a choice between a cap that silently
//! loses presets and a cap so large it re-opens the allocation hazard for every
//! other message. Chunking removes the choice — each frame stays small, and the
//! *total* is bounded separately on reassembly.
//!
//! Both directions use the same two helpers, because the hazard is symmetric:
//! the host reassembles what a plugin produced, and the server reassembles what
//! a project file supplied. Neither end may trust the other's sequencing.

use crate::protocol::{MAX_STATE_BYTES, STATE_CHUNK_BYTES};

/// Split `data` into wire-sized pieces, as `(seq, last, bytes)`.
///
/// **An empty state yields one empty chunk, not zero chunks.** A receiver waits
/// for `last`, so a zero-chunk sequence would never terminate — the reader
/// would sit until its deadline and report a timeout for a plugin that simply
/// had nothing to save. This is the case a `chunks()` call alone gets wrong,
/// since `[].chunks(n)` yields nothing.
pub fn split(data: &[u8]) -> impl Iterator<Item = (u32, bool, &[u8])> {
    let total = data.len().div_ceil(STATE_CHUNK_BYTES).max(1);
    (0..total).map(move |i| {
        let start = i * STATE_CHUNK_BYTES;
        let end = ((i + 1) * STATE_CHUNK_BYTES).min(data.len());
        // `start > end` only for the empty-input case, where `total` was forced
        // to 1 above and the slice must be empty rather than a panic.
        let bytes = if start >= data.len() {
            &data[0..0]
        } else {
            &data[start..end]
        };
        (i as u32, i + 1 == total, bytes)
    })
}

/// Accumulates a chunk sequence, refusing a malformed or oversized one.
///
/// Stateful rather than a fold over a collected `Vec` because the whole point
/// is to stop *before* the bytes are all in memory: collecting first and
/// checking after would allocate exactly what the limit exists to prevent.
#[derive(Debug)]
pub struct Reassembler {
    buf: Vec<u8>,
    next_seq: u32,
    done: bool,
    /// Ceiling on the reassembled total. A field rather than a direct read of
    /// [`MAX_STATE_BYTES`] so the refusal path is reachable from a test without
    /// allocating a gigabyte to reach it — a limit that can only be exercised
    /// by exhausting it is a limit that goes untested.
    limit: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::with_limit(MAX_STATE_BYTES)
    }
}

/// Why a chunk sequence was refused.
///
/// Separate from `StateError` because this layer does not know which direction
/// it is serving; the caller maps it to the right typed error for its side.
#[derive(Debug, PartialEq, Eq)]
pub enum ChunkError {
    /// A chunk arrived out of order, repeated, or after `last`. The sequence is
    /// not what the sender believes, and reassembling anyway would hand the
    /// plugin a corrupt blob to parse.
    OutOfOrder {
        /// The `seq` that arrived.
        got: u32,
        /// The `seq` that was required.
        expected: u32,
    },
    /// The accumulated total exceeded [`MAX_STATE_BYTES`].
    TooLarge {
        /// Total after this chunk — what the state would have been.
        bytes: usize,
        /// The limit it exceeded.
        limit: usize,
    },
}

impl Reassembler {
    /// A reassembler bounded by `limit` rather than [`MAX_STATE_BYTES`].
    ///
    /// Production uses [`Default`]; this exists so the over-limit path is
    /// testable at a size a test can actually build.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            next_seq: 0,
            done: false,
            limit,
        }
    }

    /// Take one chunk. `Ok(true)` means the sequence is complete.
    ///
    /// The size check runs against the *projected* total before extending, so
    /// an over-limit sequence is refused without ever allocating the bytes that
    /// would have broken it.
    pub fn push(&mut self, seq: u32, last: bool, bytes: &[u8]) -> Result<bool, ChunkError> {
        if self.done || seq != self.next_seq {
            return Err(ChunkError::OutOfOrder {
                got: seq,
                expected: self.next_seq,
            });
        }
        let projected = self.buf.len().saturating_add(bytes.len());
        if projected > self.limit {
            return Err(ChunkError::TooLarge {
                bytes: projected,
                limit: self.limit,
            });
        }
        self.buf.extend_from_slice(bytes);
        self.next_seq += 1;
        self.done = last;
        Ok(last)
    }

    /// The reassembled state. Only meaningful once [`push`](Self::push)
    /// returned `Ok(true)`.
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty state must still terminate its sequence.
    ///
    /// The regression this guards is a hang, not a wrong value: `[].chunks(n)`
    /// yields nothing, so a receiver waiting for `last` would wait out its
    /// deadline and report a timeout for a plugin that merely had no state.
    #[test]
    fn an_empty_state_is_one_terminating_chunk() {
        let chunks: Vec<_> = split(&[]).collect();
        assert_eq!(
            chunks.len(),
            1,
            "an empty state produced {} chunks; a receiver waits for `last`, so \
             zero chunks is a hang rather than an empty answer",
            chunks.len()
        );
        assert_eq!(chunks[0].0, 0, "first chunk must be seq 0");
        assert!(
            chunks[0].1,
            "the only chunk of an empty state must be `last`"
        );
        assert!(chunks[0].2.is_empty(), "an empty state must carry no bytes");
    }

    /// Split then reassemble is the identity, across the chunk boundary.
    ///
    /// Sizes straddle `STATE_CHUNK_BYTES` deliberately: an off-by-one in
    /// `div_ceil` or in the `end` clamp shows up only at an exact multiple or
    /// one past it, and a single mid-range size would miss both.
    #[test]
    fn split_then_reassemble_round_trips() {
        for len in [
            0,
            1,
            STATE_CHUNK_BYTES - 1,
            STATE_CHUNK_BYTES,
            STATE_CHUNK_BYTES + 1,
            STATE_CHUNK_BYTES * 2 + 7,
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut r = Reassembler::default();
            let mut finished = false;
            for (seq, last, bytes) in split(&data) {
                assert!(!finished, "chunks kept coming after `last` at len {len}");
                finished = r.push(seq, last, bytes).expect("well-formed sequence");
            }
            assert!(finished, "sequence for len {len} never set `last`");
            assert_eq!(
                r.into_inner(),
                data,
                "round trip changed the bytes at len {len}"
            );
        }
    }

    /// A gap in the sequence is refused rather than silently closed.
    #[test]
    fn a_skipped_chunk_is_refused() {
        let mut r = Reassembler::default();
        r.push(0, false, b"a").expect("first chunk");
        assert_eq!(
            r.push(2, true, b"c"),
            Err(ChunkError::OutOfOrder {
                got: 2,
                expected: 1
            }),
            "a skipped chunk was accepted — the plugin would receive a blob \
             with a hole in it and try to parse it"
        );
    }

    /// A chunk after `last` is refused.
    ///
    /// Distinct from a gap: the sequence numbers are contiguous, so only the
    /// `done` latch catches it. Without that latch a peer could append to a
    /// state the receiver had already considered complete.
    #[test]
    fn a_chunk_after_last_is_refused() {
        let mut r = Reassembler::default();
        assert!(r.push(0, true, b"a").expect("first chunk"));
        assert!(
            matches!(r.push(1, true, b"b"), Err(ChunkError::OutOfOrder { .. })),
            "a chunk after `last` was accepted, extending a completed state"
        );
    }

    /// The limit is enforced on the projected total, before the bytes land.
    ///
    /// Refusal must happen on the chunk that *would* cross the line, not after
    /// it has been appended — the whole point of checking a projection is that
    /// the over-limit allocation never happens. So the assertion is on the
    /// buffer as much as on the error: an implementation that extends first and
    /// checks after returns the same `Err` while having already allocated.
    #[test]
    fn an_oversized_sequence_is_refused_before_it_is_allocated() {
        let mut r = Reassembler::with_limit(10);
        assert!(!r.push(0, false, &[0u8; 8]).expect("under the limit"));
        assert_eq!(
            r.push(1, true, &[0u8; 3]),
            Err(ChunkError::TooLarge {
                bytes: 11,
                limit: 10
            }),
            "a sequence totalling 11 bytes was accepted under a 10-byte limit"
        );
        assert_eq!(
            r.buf.len(),
            8,
            "the refused chunk was appended anyway — the check runs after the \
             extend, so the allocation the limit exists to prevent already \
             happened"
        );
    }

    /// A chunk that fits exactly is accepted.
    ///
    /// Pairs with the test above so the comparison is pinned in both
    /// directions: `>` and `>=` both reject the oversized case, and only this
    /// catches the one that also rejects a legitimate state of exactly the
    /// maximum size.
    #[test]
    fn a_sequence_of_exactly_the_limit_is_accepted() {
        let mut r = Reassembler::with_limit(10);
        assert!(
            r.push(0, true, &[0u8; 10]).expect("exactly at the limit"),
            "a state of exactly the limit must complete"
        );
        assert_eq!(r.into_inner().len(), 10);
    }
}
