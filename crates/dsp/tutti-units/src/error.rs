use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("VBAP error: {0:?}")]
    VBAPError(vbap::VBAPError),
}

impl From<vbap::VBAPError> for Error {
    fn from(err: vbap::VBAPError) -> Self {
        Self::VBAPError(err)
    }
}

pub type Result<T> = core::result::Result<T, Error>;
