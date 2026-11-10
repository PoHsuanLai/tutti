//! [`SpeakerGeometryError`].

use thiserror::Error;

/// Why a [`SpeakerLayout`](super::SpeakerLayout) could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SpeakerGeometryError {
    /// Fewer speakers than the layout's dimension needs: two for a horizontal
    /// layout, three for one with height.
    #[error("{provided} speakers given, at least {required} are needed")]
    InsufficientSpeakers {
        /// How many speakers were given.
        provided: usize,
        /// How many the layout's dimension needs.
        required: usize,
    },

    /// More speakers than [`MAX_SPEAKERS`](super::MAX_SPEAKERS).
    #[error("{provided} speakers given, at most {max} are supported")]
    TooManySpeakers {
        /// How many speakers were given.
        provided: usize,
        /// The most a layout holds.
        max: usize,
    },

    /// A speaker's bearing or height is not a finite number.
    #[error("a speaker position is not finite")]
    NonFinitePosition,

    /// No pair or triplet of speakers could be formed: the speakers are too
    /// close together, too far apart for one pair, or lie in a plane through
    /// the listener.
    #[error("no valid speaker pair or triplet can be formed from these positions")]
    NoValidTuples,
}
