//! SIMD accelerated audio buffers for block processing.

use super::*;
extern crate alloc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use numeric_array::ArrayLength;

/// Mutably borrowed audio buffer with an arbitrary number of channels
/// containing 64 (`MAX_BUFFER_SIZE`) samples per channel. Samples are stored
/// non-interleaved. Intended as a temporary borrow to feed into
/// `AudioNode::process` or `AudioUnit::process`.
pub struct BufferMut<'a, S: Sample = F32>(&'a mut [S::Simd], PhantomData<S>);

impl<'a, S: Sample> BufferMut<'a, S> {
    /// Create new buffer from a slice. The length of the slice must be divisible by `S::LEN`.
    #[inline]
    pub fn new(buffer: &'a mut [S::Simd]) -> Self {
        debug_assert!(buffer.len() & (S::LEN - 1) == 0);
        Self(buffer, PhantomData)
    }

    /// Create an empty buffer with 0 channels.
    #[inline]
    pub fn empty() -> Self {
        Self(&mut [], PhantomData)
    }

    /// Create new buffer that is a subset of this buffer.
    #[inline]
    pub fn subset(&mut self, first_channel: usize, channels: usize) -> BufferMut<'_, S> {
        debug_assert!(first_channel + channels <= self.channels());
        BufferMut::new(&mut self.0[(first_channel << S::C)..((first_channel + channels) << S::C)])
    }

    /// Convert this buffer into an immutable one.
    #[inline]
    pub fn buffer_ref(&mut self) -> BufferRef<'_, S> {
        BufferRef::new(self.0)
    }

    /// Number of channels in this buffer.
    #[inline]
    pub fn channels(&self) -> usize {
        self.0.len() >> S::C
    }

    /// Get channel as a SIMD slice.
    #[inline]
    pub fn channel(&self, channel: usize) -> &[S::Simd] {
        debug_assert!(channel < self.channels());
        &(self.0)[(channel << S::C)..(channel + 1) << S::C]
    }

    /// Get channel as a mutable SIMD slice.
    #[inline]
    pub fn channel_mut(&mut self, channel: usize) -> &mut [S::Simd] {
        debug_assert!(channel < self.channels());
        &mut (self.0)[(channel << S::C)..(channel + 1) << S::C]
    }

    /// Set SIMD value at index `i` (0 <= `i` < `S::LEN`) of `channel`.
    #[inline]
    pub fn set(&mut self, channel: usize, i: usize, value: S::Simd) {
        debug_assert!(channel < self.channels());
        (self.0)[(channel << S::C) + i] = value;
    }

    /// Set scalar value at sample index `i` (0 <= `i` < 64) of `channel`.
    #[inline]
    pub fn set_scalar(&mut self, channel: usize, i: usize, value: S::Scalar) {
        debug_assert!(channel < self.channels());
        S::set_lane(
            &mut (self.0)[(channel << S::C) + (i >> S::S)],
            i & S::M,
            value,
        );
    }

    /// Get SIMD value at index `i` (0 <= `i` < `S::LEN`) of `channel`.
    #[inline]
    pub fn at(&self, channel: usize, i: usize) -> S::Simd {
        debug_assert!(channel < self.channels());
        (self.0)[(channel << S::C) + i]
    }

    /// Get mutable reference to SIMD value at index `i` of `channel`.
    #[inline]
    pub fn at_mut(&mut self, channel: usize, i: usize) -> &mut S::Simd {
        debug_assert!(channel < self.channels());
        &mut (self.0)[(channel << S::C) + i]
    }

    /// Get scalar value at sample index `i` (0 <= `i` < 64) of `channel`.
    #[inline]
    pub fn at_scalar(&self, channel: usize, i: usize) -> S::Scalar {
        debug_assert!(channel < self.channels());
        S::get_lane(&(self.0)[(channel << S::C) + (i >> S::S)], i & S::M)
    }

    /// Add to SIMD value at index `i` of `channel`.
    pub fn add(&mut self, channel: usize, i: usize, value: S::Simd) {
        debug_assert!(channel < self.channels());
        (self.0)[(channel << S::C) + i] += value;
    }

    /// Create a sub-span of this buffer.
    pub fn span(&self, start: usize, length: usize, target: &mut BufferVec<S>) {
        for channel in 0..self.channels() {
            for i in start..length {
                target.set_scalar(channel, i - start, self.at_scalar(channel, start));
            }
        }
    }
}

