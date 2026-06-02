//! CLAP `clap_istream`/`clap_ostream` adapters backed by in-memory buffers.
//!
//! Used by state save/load — plugins read from an [`InputStream`] and write
//! to an [`OutputStream`] without the host ever having to touch FFI.

use clap_sys::stream::{clap_istream, clap_ostream};
use std::ffi::c_void;
use std::ptr;

/// Writable stream the plugin appends bytes to during state save.
pub struct OutputStream {
    buffer: Vec<u8>,
    stream: clap_ostream,
}

impl OutputStream {
    /// Create an empty output stream.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            stream: clap_ostream {
                ctx: ptr::null_mut(),
                write: Some(ostream_write),
            },
        }
    }

    /// Raw pointer to the `clap_ostream` for FFI. Valid only while `self`
    /// is not moved or dropped.
    pub fn as_raw(&mut self) -> *const clap_ostream {
        self.stream.ctx = &mut self.buffer as *mut Vec<u8> as *mut c_void;
        &self.stream
    }

    /// Borrow the bytes written so far.
    pub fn data(&self) -> &[u8] {
        &self.buffer
    }

    /// Consume the stream and return the accumulated bytes.
    pub fn into_data(self) -> Vec<u8> {
        self.buffer
    }
}

impl Default for OutputStream {
    fn default() -> Self {
        Self::new()
    }
}

unsafe extern "C" fn ostream_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let out_buffer = &mut *((*stream).ctx as *mut Vec<u8>);
    let data = std::slice::from_raw_parts(buffer as *const u8, size as usize);
    out_buffer.extend_from_slice(data);
    size as i64
}

/// Readable stream that hands out slices of a caller-owned byte buffer
/// during state load.
pub struct InputStream<'a> {
    data: &'a [u8],
    position: usize,
    stream: clap_istream,
}

impl<'a> InputStream<'a> {
    /// Wrap an existing byte slice as a CLAP input stream.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            stream: clap_istream {
                ctx: ptr::null_mut(),
                read: Some(istream_read),
            },
        }
    }

    /// Raw pointer to the `clap_istream` for FFI. Valid only for the
    /// lifetime of `self`.
    pub fn as_raw(&mut self) -> *const clap_istream {
        self.stream.ctx = self as *mut InputStream as *mut c_void;
        &self.stream
    }

    /// Number of bytes consumed so far.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Number of bytes left to read.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.position
    }
}

unsafe extern "C" fn istream_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let input = &mut *((*stream).ctx as *mut InputStream);
    let remaining = input.data.len() - input.position;
    let to_read = (size as usize).min(remaining);

    if to_read == 0 {
        return 0;
    }

    let source = &input.data[input.position..input.position + to_read];
    let dest = std::slice::from_raw_parts_mut(buffer as *mut u8, to_read);
    dest.copy_from_slice(source);

    input.position += to_read;
    to_read as i64
}
