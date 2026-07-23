//! Host-side IPC to the plugin-server: length-prefixed bincode over a
//! local socket (Unix domain socket on Unix, named pipe on Windows).
//!
//! Wire: `[u32 big-endian length][bincode payload]` both directions.
//! EOF at either end surfaces as [`BridgeError::ProcessCrashed`].

use crate::error::{BridgeError, Result};
use crate::protocol::{BridgeMessage, HostMessage};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName as _};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Duplex byte stream to the plugin-server.
pub type ControlStream = Stream;

pub fn connect(socket: &Path) -> Result<ControlStream> {
    let name = socket
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    Ok(Stream::connect(name)?)
}

pub fn send(stream: &mut ControlStream, msg: &HostMessage) -> Result<()> {
    let data = bincode::serialize(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}

pub fn recv(stream: &mut ControlStream) -> Result<BridgeMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(crashed_on_eof)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).map_err(crashed_on_eof)?;
    Ok(bincode::deserialize(&buf)?)
}

/// Recv with timeout. Uses `set_recv_timeout` on the stream so the
/// blocking read wakes after `timeout`.
pub fn recv_within(stream: &mut ControlStream, timeout: Duration) -> Result<BridgeMessage> {
    use interprocess::local_socket::traits::Stream as _;
    stream
        .set_recv_timeout(Some(timeout))
        .map_err(BridgeError::Io)?;
    let result = recv(stream);
    // Reset to blocking (no timeout).
    let _ = stream.set_recv_timeout(None);
    result.map_err(|e| match e {
        BridgeError::Io(io)
            if io.kind() == std::io::ErrorKind::TimedOut
                || io.kind() == std::io::ErrorKind::WouldBlock =>
        {
            BridgeError::Timeout {
                operation: "recv".to_string(),
                duration_ms: timeout.as_millis() as u64,
            }
        }
        other => other,
    })
}

fn crashed_on_eof(e: std::io::Error) -> BridgeError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        BridgeError::ProcessCrashed
    } else {
        e.into()
    }
}
