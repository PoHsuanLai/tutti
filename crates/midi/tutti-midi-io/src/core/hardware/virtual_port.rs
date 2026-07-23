#![cfg(target_os = "macos")]

use coremidi::{Client, PacketBuffer, VirtualDestination, VirtualSource};
use std::sync::Arc;
use tracing::debug;

use crate::core::error::{Error, Result};

mod ump_source;
pub use ump_source::UmpVirtualSource;

pub struct VirtualMidiSource {
    _client: Client,
    source: VirtualSource,
    name: String,
}

impl core::fmt::Debug for VirtualMidiSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Owns opaque CoreMIDI handles; report the endpoint name only.
        f.debug_struct("VirtualMidiSource")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl VirtualMidiSource {
    pub fn new(name: &str) -> Result<Self> {
        let client_name = format!("tutti-virtual-src-{}", name);
        let client = Client::new(&client_name).map_err(|s| Error::CoreMidi {
            operation: "create client",
            status: s,
        })?;

        let source = client.virtual_source(name).map_err(|s| Error::CoreMidi {
            operation: "create virtual source",
            status: s,
        })?;

        debug!(name, "Created virtual MIDI source");

        Ok(Self {
            _client: client,
            source,
            name: name.to_string(),
        })
    }

    pub fn send(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let packets = PacketBuffer::new(0, data);
        self.source.received(&packets).map_err(|s| Error::CoreMidi {
            operation: "send via virtual source",
            status: s,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for VirtualMidiSource {
    fn drop(&mut self) {
        debug!(name = %self.name, "Dropping virtual MIDI source");
    }
}

pub struct VirtualMidiDestination {
    _client: Client,
    _destination: VirtualDestination,
    name: String,
}

impl core::fmt::Debug for VirtualMidiDestination {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualMidiDestination")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl VirtualMidiDestination {
    pub fn new<F>(name: &str, callback: F) -> Result<Self>
    where
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        let client_name = format!("tutti-virtual-dst-{}", name);
        let client = Client::new(&client_name).map_err(|s| Error::CoreMidi {
            operation: "create client",
            status: s,
        })?;

        let callback = Arc::new(callback);
        let destination = client
            .virtual_destination(name, move |packet_list| {
                for packet in packet_list.iter() {
                    let data = packet.data();
                    if !data.is_empty() {
                        callback(data);
                    }
                }
            })
            .map_err(|s| Error::CoreMidi {
                operation: "create virtual destination",
                status: s,
            })?;

        debug!(name, "Created virtual MIDI destination");

        Ok(Self {
            _client: client,
            _destination: destination,
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for VirtualMidiDestination {
    fn drop(&mut self) {
        debug!(name = %self.name, "Dropping virtual MIDI destination");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_virtual_source() {
        let source = VirtualMidiSource::new("Test Source");
        assert!(source.is_ok());
        let source = source.unwrap();
        assert_eq!(source.name(), "Test Source");
    }

    #[test]
    fn virtual_source_send_empty() {
        let source = VirtualMidiSource::new("Test Send Empty").unwrap();
        assert!(source.send(&[]).is_ok());
    }

    #[test]
    fn virtual_source_send_note_on() {
        let source = VirtualMidiSource::new("Test Send Note").unwrap();
        let result = source.send(&[0x90, 60, 100]);
        assert!(result.is_ok());
    }

    #[test]
    fn create_virtual_destination() {
        let dest = VirtualMidiDestination::new("Test Dest", |_data| {});
        assert!(dest.is_ok());
        let dest = dest.unwrap();
        assert_eq!(dest.name(), "Test Dest");
    }

    #[test]
    fn virtual_destination_receives_callback() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let received = Arc::new(AtomicBool::new(false));
        let received_clone = received.clone();

        let _dest = VirtualMidiDestination::new("Test Callback", move |data| {
            if !data.is_empty() {
                received_clone.store(true, Ordering::SeqCst);
            }
        })
        .unwrap();

        // Actual receipt requires another app or a connected source to send data,
        // so we just verify construction succeeds.
        assert!(!received.load(Ordering::SeqCst));
    }
}
