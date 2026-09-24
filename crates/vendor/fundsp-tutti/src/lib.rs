#![cfg_attr(docsrs, feature(doc_cfg))]
//! FunDSP is an audio processing and synthesis library.
//!
//! See `README.md` in crate root folder for an overview.
//! For a list of changes, see `CHANGES.md` in the same folder.
//!
//! The central abstractions are located in the `audionode` and `audiounit` modules.
//! The `combinator` module defines the graph operators.
// ── This crate links `std`, unconditionally ─────────────────────────────────
//
// Upstream FunDSP is `no_std`-capable and this file used to carry
// `#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]` with a `std`
// feature to match. That claim was **false in this fork** and had been for some
// time: `cargo check -p fundsp-tutti --no-default-features` failed with five
// errors in `latency/`, a Tutti addition that uses `Vec` and
// `std::array::from_fn` unguarded. So the configuration was advertised, was
// unbuildable, and nothing in the tree ever selected it — `tutti-core`, the one
// consumer, passes `features = ["std"]`.
//
// `no_std` is not a goal for Tutti (decided 2026-09-02), so the gate is gone
// rather than repaired: a feature that cannot be turned off is decoration, and
// one that can be turned off into a build that does not compile is worse. The
// engine's own leaf crates make the same call — see `tutti-node`'s `Cargo.toml`.
//
// Restoring it means fixing `latency/` first, then re-adding the feature and a
// CI job that actually builds it.
#![allow(
    clippy::precedence,
    clippy::type_complexity,
    clippy::float_cmp,
    clippy::len_zero,
    double_negations,
    clippy::needless_range_loop,
    clippy::manual_range_contains,
    clippy::too_many_arguments,
    clippy::comparison_chain,
    clippy::unnecessary_cast
)]

// ── The node contract lives in `tutti-node` ─────────────────────────────────
//
// The numeric tower (`Sample`/`F32`/`F64`, `Num`/`Int`/`Float`/`Real`, the SIMD
// geometry constants and `MAX_BUFFER_SIZE`), the planar block buffers, the
// `Signal` routing vocabulary, `Setting`, and the `AudioUnit` trait itself were
// relocated **down** into `tutti-node`, which sits below this crate so that a
// node can be written without depending on the fork.
//
// They are re-exported here at the same spellings they had when they were
// defined here, so this crate's own modules — and `prelude` / `prelude32` /
// `prelude64`, which glob this root — compile unchanged. A consumer that wants
// the contract without the fork now has a crate to name; a consumer that does
// not care keeps `fundsp_tutti::Float` working.
pub use tutti_node::{
    F32, F32x, F64, F64x, Float, Frame, I32x, I64x, Int, MAX_BUFFER_LOG, MAX_BUFFER_SIZE, Num,
    Real, SIMD_C, SIMD_LEN, SIMD_M, SIMD_N, SIMD_S, Sample, Size, U32x, convert, full_simd_items,
    full_simd_items_s, simd_items, simd_items_s,
};

// The tower's own `use` list, kept here because every module in this crate
// says `use super::*` and reached these through the root. They were never
// re-exported deliberately — they were in scope as a side effect of the tower
// being defined in this file — but ~190 sites (arity arithmetic like
// `M: Size<f32> + Mul<N>`, and every `impl Add for An<X>`) depend on it, so
// dropping them here would be a churn this relocation is not for.
use core::ops::{Add, Mul, Not};
use numeric_array::typenum::U1;
use wide::{f32x8, f64x4};

#[doc(inline)]
pub use params::SampleRate;

/// Default sample rate as a typed [`SampleRate`] — an alias of the engine's one
/// placeholder rate, [`SampleRate::DEFAULT`], kept so this crate's own nodes
/// compile unchanged. Nothing outside the fork names it.
pub const DEFAULT_SAMPLE_RATE: SampleRate = SampleRate::DEFAULT;

/// [`DEFAULT_SAMPLE_RATE`] as a raw `f64`, for the fork's coefficient math.
/// It used to be `tutti-node`'s own constant; it is now derived here, so the
/// engine has one default rate rather than two that happen to agree.
pub const DEFAULT_SR: f64 = SampleRate::DEFAULT.0;

pub mod adsr;
pub mod audionode;
pub mod audiounit;
pub mod biquad;
pub mod biquad_bank;
/// The planar block buffers, re-exported from [`tutti_node`].
///
/// Kept as a module rather than a flat re-export because callers spell
/// `fundsp::buffer::BufferArray` (`tutti-core`'s `engine.rs` does), and the
/// preludes `pub use super::buffer::*`. Both keep working.
pub mod buffer {
    pub use tutti_node::buffer::*;
}
pub mod combinator;
pub mod delay;
pub mod denormal;
pub mod dynamics;
pub mod envelope;
pub mod feedback;
pub mod fft;
pub mod filter;
pub mod fir;
pub mod follow;
pub mod generate;
pub mod granular;
pub mod graph;
pub mod latency;
pub mod math;
pub mod moog;
pub mod net;
pub mod noise;
pub mod oscillator;
pub mod oversample;
pub mod pan;
pub mod params;
pub mod prelude;
pub mod prelude32;
pub mod prelude64;
pub mod realnet;
pub mod realseq;
pub mod resample;
pub mod resynth;
pub mod reverb;
pub mod rez;
pub mod ring;
pub mod sequencer;
pub mod setting;
pub mod shape;
pub mod shared;
/// Signal-flow analysis, re-exported from [`tutti_node`].
///
/// `AudioUnit::route` takes and returns a `SignalFrame`, so the vocabulary
/// moved down with the trait. A module (not a flat re-export) because the
/// preludes say `pub use super::signal::*` and consumers spell
/// `fundsp::signal::Signal`.
pub mod signal {
    pub use tutti_node::signal::*;
}
pub mod slot;
pub mod snoop;
pub mod sound;
pub mod svf;
pub mod system;
pub mod unit_param;
pub mod vertex;
pub mod wave;
pub mod wavetable;

