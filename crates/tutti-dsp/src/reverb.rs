//! The reverbs: their model, the [`Network`](network::Network), and the two
//! designs over it, Freeverb and the feedback delay network, each a design and the
//! kernel [`Realized`](crate::Realized) runs.

pub(crate) mod fdn;
pub(crate) mod freeverb;
pub(crate) mod network;
