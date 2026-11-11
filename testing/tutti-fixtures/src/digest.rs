//! What a digest test is made of: a seeded noise generator and an FNV-1a hash
//! over the bits of a render.
//!
//! A digest test pins a render bit for bit (a tolerance would let a changed
//! coefficient through): it feeds a node [`noise`], hashes every output sample
//! with [`Fnv`], and compares the figure with one recorded earlier. Both are
//! plain `f32`, `u32` and `u64` code with no workspace type in a signature, so
//! a crate at any layer can dev-depend on this one for them. Where the render
//! reaches `libm` the comparison goes through `tutti_nodes::testing::assert_golden`.

/// Deterministic broadband noise: an endless stream of `f32` in `[-1, 1)` from
/// a linear congruential generator (Numerical Recipes' constants, the top 24
/// bits of the state).
///
/// [`Noise::seeded`] scrambles the seed first, so neighbouring seeds give
/// unrelated streams; [`Noise::from_state`] starts from the raw state.
#[derive(Clone, Debug)]
pub struct Noise(u32);

impl Noise {
    /// The stream of `seed`.
    pub fn seeded(seed: u32) -> Self {
        Self(seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453))
    }

    /// The stream that starts from generator state `state` as it is.
    pub fn from_state(state: u32) -> Self {
        Self(state)
    }
}

impl Iterator for Noise {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        Some((self.0 >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
    }
}

/// `len` samples of [`Noise::seeded`]`(seed)`, in `[-1, 1)`.
///
/// Scale it with a `map` (`0.5 * x`): a gain applied after the draw is what
/// every digest test that wants a level does.
pub fn noise(seed: u32, len: usize) -> Vec<f32> {
    Noise::seeded(seed).take(len).collect()
}

/// A 64-bit FNV-1a hash, fed bytes or the little-endian bits of a number.
///
/// Feeding is order-sensitive and chunk-insensitive: `f32s(&[a, b])` is
/// `f32(a)` then `f32(b)`, and `bytes` of a concatenation is `bytes` of each
/// part in turn.
///
/// ```
/// use tutti_fixtures::Fnv;
///
/// let mut h = Fnv::new();
/// h.f32s(&[1.0, -0.5]);
/// let mut g = Fnv::new();
/// g.f32(1.0);
/// g.f32(-0.5);
/// assert_eq!(h.finish(), g.finish());
/// assert_eq!(Fnv::new().finish(), 0xcbf2_9ce4_8422_2325);
/// ```
#[derive(Clone, Debug)]
pub struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// The hash of no bytes.
    pub fn new() -> Self {
        Self(Self::OFFSET)
    }

    /// Feeds `bytes`.
    pub fn bytes(&mut self, bytes: impl IntoIterator<Item = u8>) {
        for byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// Feeds `x`, little-endian.
    pub fn u64(&mut self, x: u64) {
        self.bytes(x.to_le_bytes());
    }

    /// Feeds the bits of `x`, little-endian.
    pub fn f32(&mut self, x: f32) {
        self.bytes(x.to_le_bytes());
    }

    /// Feeds the bits of each sample of `samples`, in order.
    pub fn f32s(&mut self, samples: &[f32]) {
        for &x in samples {
            self.f32(x);
        }
    }

    /// Feeds the bits of `x`, little-endian.
    pub fn f64(&mut self, x: f64) {
        self.bytes(x.to_le_bytes());
    }

    /// The hash of everything fed so far.
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a's published vectors (the empty string, "a", "foobar").
    ///
    /// Mutation (run): the prime's last digit changed, or the xor and the
    /// multiply swapped (FNV-1), fails "a" and "foobar".
    #[test]
    fn fnv_matches_the_published_vectors() {
        let hash = |s: &str| {
            let mut h = Fnv::new();
            h.bytes(s.bytes());
            h.finish()
        };
        assert_eq!(hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash("foobar"), 0x8594_4171_f739_67e8);
    }

    /// A float is fed as its little-endian bits.
    ///
    /// Mutation (run): `to_be_bytes` in `f32` fails.
    #[test]
    fn a_float_is_fed_as_its_little_endian_bits() {
        let mut h = Fnv::new();
        h.f32(1.0);
        let mut g = Fnv::new();
        g.bytes([0x00, 0x00, 0x80, 0x3f]);
        assert_eq!(h.finish(), g.finish());
        let mut h = Fnv::new();
        h.f64(1.0);
        let mut g = Fnv::new();
        g.bytes(1.0f64.to_bits().to_le_bytes());
        assert_eq!(h.finish(), g.finish());
    }

    /// The generator is pinned by a digest of 4096 draws and stays in `[-1, 1)`.
    ///
    /// Mutation (run): the increment `1_013_904_223` changed by one, or the
    /// seed scramble dropped, moves the digest.
    #[test]
    fn noise_is_the_recorded_stream() {
        let x = noise(5, 4096);
        assert!(x.iter().all(|s| (-1.0..1.0).contains(s)));
        let mut h = Fnv::new();
        h.f32s(&x);
        assert_eq!(h.finish(), 0x7d57_5a34_8383_94fa);
        assert_eq!(noise(5, 8), x[..8], "a longer draw extends a shorter one");
        assert_ne!(noise(6, 8), x[..8], "neighbouring seeds differ");
    }
}
