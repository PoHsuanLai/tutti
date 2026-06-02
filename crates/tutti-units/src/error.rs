use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[cfg(feature = "spatial")]
    #[error("VBAP error: {0:?}")]
    VBAPError(vbap::VBAPError),
}

#[cfg(feature = "spatial")]
impl From<vbap::VBAPError> for Error {
    fn from(err: vbap::VBAPError) -> Self {
        Self::VBAPError(err)
    }
}

pub type Result<T> = core::result::Result<T, Error>;
