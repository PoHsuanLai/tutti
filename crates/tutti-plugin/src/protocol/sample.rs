//! Sample-format discriminator that travels on the wire.
//!
//! Runtime counterparts (`Sample` trait, `AudioBuffer<T>`) live in `crate::protocol::audio`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleFormat {
    Float32,
    Float64,
}

#[allow(clippy::derivable_impls)]
impl Default for SampleFormat {
    fn default() -> Self {
        Self::Float32
    }
}