// Backward-compatible f32-specific methods on BufferMut<'_, F32>.
impl<'a> BufferMut<'a, F32> {
    /// Get channel as an f32 scalar slice.
    #[inline]
    pub fn channel_f32(&self, channel: usize) -> &'a [f32] {
        debug_assert!(channel < self.channels());
        let data = self.channel(channel).as_ptr() as *const f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts(data, MAX_BUFFER_SIZE) }
    }

    /// Get channel as a mutable f32 scalar slice.
    #[inline]
    pub fn channel_f32_mut(&mut self, channel: usize) -> &'a mut [f32] {
        debug_assert!(channel < self.channels());
        let data = self.channel_mut(channel).as_mut_ptr() as *mut f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts_mut(data, MAX_BUFFER_SIZE) }
    }

    /// Set `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn set_f32(&mut self, channel: usize, i: usize, value: f32) {
        self.set_scalar(channel, i, value);
    }

    /// Get `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn at_f32(&self, channel: usize, i: usize) -> f32 {
        self.at_scalar(channel, i)
    }
}

/// Immutably borrowed audio buffer with an arbitrary number of channels
/// containing 64 (`MAX_BUFFER_SIZE`) samples per channel. Samples are stored non-interleaved.
/// Intended as a temporary borrow to feed into `AudioNode::process` or `AudioUnit::process`.
pub struct BufferRef<'a, S: Sample = F32>(&'a [S::Simd], PhantomData<S>);

impl<'a, S: Sample> BufferRef<'a, S> {
    /// Create new buffer from a slice. The length of the slice must be divisible by `S::LEN`.
    #[inline]
    pub fn new(buffer: &'a [S::Simd]) -> Self {
        debug_assert!(buffer.len() & (S::LEN - 1) == 0);
        Self(buffer, PhantomData)
    }

    /// Create an empty buffer with 0 channels.
    #[inline]
    pub fn empty() -> Self {
        Self(&[], PhantomData)
    }

    /// Create new buffer that is a subset of this buffer.
    #[inline]
    pub fn subset(&self, first_channel: usize, channels: usize) -> BufferRef<'_, S> {
        debug_assert!(first_channel + channels <= self.channels());
        BufferRef::new(&self.0[(first_channel << S::C)..((first_channel + channels) << S::C)])
    }

    /// Number of channels in this buffer.
    #[inline]
    pub fn channels(&self) -> usize {
        self.0.len() >> S::C
    }

    /// Get channel SIMD slice.
    #[inline]
    pub fn channel(&self, channel: usize) -> &[S::Simd] {
        debug_assert!(channel < self.channels());
        &(self.0)[(channel << S::C)..(channel + 1) << S::C]
    }

    /// Access SIMD value at index `i` (0 <= `i` < `S::LEN`).
    #[inline]
    pub fn at(&self, channel: usize, i: usize) -> S::Simd {
        debug_assert!(channel < self.channels());
        (self.0)[(channel << S::C) + i]
    }

    /// Access scalar value at sample index `i` (0 <= `i` < 64) of `channel`.
    #[inline]
    pub fn at_scalar(&self, channel: usize, i: usize) -> S::Scalar {
        debug_assert!(channel < self.channels());
        S::get_lane(&(self.0)[(channel << S::C) + (i >> S::S)], i & S::M)
    }

    /// Create a sub-span of this buffer.
    pub fn span(&self, start: usize, length: usize, target: &mut BufferVec<S>) {
        for channel in 0..self.channels() {
            for i in start..length {
                target.set_scalar(channel, i - start, self.at_scalar(channel, start));
            }
        }
    }
}

// Backward-compatible f32-specific methods on BufferRef<'_, F32>.
impl<'a> BufferRef<'a, F32> {
    /// Get channel as an f32 scalar slice.
    #[inline]
    pub fn channel_f32(&self, channel: usize) -> &'a [f32] {
        debug_assert!(channel < self.channels());
        let data = self.channel(channel).as_ptr() as *const f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts(data, MAX_BUFFER_SIZE) }
    }

    /// Access `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn at_f32(&self, channel: usize, i: usize) -> f32 {
        self.at_scalar(channel, i)
    }
}

