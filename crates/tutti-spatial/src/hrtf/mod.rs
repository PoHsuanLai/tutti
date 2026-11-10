//! Binaural rendering by FFT convolution against a measured HRIR sphere.
//! The convolver, the sphere's parsing, resampling and face lookup are
//! `tutti_dsp::HrtfBinaural`'s; the node here keeps the controls and the
//! de-zipper.
//! Always 2 channels out; no speaker layout involved.
//!
//! There is no `build_binaural_mix` counterpart to
//! [`build_vbap_mix`](crate::vbap::build_vbap_mix): summing binaural
//! renders is plain stereo addition, which the graph's fan-in (several
//! sources on one sink port or output channel) already does.

mod node;

pub use node::{HrtfBinauralControls, HrtfBinauralNode};
