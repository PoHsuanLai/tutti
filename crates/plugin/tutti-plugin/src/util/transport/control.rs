//! Host-side IPC to the plugin-server: length-prefixed bincode over a
//! local socket (Unix domain socket on Unix, named pipe on Windows).
//!
//! Wire: `[u32 big-endian length][bincode payload]` both directions, with the
//! body capped at [`MAX_FRAME_BYTES`] — the length arrives from the peer, so it
//! is checked before it is believed, in both `send` and `recv`.
//! EOF at either end surfaces as [`BridgeError::ProcessCrashed`], and so does a
//! length prefix over the cap: a peer that cannot frame has desynchronised the
//! stream, and there is no resynchronisation point to recover to.

use crate::error::{BridgeError, Result};
use crate::protocol::{BridgeMessage, HostMessage, MAX_FRAME_BYTES};

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
    // Refuse to emit a frame the peer is now required to reject, and — the
    // sharper reason — never let `as u32` truncate the length. A payload above
    // `u32::MAX` would be prefixed with its low 32 bits, so the peer would read
    // that many bytes and treat whatever followed as the next frame's header:
    // permanent desync rather than a clean error, on a stream that has no
    // resynchronisation point. The cap is well under `u32::MAX`, so checking it
    // subsumes the truncation.
    //
    // Reachable through `LoadState { data }`, whose chunk comes from a project
    // file. A corrupt or hostile document is the path in.
    if data.len() > MAX_FRAME_BYTES {
        return Err(BridgeError::ConnectionFailed(format!(
            "outgoing frame is {} bytes, over the {MAX_FRAME_BYTES}-byte protocol limit",
            data.len()
        )));
    }
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
    // Reject before allocating. The length is the peer's word for how much
    // memory to reserve, so honouring it unchecked lets a corrupt server ask
    // for 4 GiB — and then leaves the read below stallable indefinitely,
    // because `SO_RCVTIMEO` restarts per syscall and `read_exact` loops. See
    // [`MAX_FRAME_BYTES`] for both hazards in full.
    //
    // The verdict is `ProcessCrashed` rather than a decode error: a peer that
    // cannot frame is a peer the host has no way to keep talking to, and the
    // stream is now desynchronised — the bytes after this prefix are not a
    // frame boundary. `pump` turns the error into `crash()`, which is the
    // correct end state.
    if len > MAX_FRAME_BYTES {
        return Err(BridgeError::ProcessCrashed);
    }
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

/// The two bounds `MAX_FRAME_BYTES` has to sit between, checked at compile time.
///
/// A `const` block rather than a `#[test]`: both operands are constants, so
/// there is nothing to observe at runtime that the compiler cannot settle
/// first, and a bad cap should fail the build rather than one test binary.
/// (Clippy says the same thing via `assertions_on_constants`.)
const _: () = {
    // `send` writes `data.len() as u32`. That cast is only safe because the
    // guard above it rejects everything larger, so this asserts the premise the
    // cast depends on. Raise the constant past `u32::MAX` and the guard stops
    // covering the truncation it was written to prevent — silently, since the
    // cast itself never fails.
    assert!(
        MAX_FRAME_BYTES <= u32::MAX as usize,
        "MAX_FRAME_BYTES exceeds what a u32 length prefix can carry, so \
         `data.len() as u32` in `send` can still truncate and desynchronise \
         the stream"
    );

    // The cap must also leave room for the largest message the protocol
    // defines. The binding case is a plugin state chunk (`LoadState` /
    // `StateData`), which for a sample-based instrument reaches single-digit
    // MiB. A cap below that would turn preset loading into a connection
    // failure — the regression a "tighten it until nothing complains" edit
    // would cause, and one no test in this crate would catch, since none sends
    // a realistically large state chunk.
    assert!(
        MAX_FRAME_BYTES >= 16 * 1024 * 1024,
        "MAX_FRAME_BYTES is below the state chunk a sampler can legitimately \
         produce, so `save_state`/`load_state` would fail on real plugins"
    );
};
