//! CLAP `clap_istream`/`clap_ostream` adapters over in-memory buffers.
//!
//! State save/load: a plugin reads from an [`InputStream`] and writes to an
//! [`OutputStream`] without the host touching FFI. The raw vtable exists only
//! for the span of one extension call ([`Plugin::state_save`](crate::Plugin)
//! and friends build it on their own stack).

use crate::guard::guard;
use clap_sys::stream::{clap_istream, clap_ostream};
use std::ffi::c_void;
use std::ptr;

/// A writable stream the plugin appends bytes to during state save.
#[derive(Debug, Default)]
pub struct OutputStream {
    buffer: Vec<u8>,
}

impl OutputStream {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written so far.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.buffer
    }

    /// Consumes the stream and returns the accumulated bytes.
    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.buffer
    }

    /// Run `f` with a `clap_ostream` that appends to this stream. The raw
    /// stream is valid for the call only.
    pub(crate) fn with_raw<R>(&mut self, f: impl FnOnce(*const clap_ostream) -> R) -> R {
        let raw = clap_ostream {
            ctx: ptr::from_mut(self).cast::<c_void>(),
            write: Some(ostream_write),
        };
        f(&raw)
    }

    /// A raw `clap_ostream` over this stream, for a caller that drives the
    /// callbacks itself (a test). The pointer's `ctx` addresses `self`, so it
    /// is valid until `self` moves or drops; `raw` is where it is stored.
    pub fn raw(&mut self, raw: &mut Option<clap_ostream>) -> *const clap_ostream {
        let stored = raw.insert(clap_ostream {
            ctx: ptr::from_mut(self).cast::<c_void>(),
            write: Some(ostream_write),
        });
        ptr::from_ref(stored)
    }
}

extern "C" fn ostream_write(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    guard(-1, || {
        if stream.is_null() || (buffer.is_null() && size != 0) {
            return -1;
        }
        // SAFETY: `ctx` is the `&mut OutputStream` the stream was built from
        // (`with_raw`/`raw`), which outlives the call it is handed to; the plugin
        // calls `write` only inside that call.
        let out = unsafe { &mut *(*stream).ctx.cast::<OutputStream>() };
        // SAFETY: the plugin guarantees `buffer` addresses `size` readable bytes
        // for the duration of the call (`stream.h`).
        let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), size as usize) };
        out.buffer.extend_from_slice(data);
        size as i64
    })
}

/// A readable stream over a caller-owned byte buffer, for state load.
#[derive(Debug)]
pub struct InputStream<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> InputStream<'a> {
    /// Wraps a byte slice.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// Number of bytes consumed so far.
    #[must_use]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Number of bytes left to read.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    /// Run `f` with a `clap_istream` that reads this stream. The raw stream
    /// is valid for the call only.
    pub(crate) fn with_raw<R>(&mut self, f: impl FnOnce(*const clap_istream) -> R) -> R {
        let raw = clap_istream {
            ctx: ptr::from_mut(self).cast::<c_void>(),
            read: Some(istream_read),
        };
        f(&raw)
    }

    /// As [`OutputStream::raw`], for a read stream.
    pub fn raw(&mut self, raw: &mut Option<clap_istream>) -> *const clap_istream {
        let stored = raw.insert(clap_istream {
            ctx: ptr::from_mut(self).cast::<c_void>(),
            read: Some(istream_read),
        });
        ptr::from_ref(stored)
    }

    /// Test hook: park the cursor, so a callback's bounds check is exercised.
    #[doc(hidden)]
    pub fn set_position_for_test(&mut self, position: usize) {
        self.position = position;
    }
}

extern "C" fn istream_read(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    guard(-1, || {
        if stream.is_null() || (buffer.is_null() && size != 0) {
            return -1;
        }
        // SAFETY: `ctx` is the `&mut InputStream` the stream was built from
        // (`with_raw`/`raw`), which outlives the call it is handed to.
        let input = unsafe { &mut *(*stream).ctx.cast::<InputStream<'_>>() };
        let remaining = input.data.len() - input.position;
        let to_read = (size as usize).min(remaining);
        if to_read == 0 {
            return 0;
        }
        let source = &input.data[input.position..input.position + to_read];
        // SAFETY: the plugin guarantees `buffer` addresses `size >= to_read`
        // writable bytes for the duration of the call (`stream.h`).
        let dest = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), to_read) };
        dest.copy_from_slice(source);
        input.position += to_read;
        to_read as i64
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic inside a real stream callback answers CLAP's `-1` (error)
    /// instead of aborting. A position past the end makes the slice bounds
    /// check panic (unwinding), in release builds too.
    ///
    /// Mutation: drop the `guard` from `istream_read`: the panic crosses the
    /// `extern "C"` boundary and the test process aborts.
    #[test]
    fn a_panic_inside_a_stream_callback_answers_an_error() {
        let bytes = [1u8, 2, 3];
        let mut stream = InputStream::new(&bytes);
        stream.position = bytes.len() + 1;
        let mut slot = None;
        let raw = stream.raw(&mut slot);
        let mut out = [0u8; 4];
        // SAFETY: `raw` addresses the live stream above; `out` is 4 writable bytes.
        let got = unsafe { ((*raw).read.unwrap())(raw, out.as_mut_ptr().cast(), 4) };
        assert_eq!(got, -1);
    }

    /// The bytes a plugin writes arrive in order.
    ///
    /// Mutation: make `ostream_write` overwrite instead of extend: the second
    /// write replaces the first and the assertion fails.
    #[test]
    fn writes_accumulate() {
        let mut stream = OutputStream::new();
        stream.with_raw(|raw| {
            for chunk in [&[1u8, 2][..], &[3u8][..]] {
                // SAFETY: `raw` is the live stream `with_raw` built; `chunk` is readable.
                let n = unsafe {
                    ((*raw).write.unwrap())(raw, chunk.as_ptr().cast(), chunk.len() as u64)
                };
                assert_eq!(n, chunk.len() as i64);
            }
        });
        assert_eq!(stream.data(), [1, 2, 3]);
    }
}
