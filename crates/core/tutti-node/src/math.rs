//! The arithmetic the node contract itself needs, and nothing more.
//!
//! This is deliberately a *subset* of the fork's `math` module rather than the
//! whole of it. The fork's version is a DSP toolbox — noise generators, easing
//! curves, `midi_hz`, softsign, the `SegmentInterpolator` family — and none of
//! that is part of what it means to be a node. What is:
//!
//! - the [`Num`]/[`Real`]-generic wrappers [`Signal`](crate::signal::Signal)
//!   and [`AudioUnit`](crate::AudioUnit) call through (`min`, `abs`, `ceil`,
//!   `floor`, `round`, `log10`, `amp_db`), and
//! - [`AttoHash`], because `AudioUnit::ping` takes and returns one. The hash is
//!   part of the trait's signature, so it has to be here; splitting it from the
//!   fork's copy would change every node's deterministic starting phase, which
//!   is audible.
//!
//! The fork re-exports these, so `fundsp_tutti::math::{min, AttoHash, …}` keeps
//! resolving and its own toolbox keeps its single home.

use crate::num::{Num, Real};
use core::ops::BitXor;

/// The absolute function.
#[inline]
pub fn abs<T: Num>(x: T) -> T {
    x.abs()
}

/// Minimum function.
#[inline]
pub fn min<T: Num>(x: T, y: T) -> T {
    x.min(y)
}

/// Maximum function.
#[inline]
pub fn max<T: Num>(x: T, y: T) -> T {
    x.max(y)
}

/// Floor function.
#[inline]
pub fn floor<T: Num>(x: T) -> T {
    x.floor()
}

/// Ceiling function.
#[inline]
pub fn ceil<T: Num>(x: T) -> T {
    x.ceil()
}

/// Rounds `x`.
#[inline]
pub fn round<T: Num>(x: T) -> T {
    x.round()
}

/// Base 10 logarithm.
#[inline]
pub fn log10<T: Real>(x: T) -> T {
    x.log10()
}

/// Convert amplitude `gain` (`gain` > 0) to decibels. Gain 1.0 = 0 dB (unity gain).
#[inline]
pub fn amp_db<T: Real>(gain: T) -> T {
    log10(gain) * T::new(20)
}

/// Pico sized hasher.
/// It is used in computing deterministic pseudorandom phase hashes.
#[derive(Default, Clone)]
pub struct AttoHash {
    state: u64,
}

impl AttoHash {
    /// Create new hasher from seed.
    #[inline]
    pub fn new(seed: u64) -> AttoHash {
        AttoHash { state: seed }
    }
    /// Generator state.
    #[inline]
    pub fn state(&self) -> u64 {
        self.state
    }
    /// Hash `data`. Consumes self and returns a new `AttoHash`.
    #[inline]
    pub fn hash(self, data: u64) -> Self {
        // Hash taken from FxHasher.
        AttoHash {
            state: self
                .state
                .rotate_left(5)
                .bitxor(data)
                .wrapping_mul(0x517cc1b727220a95),
        }
    }
    /// Get current hash in 0...1.
    #[inline]
    pub fn hash01<T: crate::num::Float>(self) -> T {
        let x = funutd::hash::hash64a(self.state);
        T::from_f64((x >> 11) as f64 / (1u64 << 53) as f64)
    }
    /// Get current hash in -1...1.
    #[inline]
    pub fn hash11<T: crate::num::Float>(self) -> T {
        let x = funutd::hash::hash64a(self.state);
        T::from_f64((x >> 10) as f64 / (1u64 << 53) as f64 - 1.0)
    }
}