/// An owned buffer on the heap with an arbitrary number of channels
/// containing 64 (`MAX_BUFFER_SIZE`) samples per channel. Samples are stored non-interleaved.
#[derive(Clone)]
pub struct BufferVec<S: Sample = F32> {
    buffer: Vec<S::Simd>,
    _marker: PhantomData<S>,
}

impl<S: Sample> Default for BufferVec<S> {
    fn default() -> Self {
        Self {
            buffer: Vec::new(),
            _marker: PhantomData,
        }
    }
}

impl<S: Sample> BufferVec<S> {
    /// Create new owned buffer with the given number of `channels`.
    pub fn new(channels: usize) -> Self {
        let mut buffer = Vec::with_capacity(channels << S::C);
        buffer.resize(channels << S::C, S::simd_zero());
        Self {
            buffer,
            _marker: PhantomData,
        }
    }

    /// Number of channels in this buffer.
    #[inline]
    pub fn channels(&self) -> usize {
        self.buffer.len() >> S::C
    }

    /// Length of the buffer in SIMD elements per channel.
    #[inline]
    pub fn length(&self) -> usize {
        S::LEN
    }

    /// Length of the buffer in SIMD elements per channel.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        S::LEN
    }

    /// Access SIMD value at index `i` (0 <= `i` < `S::LEN`) of `channel`.
    #[inline]
    pub fn at(&self, channel: usize, i: usize) -> S::Simd {
        debug_assert!(channel < self.channels());
        self.buffer[(channel << S::C) + i]
    }

    /// Set SIMD `value` at index `i` (0 <= `i` < `S::LEN`) of `channel`.
    #[inline]
    pub fn set(&mut self, channel: usize, i: usize, value: S::Simd) {
        debug_assert!(channel < self.channels());
        self.buffer[(channel << S::C) + i] = value;
    }

    /// Access scalar value at sample index `i` (0 <= `i` < 64) of `channel`.
    #[inline]
    pub fn at_scalar(&self, channel: usize, i: usize) -> S::Scalar {
        debug_assert!(channel < self.channels());
        S::get_lane(&self.buffer[(channel << S::C) + (i >> S::S)], i & S::M)
    }

    /// Set scalar `value` at sample index `i` (0 <= `i` < 64) of `channel`.
    #[inline]
    pub fn set_scalar(&mut self, channel: usize, i: usize, value: S::Scalar) {
        debug_assert!(channel < self.channels());
        S::set_lane(
            &mut self.buffer[(channel << S::C) + (i >> S::S)],
            i & S::M,
            value,
        );
    }

    /// Get channel SIMD slice.
    #[inline]
    pub fn channel(&self, channel: usize) -> &[S::Simd] {
        debug_assert!(channel < self.channels());
        &self.buffer[(channel << S::C)..(channel + 1) << S::C]
    }

    /// Get mutable channel SIMD slice.
    #[inline]
    pub fn channel_mut(&mut self, channel: usize) -> &mut [S::Simd] {
        debug_assert!(channel < self.channels());
        &mut self.buffer[(channel << S::C)..(channel + 1) << S::C]
    }

    /// Fill all channels of buffer with zeros.
    #[inline]
    pub fn clear(&mut self) {
        self.buffer.fill(S::simd_zero());
    }

    /// Resize the buffer.
    pub fn resize(&mut self, channels: usize) {
        self.buffer.resize(channels << S::C, S::simd_zero());
    }

    /// Get an immutably borrowed buffer.
    #[inline]
    pub fn buffer_ref(&self) -> BufferRef<'_, S> {
        BufferRef::new(&self.buffer)
    }

    /// Get a mutably borrowed buffer.
    #[inline]
    pub fn buffer_mut(&mut self) -> BufferMut<'_, S> {
        BufferMut::new(&mut self.buffer)
    }
}

