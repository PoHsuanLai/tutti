//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + bincode payload.

use interprocess::local_socket::{
    traits::Listener as _, GenericFilePath, ListenerOptions, Stream, ToFsName as _,
};
use std::io::{Read, Write};
use tutti_plugin::server::{BridgeMessage, HostMessage};
use tutti_plugin::Result;

pub(crate) trait Transport: Send {
    fn recv(&mut self) -> Result<HostMessage>;
    fn send(&mut self, msg: &BridgeMessage) -> Result<()>;
}

pub(crate) struct SocketTransport {
    stream: Stream,
}

impl SocketTransport {
    pub(crate) fn new(stream: Stream) -> Self {
        Self { stream }
    }
}

impl Transport for SocketTransport {
    fn recv(&mut self) -> Result<HostMessage> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        self.stream.read_exact(&mut data)?;
        Ok(bincode::deserialize(&data)?)
    }

    fn send(&mut self, msg: &BridgeMessage) -> Result<()> {
        let data = bincode::serialize(msg)?;
        self.stream.write_all(&(data.len() as u32).to_be_bytes())?;
        self.stream.write_all(&data)?;
        Ok(())
    }
}

pub(crate) struct TransportListener {
    listener: interprocess::local_socket::Listener,
}

impl TransportListener {
    pub(crate) fn bind(socket_path: &std::path::Path) -> Result<Self> {
        let _ = std::fs::remove_file(socket_path);
        let name = socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let listener = ListenerOptions::new().name(name).create_sync()?;
        Ok(Self { listener })
    }

    pub(crate) fn accept(&self) -> Result<SocketTransport> {
        let stream = self.listener.accept()?;
        Ok(SocketTransport::new(stream))
    }
}
