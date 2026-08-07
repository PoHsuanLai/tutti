use thiserror::Error;

/// What VBAP panning can fail at. Module-scoped because both variants are
/// speaker geometry; HRTF has its own error type.
#[derive(Debug, Clone, Error)]
pub enum VbapError {
    #[error("VBAP error: {0:?}")]
    Vbap(vbap::VBAPError),

    /// A width with no speaker preset. Only 2/4/6/8/12 are defined.
    #[error("no speaker preset for a {0}-channel layout (have 2/4/6/8/12)")]
    UnsupportedSpeakerLayout(u16),
}

impl From<vbap::VBAPError> for VbapError {
    fn from(err: vbap::VBAPError) -> Self {
        Self::Vbap(err)
    }
}

pub type Result<T> = core::result::Result<T, VbapError>;