// Backward-compatible f32-specific methods on BufferVec<F32>.
impl BufferVec<F32> {
    /// Access `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn at_f32(&self, channel: usize, i: usize) -> f32 {
        self.at_scalar(channel, i)
    }

    /// Set `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn set_f32(&mut self, channel: usize, i: usize, value: f32) {
        self.set_scalar(channel, i, value);
    }

    /// Get channel as an f32 scalar slice.
    #[inline]
    pub fn channel_f32(&mut self, channel: usize) -> &[f32] {
        debug_assert!(channel < self.channels());
        let data = self.channel(channel).as_ptr() as *const f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts(data, MAX_BUFFER_SIZE) }
    }

    /// Get channel as a mutable f32 scalar slice.
    #[inline]
    pub fn channel_f32_mut(&mut self, channel: usize) -> &mut [f32] {
        debug_assert!(channel < self.channels());
        let data = self.channel_mut(channel).as_mut_ptr() as *mut f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts_mut(data, MAX_BUFFER_SIZE) }
    }
}

/// An owned audio buffer stored inline as an array with an arbitrary number of channels
/// containing 64 (`MAX_BUFFER_SIZE`) samples per channel.
/// Samples are stored non-interleaved.
/// The number of channels must be known at compile time:
/// the size `N` is given as a type-level integer (`U0`, `U1`, ...).
///
/// Note: `BufferArray` is f32-only because it uses `[F32x; SIMD_LEN]` which
/// requires a const array size derived from the sample format. Generifying this
/// would require `generic_const_exprs` (unstable).
#[repr(C)]
#[derive(Clone, Default)]
pub struct BufferArray<N: ArrayLength> {
    array: Frame<[F32x; SIMD_LEN], N>,
}

impl<N: ArrayLength> BufferArray<N> {
    /// Create new buffer and initialize it with zeros.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create new buffer.
    #[inline]
    pub(crate) fn uninitialized() -> Self {
        // Safety: This is undefined behavior but it seems to work fine. Zero initialization is safe but slower in benchmarks.
        #[allow(clippy::uninit_assumed_init)]
        unsafe {
            core::mem::MaybeUninit::uninit().assume_init()
        }
    }

    /// Access value at index `i` (0 <= `i` <= 7) of `channel`.
    #[inline]
    pub fn at(&self, channel: usize, i: usize) -> F32x {
        self.array[channel][i]
    }

