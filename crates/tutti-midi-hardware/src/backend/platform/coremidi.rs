//! The macOS platform: CoreMIDI.

use crate::InputProducerHandle;
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};

/// Returns this platform's MIDI backend.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::coremidi::CoreMidiEndpoints::new())
}

/// Publishes the virtual source `spec` describes.
pub(crate) fn open_virtual_source(
    spec: &VirtualSpec,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    let source = super::coremidi::UmpVirtualSource::open(spec)?;
    Ok((Box::new(source), spec.protocol()))
}

/// Publishes the virtual destination `spec` describes, delivering into `producer`.
pub(crate) fn open_virtual_destination(
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(Box<dyn InputConnection>, VirtualProtocol)> {
    let destination = super::coremidi::UmpVirtualDestination::open(spec, producer)?;
    Ok((Box::new(destination), spec.protocol()))
}

impl From<tutti_coremidi_ffi::Error> for Error {
    fn from(e: tutti_coremidi_ffi::Error) -> Self {
        Self::CoreMidi {
            operation: e.operation,
            status: e.status,
        }
    }
}
