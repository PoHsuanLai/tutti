//! The Linux platform: ALSA's UMP sequencer.

use crate::InputProducerHandle;
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};

/// Returns this platform's MIDI backend.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::alsa::AlsaEndpoints::new())
}

/// Publishes the virtual source `spec` describes.
pub(crate) fn open_virtual_source(
    spec: &VirtualSpec,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    let (source, protocol) = super::alsa::virtual_endpoint::source(spec)?;
    Ok((Box::new(source), protocol))
}

/// Publishes the virtual destination `spec` describes, delivering into `producer`.
pub(crate) fn open_virtual_destination(
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(Box<dyn InputConnection>, VirtualProtocol)> {
    let (destination, protocol) = super::alsa::virtual_endpoint::destination(spec, producer)?;
    Ok((Box::new(destination), protocol))
}

impl From<tutti_alsa_ffi::Error> for Error {
    fn from(e: tutti_alsa_ffi::Error) -> Self {
        Self::Alsa {
            operation: e.operation,
            code: e.code,
        }
    }
}
