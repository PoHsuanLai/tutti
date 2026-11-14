//! The Windows platform: Windows Multimedia.

use crate::InputProducerHandle;
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};

/// Why no virtual endpoint can be published here.
const VIRTUAL_REASON: &str = "Windows Multimedia has no virtual MIDI ports";

/// Returns this platform's MIDI backend.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::winmm::WinMmEndpoints::new())
}

/// Refuses: winmm has no virtual ports.
pub(crate) fn open_virtual_source(
    _spec: &VirtualSpec,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    Err(Error::Unsupported(VIRTUAL_REASON))
}

/// Refuses: winmm has no virtual ports.
pub(crate) fn open_virtual_destination(
    _spec: &VirtualSpec,
    _producer: InputProducerHandle,
) -> Result<(Box<dyn InputConnection>, VirtualProtocol)> {
    Err(Error::Unsupported(VIRTUAL_REASON))
}

impl From<tutti_winmm_ffi::Error> for Error {
    fn from(e: tutti_winmm_ffi::Error) -> Self {
        Self::WinMm {
            operation: e.operation,
            code: e.code,
        }
    }
}