// GenericSequence is for Frame::generate.
pub use numeric_array::{
    self,
    generic_array::sequence::{Concat, GenericSequence},
    typenum,
};

pub use funutd;
pub use lfqueue;
pub use wide;

extern crate alloc;
pub use alloc::sync::Arc;
pub type Queue<T, const N: usize> = lfqueue::ConstBoundedQueue<T, N>;

pub mod write;

// No file decode here. `read.rs` (`Wave::load`, `WaveAsset`, `WaveMetadata`,
// `WaveError`), `stream.rs` (`FileIn`) and the peak builder only `read.rs`
// used moved to `tutti-io`, with symphonia and the codec features (tutti
// design doc 013, Phase 0). This fork's `Wave` stays for its own nodes and its
// WAV writer, and cannot load a file.

#[cfg(feature = "fft")]
pub mod convolve;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_sample_constants() {
        assert_eq!(F32::S, 3);
        assert_eq!(F32::N, 8);
        assert_eq!(F32::M, 7);
        assert_eq!(F32::C, 3);
        assert_eq!(F32::LEN, 8);
        // LEN * N == MAX_BUFFER_SIZE
        assert_eq!(F32::LEN * F32::N, MAX_BUFFER_SIZE);
    }

    #[test]
    fn f64_sample_constants() {
        assert_eq!(F64::S, 2);
        assert_eq!(F64::N, 4);
        assert_eq!(F64::M, 3);
        assert_eq!(F64::C, 4);
        assert_eq!(F64::LEN, 16);
        // LEN * N == MAX_BUFFER_SIZE
        assert_eq!(F64::LEN * F64::N, MAX_BUFFER_SIZE);
    }

    #[test]
    fn f32_backward_compat_constants() {
        // Existing global constants must match F32 values.
        assert_eq!(SIMD_S, F32::S);
        assert_eq!(SIMD_N, F32::N);
        assert_eq!(SIMD_M, F32::M);
        assert_eq!(SIMD_C, F32::C);
        assert_eq!(SIMD_LEN, F32::LEN);
    }

    #[test]
    fn simd_from_fn_f32() {
        let v = F32::simd_from_fn(|i| (i + 1) as f32);
        for i in 0..8 {
            assert_eq!(F32::get_lane(&v, i), (i + 1) as f32);
        }
    }

    #[test]
    fn simd_from_fn_f64() {
        let v = F64::simd_from_fn(|i| (i + 10) as f64);
        for i in 0..4 {
            assert_eq!(F64::get_lane(&v, i), (i + 10) as f64);
        }
    }

    #[test]
    fn simd_set_get_lane_f32() {
        let mut v = F32::simd_zero();
        F32::set_lane(&mut v, 3, 42.0);
        assert_eq!(F32::get_lane(&v, 3), 42.0);
        assert_eq!(F32::get_lane(&v, 0), 0.0);
    }

    #[test]
    fn simd_set_get_lane_f64() {
        let mut v = F64::simd_zero();
        F64::set_lane(&mut v, 2, 99.5);
        assert_eq!(F64::get_lane(&v, 2), 99.5);
        assert_eq!(F64::get_lane(&v, 0), 0.0);
    }

    #[test]
    fn simd_items_s_matches_legacy() {
        for samples in 0..=MAX_BUFFER_SIZE {
            assert_eq!(simd_items_s::<F32>(samples), simd_items(samples));
            assert_eq!(full_simd_items_s::<F32>(samples), full_simd_items(samples));
        }
    }

    #[test]
    fn simd_items_s_f64() {
        // 0 samples => 0 SIMD items
        assert_eq!(simd_items_s::<F64>(0), 0);
        // 1..4 samples => 1 SIMD item (partial)
        assert_eq!(simd_items_s::<F64>(1), 1);
        assert_eq!(simd_items_s::<F64>(4), 1);
        // 5 samples => 2 SIMD items
        assert_eq!(simd_items_s::<F64>(5), 2);
        // 64 samples => 16 SIMD items
        assert_eq!(simd_items_s::<F64>(64), 16);
        // Full items: 3 samples => 0 full items
        assert_eq!(full_simd_items_s::<F64>(3), 0);
        // Full items: 4 samples => 1 full item
        assert_eq!(full_simd_items_s::<F64>(4), 1);
    }
}
