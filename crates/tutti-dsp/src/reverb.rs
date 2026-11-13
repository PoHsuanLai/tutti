//! The reverbs: their model, the [`Network`](network::Network), and the two
//! designs over it, Freeverb and the feedback delay network, each a design and the
//! kernel [`Realized`](crate::Realized) runs.
//!
//! The model is a description, and each algorithm owns its realization: an engine that
//! interpreted every [`Network`](network::Network) would fix one loop and one float order
//! for all of them, where each realization keeps its own (Freeverb's is the classic
//! comb and all-pass order). The link between the two is the reverb cases of the
//! `contract` feature, which hold a realization's measured RT60, pre-delay, wet-only
//! silence, freeze and line lengths to its network's analytic queries, so the analysis
//! and the render cannot drift apart without a failure. Algorithms not built here fit
//! the model too: a Schroeder all-pass is a one-line network, early reflections are a
//! [`Taps`](network::Taps) stage, and a convolution is the [`Fir`](network::Stage::Fir)
//! stage's summary.

pub(crate) mod fdn;
pub(crate) mod freeverb;
pub(crate) mod network;
