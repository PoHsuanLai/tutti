use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("VBAP error: {0:?}")]
    VBAPError(vbap::VBAPError),

    /// A channel width with no VBAP speaker preset. Only 2/4/6/8/12 (stereo /
    /// quad / 5.1 / 7.1 / 7.1.4) have presets; anything else — mono, or an
    /// arbitrary `Multi(n)` — has no defined speaker geometry to pan into.
    #[error("no speaker preset for a {0}-channel layout (have 2/4/6/8/12)")]
    UnsupportedSpeakerLayout(u16),
}

impl From<vbap::VBAPError> for Error {
    fn from(err: vbap::VBAPError) -> Self {
        Self::VBAPError(err)
    }
}

pub type Result<T> = core::result::Result<T, Error>;
