//! Time-modulation effects: chorus, flanger and phaser.
//!
//! All three sweep something with an internal LFO and blend the result back
//! against the dry signal, but they split into two mechanisms.
//! [`ChorusNode`] and [`FlangerNode`] sweep a **delay time** — they share
//! [`modulated_delay`] and differ only in their defaults, chorus sitting at a
//! longer base delay (10 ms) for thickening and flanger at a short one (1 ms)
//! for the swept comb notches. [`PhaserNode`] sweeps the centre of an
//! **all-pass chain** instead, so it notches by phase cancellation and uses no
//! delay line at all.
//!
//! Consequence worth knowing: chorus and flanger take their depth in
//! [`Seconds`](tutti_core::Seconds) of delay sweep, while the phaser's is a
//! unitless [`Depth`](tutti_core::Depth) fraction of its frequency range.

mod modulated_delay;
mod shared;

mod chorus;
mod flanger;
mod phaser;

pub use chorus::ChorusNode;
pub use flanger::FlangerNode;
pub use phaser::{PhaserNode, StereoPhaserNode};
