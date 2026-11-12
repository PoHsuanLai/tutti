//! Time-modulation effects: chorus, flanger and phaser.
//!
//! All three sweep something with an internal LFO and blend the result back
//! against the dry signal, but they split into two mechanisms.
//! [`ModDelayNode`] sweeps a **delay time** — chorus and flanger are the same
//! node with different [`Sweep`](tutti_dsp::Sweep)s, chorus sitting at a longer base
//! delay (10 ms) for thickening and flanger at a short one (1 ms) for the swept
//! comb notches. [`PhaserNode`] sweeps the centre of an **all-pass chain**
//! instead, so it notches by phase cancellation and uses no delay line at all.
//!
//! Both are width-generic, with per-channel phase offsets. The chorus and flanger's LFO is
//! state of its recurrence on the stream's clock; the phaser still computes its own once
//! per block into a buffer every channel reads.
//!
//! Consequence worth knowing: chorus and flanger take their depth in
//! [`Seconds`](tutti_types::Seconds) of delay sweep, while the phaser's is a
//! unitless [`Depth`](tutti_types::Depth) fraction of its frequency range.

mod shared;

mod mod_delay;
mod phaser;

pub use mod_delay::ModDelayNode;
pub use phaser::PhaserNode;