    /// Get `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn at_f32(&self, channel: usize, i: usize) -> f32 {
        debug_assert!(channel < self.channels());
        self.array[channel][i >> SIMD_S].as_array()[i & SIMD_M]
    }

    /// Set value at index `i` (0 <= `i` <= 7) of `channel`.
    #[inline]
    pub fn set(&mut self, channel: usize, i: usize, value: F32x) {
        debug_assert!(channel < self.channels());
        self.array[channel][i] = value;
    }

    /// Get `f32` value at index `i` (0 <= `i` <= 63) of `channel`.
    #[inline]
    pub fn set_f32(&mut self, channel: usize, i: usize, value: f32) {
        debug_assert!(channel < self.channels());
        self.array[channel][i >> SIMD_S].as_mut_array()[i & SIMD_M] = value;
    }

    /// Number of channels in this buffer.
    #[inline]
    pub fn channels(&self) -> usize {
        N::USIZE
    }

    /// Length of the buffer is 8 SIMD samples.
    #[inline]
    pub fn length(&self) -> usize {
        SIMD_LEN
    }

    /// Length of the buffer is 8 SIMD samples.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        SIMD_LEN
    }

    /// Fill all channels of buffer with zeros.
    #[inline]
    pub fn clear(&mut self) {
        self.array.fill([F32x::ZERO; SIMD_LEN]);
    }

    /// Get channel slice.
    #[inline]
    pub fn channel(&self, channel: usize) -> &[F32x] {
        // Safety: we know Frames are contiguous and we know the length statically.
        unsafe {
            &core::slice::from_raw_parts(self.array.as_ptr() as *const F32x, N::USIZE << SIMD_C)
                [(channel << SIMD_C)..(channel + 1) << SIMD_C]
        }
    }

    /// Get mutable channel slice.
    #[inline]
    pub fn channel_mut(&mut self, channel: usize) -> &mut [F32x] {
        // Safety: we know Frames are contiguous and we know the length statically.
        unsafe {
            &mut core::slice::from_raw_parts_mut(
                self.array.as_mut_ptr() as *mut F32x,
                N::USIZE << SIMD_C,
            )[(channel << SIMD_C)..(channel + 1) << SIMD_C]
        }
    }

    /// Get channel as a scalar slice.
    #[inline]
    pub fn channel_f32(&mut self, channel: usize) -> &[f32] {
        let data = self.channel(channel).as_ptr() as *const f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts(data, MAX_BUFFER_SIZE) }
    }

    /// Get channel as a mutable scalar slice.
    #[inline]
    pub fn channel_f32_mut(&mut self, channel: usize) -> &mut [f32] {
        let data = self.channel_mut(channel).as_mut_ptr() as *mut f32;
        // Safety: we know each channel contains exactly `MAX_BUFFER_SIZE` samples.
        unsafe { core::slice::from_raw_parts_mut(data, MAX_BUFFER_SIZE) }
    }

    /// Get immutably borrowed buffer.
    #[inline]
    pub fn buffer_ref(&self) -> BufferRef<'_> {
        // Safety: we know Frames are contiguous and we know the length statically.
        let slice = unsafe {
            core::slice::from_raw_parts(self.array.as_ptr() as *const F32x, N::USIZE << SIMD_C)
        };
        BufferRef::new(slice)
    }

    /// Get mutably borrowed buffer.
    #[inline]
    pub fn buffer_mut(&mut self) -> BufferMut<'_> {
        // Safety: we know Frames are contiguous and we know the length statically.
        let data = self.array.as_mut_ptr() as *mut F32x;
        let slice = unsafe { core::slice::from_raw_parts_mut(data, N::USIZE << SIMD_C) };
        BufferMut::new(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- F32 backward compatibility ---

    #[test]
    fn f32_buffervec_roundtrip() {
        let mut buf = BufferVec::new(2);
        assert_eq!(buf.channels(), 2);
        buf.set_f32(0, 0, 1.0);
        buf.set_f32(0, 63, 2.0);
        buf.set_f32(1, 32, 3.0);
        assert_eq!(buf.at_f32(0, 0), 1.0);
        assert_eq!(buf.at_f32(0, 63), 2.0);
        assert_eq!(buf.at_f32(1, 32), 3.0);
        assert_eq!(buf.at_f32(1, 0), 0.0);
    }

    #[test]
    fn f32_buffervec_simd_roundtrip() {
        let mut buf: BufferVec<F32> = BufferVec::new(1);
        let v = F32::simd_from_fn(|i| i as f32 * 0.5);
        buf.set(0, 0, v);
        let read = buf.at(0, 0);
        for i in 0..8 {
            assert_eq!(F32::get_lane(&read, i), i as f32 * 0.5);
        }
    }

    #[test]
    fn f32_buffer_ref_mut_conversion() {
        let mut buf = BufferVec::new(1);
        buf.set_f32(0, 5, 7.0);
        {
            let bref = buf.buffer_ref();
            assert_eq!(bref.at_f32(0, 5), 7.0);
            assert_eq!(bref.channels(), 1);
        }
        {
            let mut bmut = buf.buffer_mut();
            bmut.set_f32(0, 10, 9.0);
            assert_eq!(bmut.at_f32(0, 10), 9.0);
        }
        assert_eq!(buf.at_f32(0, 10), 9.0);
    }

    #[test]
    fn f32_buffervec_clear() {
        let mut buf = BufferVec::new(2);
        buf.set_f32(0, 0, 42.0);
        buf.set_f32(1, 63, 99.0);
        buf.clear();
        assert_eq!(buf.at_f32(0, 0), 0.0);
        assert_eq!(buf.at_f32(1, 63), 0.0);
    }

    #[test]
    fn f32_buffervec_resize() {
        let mut buf: BufferVec<F32> = BufferVec::new(1);
        assert_eq!(buf.channels(), 1);
        buf.resize(3);
        assert_eq!(buf.channels(), 3);
    }

    #[test]
    fn f32_buffermut_subset() {
        let mut buf = BufferVec::new(4);
        buf.set_f32(2, 0, 5.0);
        let mut bmut = buf.buffer_mut();
        let sub = bmut.subset(2, 1);
        assert_eq!(sub.channels(), 1);
        assert_eq!(sub.at_scalar(0, 0), 5.0);
    }

    // --- F64 generic buffer tests ---

    #[test]
    fn f64_buffervec_roundtrip() {
        let mut buf: BufferVec<F64> = BufferVec::new(2);
        assert_eq!(buf.channels(), 2);
        buf.set_scalar(0, 0, 1.0_f64);
        buf.set_scalar(0, 63, 2.0_f64);
        buf.set_scalar(1, 32, 3.0_f64);
        assert_eq!(buf.at_scalar(0, 0), 1.0);
        assert_eq!(buf.at_scalar(0, 63), 2.0);
        assert_eq!(buf.at_scalar(1, 32), 3.0);
        assert_eq!(buf.at_scalar(1, 0), 0.0);
    }

    #[test]
    fn f64_buffervec_simd_roundtrip() {
        let mut buf: BufferVec<F64> = BufferVec::new(1);
        let v = F64::simd_from_fn(|i| i as f64 * 0.25);
        buf.set(0, 0, v);
        let read = buf.at(0, 0);
        for i in 0..4 {
            assert_eq!(F64::get_lane(&read, i), i as f64 * 0.25);
        }
    }

    #[test]
    fn f64_buffervec_clear() {
        let mut buf: BufferVec<F64> = BufferVec::new(2);
        buf.set_scalar(0, 0, 42.0);
        buf.set_scalar(1, 63, 99.0);
        buf.clear();
        assert_eq!(buf.at_scalar(0, 0), 0.0);
        assert_eq!(buf.at_scalar(1, 63), 0.0);
    }

    #[test]
    fn f64_buffer_ref_mut_conversion() {
        let mut buf: BufferVec<F64> = BufferVec::new(1);
        buf.set_scalar(0, 5, 7.5);
        {
            let bref: BufferRef<'_, F64> = buf.buffer_ref();
            assert_eq!(bref.at_scalar(0, 5), 7.5);
            assert_eq!(bref.channels(), 1);
        }
        {
            let mut bmut: BufferMut<'_, F64> = buf.buffer_mut();
            bmut.set_scalar(0, 10, 9.5);
            assert_eq!(bmut.at_scalar(0, 10), 9.5);
        }
        assert_eq!(buf.at_scalar(0, 10), 9.5);
    }

    #[test]
    fn f64_buffervec_resize() {
        let mut buf: BufferVec<F64> = BufferVec::new(1);
        assert_eq!(buf.channels(), 1);
        buf.resize(4);
        assert_eq!(buf.channels(), 4);
    }

    #[test]
    fn f64_buffervec_channel_len() {
        let buf: BufferVec<F64> = BufferVec::new(3);
        assert_eq!(buf.len(), F64::LEN);
        assert_eq!(buf.length(), 16);
        assert_eq!(buf.channel(0).len(), F64::LEN);
    }

    #[test]
    fn f64_buffermut_subset() {
        let mut buf: BufferVec<F64> = BufferVec::new(4);
        buf.set_scalar(2, 0, 5.5);
        let mut bmut = buf.buffer_mut();
        let sub = bmut.subset(2, 1);
        assert_eq!(sub.channels(), 1);
        assert_eq!(sub.at_scalar(0, 0), 5.5);
    }

    #[test]
    fn f64_buffermut_add() {
        let mut buf: BufferVec<F64> = BufferVec::new(1);
        let v1 = F64::simd_from_fn(|_| 1.0);
        let v2 = F64::simd_from_fn(|_| 2.0);
        buf.set(0, 0, v1);
        let mut bmut = buf.buffer_mut();
        bmut.add(0, 0, v2);
        let result = bmut.at(0, 0);
        for i in 0..4 {
            assert_eq!(F64::get_lane(&result, i), 3.0);
        }
    }

    #[test]
    fn f64_all_samples_addressable() {
        // Verify we can write and read all 64 sample positions per channel.
        let mut buf: BufferVec<F64> = BufferVec::new(1);
        for i in 0..MAX_BUFFER_SIZE {
            buf.set_scalar(0, i, (i + 1) as f64);
        }
        for i in 0..MAX_BUFFER_SIZE {
            assert_eq!(buf.at_scalar(0, i), (i + 1) as f64);
        }
    }

    #[test]
    fn f32_all_samples_addressable() {
        // Same for f32 through the generic API.
        let mut buf: BufferVec<F32> = BufferVec::new(1);
        for i in 0..MAX_BUFFER_SIZE {
            buf.set_scalar(0, i, (i + 1) as f32);
        }
        for i in 0..MAX_BUFFER_SIZE {
            assert_eq!(buf.at_scalar(0, i), (i + 1) as f32);
        }
    }
}
