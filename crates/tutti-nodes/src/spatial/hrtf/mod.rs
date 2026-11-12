//! Binaural rendering by FFT convolution against a measured HRIR sphere.
//! The convolver, the sphere's parsing, resampling and face lookup are
//! `tutti_dsp::HrtfBinaural`'s; the node here keeps the params and the
//! de-zipper.
//! Always 2 channels out; no speaker layout involved.
//!
//! Summing binaural renders is plain fan-in: wire each render to the same
//! stereo ports and the graph sums them.

mod node;

pub use node::HrtfBinauralNode;
