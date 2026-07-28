//! Sample plumbing under the vocoder: one ring, two views of it.
//!
//! [`SampleFifo`] is the input side (push a block, pop a frame's worth);
//! [`OverlapAdd`] is the output side (accumulate overlapping synthesis frames,
//! drain what has fully summed). Neither knows what a spectrum is.

/// The wrap arithmetic both rings below share.
///
/// A power-of-two capacity plus two monotonic cursors. The cursors never wrap,
/// so "how much is unconsumed" is a plain subtraction and cannot be confused
/// with the empty case; only the *index* wraps, via [`mask`](Self::mask) —
/// which is why the capacity is forced to a power of two rather than merely
/// being one in practice.
///
/// Deliberately not a public container. It is the shared half of two rings with
/// genuinely different interfaces ([`SampleFifo`], [`OverlapAdd`]), and exposing
/// the union of their operations is what let the original code index one ring
/// with the other's cursor.
pub(super) struct Ring {
    pub(super) data: Vec<f32>,
    pub(super) write: usize,
    pub(super) read: usize,
}

impl Ring {
    /// `capacity` is rounded up to a power of two so wrapping is a mask.
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity.next_power_of_two()],
            write: 0,
            read: 0,
        }
    }

    #[inline]
    pub(super) fn mask(&self, i: usize) -> usize {
        i & (self.data.len() - 1)
    }

    /// Samples written but not yet consumed, **capped at the capacity**.
    ///
    /// The cap is the load-bearing part. The cursors are monotonic so the raw
    /// subtraction keeps counting past the ring end, and a reader that trusted it
    /// would hand back slots overwritten laps ago as though they were fresh — a
    /// plausible-sounding wrong answer rather than a detectable failure. That is
    /// exactly how the vocoder's input-rate bug stayed hidden: `available()`
    /// reported 79,231 pending in a 4,096-sample ring and every caller believed
    /// it.
    ///
    /// Capping does not *fix* an overrun — the data is already gone. It bounds
    /// the damage to "the oldest samples were dropped" instead of "the stream is
    /// silently interleaved with stale laps", and it makes
    /// [`Ring::overrun`](Self::overrun) meaningful.
    #[inline]
    pub(super) fn available(&self) -> usize {
        self.write.saturating_sub(self.read).min(self.data.len())
    }

    /// Whether more has been written than the ring can hold — i.e. unread
    /// samples were overwritten. Always a bug in the *caller's* rate matching,
    /// never something the ring can recover from, so it is exposed for tests to
    /// assert against rather than handled here.
    #[cfg(test)]
    #[inline]
    pub(super) fn overrun(&self) -> bool {
        self.write.saturating_sub(self.read) > self.data.len()
    }

    pub(super) fn reset(&mut self) {
        self.data.fill(0.0);
        self.write = 0;
        self.read = 0;
    }
}

/// The vocoder's input side: a plain FIFO.
///
/// Samples arrive in blocks of whatever size the host hands down, and are
/// consumed a window at a time with the read cursor advancing by one *hop* —
/// so a frame is read four times over at 75% overlap. That is why this reads
/// through [`peek`](Self::peek) and advances separately, rather than popping:
/// the data outlives any single read.
pub(super) struct SampleFifo(pub(super) Ring);

impl SampleFifo {
    pub(super) fn new(capacity: usize) -> Self {
        Self(Ring::new(capacity))
    }

    #[inline]
    pub(super) fn available(&self) -> usize {
        self.0.available()
    }

    #[inline]
    pub(super) fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            let i = self.0.mask(self.0.write);
            self.0.data[i] = s;
            self.0.write += 1;
        }
    }

    /// Sample at `offset` past the read cursor, without consuming it.
    #[inline]
    pub(super) fn peek(&self, offset: usize) -> f32 {
        self.0.data[self.0.mask(self.0.read + offset)]
    }

    /// Consume `count` samples. Separate from [`peek`](Self::peek) because one
    /// frame is read `window / hop` times before being retired.
    #[inline]
    pub(super) fn consume(&mut self, count: usize) {
        self.0.read += count;
    }

    pub(super) fn reset(&mut self) {
        self.0.reset();
    }
}

/// The vocoder's output side: an overlap-add accumulator.
///
/// Not a FIFO, which is why it is its own type. Synthesis *sums into* a whole
/// window starting at the write cursor — overlapping the tails of the previous
/// three frames — and only then advances by one synthesis hop. So writes land
/// **ahead** of the cursor and are revisited by later frames, while reads drain
/// behind it. A FIFO's `push` cannot express that.
pub(super) struct OverlapAdd(pub(super) Ring);

impl OverlapAdd {
    pub(super) fn new(capacity: usize) -> Self {
        Self(Ring::new(capacity))
    }

    #[inline]
    pub(super) fn available(&self) -> usize {
        self.0.available()
    }

    /// See [`Ring::overrun`]. On this side an overrun means the caller fed the
    /// vocoder input slower than it is publishing output.
    #[cfg(test)]
    #[inline]
    pub(super) fn overrun(&self) -> bool {
        self.0.overrun()
    }

    /// Sum `value` into the slot `offset` past the write cursor.
    ///
    /// Flushes subnormals: this accumulator is IIR-like, so on a silent tail it
    /// decays into the subnormal range where x86 FPUs trap into microcode and
    /// spike. Snapping to zero never changes audible output — subnormals are
    /// below ~1.2e-38, far under the noise floor of 32-bit audio.
    #[inline]
    pub(super) fn add_at(&mut self, offset: usize, value: f32) {
        let i = self.0.mask(self.0.write + offset);
        let sum = self.0.data[i] + value;
        self.0.data[i] = if sum.is_subnormal() { 0.0 } else { sum };
    }

    /// Zero the slot `offset` past the write cursor, before a later frame sums
    /// into it. Without this the ring would accumulate stale audio from a
    /// previous lap.
    #[inline]
    pub(super) fn clear_at(&mut self, offset: usize) {
        let i = self.0.mask(self.0.write + offset);
        self.0.data[i] = 0.0;
    }

    /// Advance the write cursor by one synthesis hop, publishing that many
    /// finished samples to the reader.
    #[inline]
    pub(super) fn advance(&mut self, hop: usize) {
        self.0.write += hop;
    }

    /// Drain up to `out.len()` finished samples. Returns how many were
    /// available; the rest of `out` is left untouched.
    #[inline]
    pub(super) fn drain(&mut self, out: &mut [f32]) -> usize {
        let count = out.len().min(self.available());
        for (i, slot) in out.iter_mut().take(count).enumerate() {
            *slot = self.0.data[self.0.mask(self.0.read + i)];
        }
        self.0.read += count;
        count
    }

    pub(super) fn reset(&mut self) {
        self.0.reset();
    }
}
