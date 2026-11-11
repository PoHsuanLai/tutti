//! IBStream COM implementation backed by an in-memory `Cursor<Vec<u8>>`.

use crate::guard::{Out, guard};
use std::ffi::c_void;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use parking_lot::Mutex;
use vst3::Steinberg::{
    IBStream,
    IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet},
    IBStreamTrait, kInternalError, kInvalidArgument, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

use crate::sdk::sdk_enum_i32;

/// The most a stream may hold (1 GiB): a plugin's state is far smaller, and a
/// seek then a write otherwise zero-fills up to any position a plugin names.
const MAX_STREAM_BYTES: u64 = 1 << 30;

/// COM-wrapped in-memory stream.
///
/// Construct via [`BStream::new`] or [`BStream::from_data`]; the returned
/// [`ComWrapper`] is the owned reference (it holds the object's initial +1
/// refcount) and can be converted to a raw `FUnknown`-compatible pointer
/// via `ComWrapper::as_com_ref`.
pub(crate) struct BStream {
    cursor: Mutex<Cursor<Vec<u8>>>,
}

impl Class for BStream {
    type Interfaces = (IBStream,);
}

impl BStream {
    /// An empty stream.
    pub(crate) fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            cursor: Mutex::new(Cursor::new(Vec::new())),
        })
    }

    /// A stream positioned at the start of `data`.
    pub(crate) fn from_data(data: Vec<u8>) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            cursor: Mutex::new(Cursor::new(data)),
        })
    }

    /// A copy of everything in the stream, whatever the position.
    pub(crate) fn data(&self) -> Vec<u8> {
        self.cursor.lock().get_ref().clone()
    }
}

impl IBStreamTrait for BStream {
    unsafe fn read(
        &self,
        buffer: *mut c_void,
        num_bytes: i32,
        num_bytes_read: *mut i32,
    ) -> tresult {
        // SAFETY: a COM out-parameter: null or valid for this call.
        let read = unsafe { Out::new(num_bytes_read) };
        read.set(0);
        guard(kInternalError, || {
            let Ok(len) = usize::try_from(num_bytes) else {
                return kInvalidArgument;
            };
            if len == 0 {
                return kResultOk;
            }
            if buffer.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: `buffer` is non-null and `len` positive (both checked
            // above); the ABI makes it valid for `len` writable bytes for the
            // call, and the plugin does not alias it while this method runs.
            let buf = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), len) };
            match self.cursor.lock().read(buf) {
                Ok(n) => {
                    read.set(n as i32);
                    kResultOk
                }
                Err(_) => kInvalidArgument,
            }
        })
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> tresult {
        // SAFETY: a COM out-parameter: null or valid for this call.
        let written = unsafe { Out::new(num_bytes_written) };
        written.set(0);
        guard(kInternalError, || {
            let Ok(len) = usize::try_from(num_bytes) else {
                return kInvalidArgument;
            };
            if len == 0 {
                return kResultOk;
            }
            if buffer.is_null() {
                return kInvalidArgument;
            }
            let mut cursor = self.cursor.lock();
            // `Cursor<Vec<u8>>` zero-fills up to a seek position on write, so an
            // unbounded position is an unbounded allocation.
            if cursor.position().saturating_add(len as u64) > MAX_STREAM_BYTES {
                return kInvalidArgument;
            }
            // SAFETY: `buffer` is non-null and `len` positive (both checked
            // above); the ABI makes it valid for `len` readable bytes for the
            // call.
            let buf = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), len) };
            match cursor.write(buf) {
                Ok(n) => {
                    written.set(n as i32);
                    kResultOk
                }
                Err(_) => kInvalidArgument,
            }
        })
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        // SAFETY: a COM out-parameter: null or valid for this call.
        let result = unsafe { Out::new(result) };
        guard(kInternalError, || {
            let seek_from = match mode {
                m if m == sdk_enum_i32(kIBSeekSet) => match u64::try_from(pos) {
                    Ok(start) if start <= MAX_STREAM_BYTES => SeekFrom::Start(start),
                    // Past the cap no write could land: refuse it here too, so
                    // `tell` never reports a position the stream cannot hold.
                    Ok(_) => return kInvalidArgument,
                    // A negative absolute position is not a position (cast to
                    // `u64` it would wrap to about 2^64 and answer `kResultOk`).
                    Err(_) => return kInvalidArgument,
                },
                m if m == sdk_enum_i32(kIBSeekCur) => SeekFrom::Current(pos),
                m if m == sdk_enum_i32(kIBSeekEnd) => SeekFrom::End(pos),
                _ => return kInvalidArgument,
            };
            match self.cursor.lock().seek(seek_from) {
                Ok(new_pos) => {
                    result.set(new_pos as i64);
                    kResultOk
                }
                Err(_) => kInvalidArgument,
            }
        })
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        // SAFETY: a COM out-parameter: null or valid for this call.
        let pos = unsafe { Out::new(pos) };
        guard(kInternalError, || {
            pos.set(self.cursor.lock().position() as i64);
            kResultOk
        })
    }
}

opaque_debug!(BStream);

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;

    fn with_ptr<R>(stream: &ComWrapper<BStream>, f: impl FnOnce(ComPtr<IBStream>) -> R) -> R {
        f(stream.to_com_ptr::<IBStream>().unwrap())
    }

    #[test]
    fn test_stream_write_read() {
        let stream = BStream::new();
        let data = b"Hello, VST3!";
        with_ptr(&stream, |ptr| {
            let mut written = 0i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe {
                ptr.write(
                    data.as_ptr() as *mut c_void,
                    data.len() as i32,
                    &mut written,
                )
            };
            assert_eq!(result, kResultOk);
            assert_eq!(written, data.len() as i32);

            // `tell` must report the cursor the write left behind. A plugin
            // saving state writes a chunk and then asks where it landed to
            // size the next one, so a `tell` stuck at a constant silently
            // corrupts every multi-chunk save. Nothing else in the crate
            // calls it, so this is its only cover.
            let mut pos = -1i64;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.tell(&mut pos) };
            assert_eq!(result, kResultOk);
            assert_eq!(pos, data.len() as i64, "tell must follow the write");

            let mut new_pos = 0i64;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.seek(0, sdk_enum_i32(kIBSeekSet), &mut new_pos) };
            assert_eq!(result, kResultOk);
            assert_eq!(new_pos, 0);

            // ...and follow a seek back to the start, so a `tell` that merely
            // returned the stream length would fail here.
            let mut pos = -1i64;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.tell(&mut pos) }, kResultOk);
            assert_eq!(pos, 0, "tell must follow the seek");

            let mut buffer = [0u8; 32];
            let mut bytes_read = 0i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe {
                ptr.read(
                    buffer.as_mut_ptr() as *mut c_void,
                    buffer.len() as i32,
                    &mut bytes_read,
                )
            };
            assert_eq!(result, kResultOk);
            assert_eq!(bytes_read, data.len() as i32);
            assert_eq!(&buffer[..data.len()], data);

            // The read advanced the cursor to the end, not past it: the buffer
            // was 32 bytes and only 12 were there to read.
            let mut pos = -1i64;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.tell(&mut pos) }, kResultOk);
            assert_eq!(pos, data.len() as i64, "tell must follow the read");
        });
    }

    /// A negative absolute seek is refused, and leaves the position where it was.
    #[test]
    fn test_stream_seek_set_negative_is_invalid() {
        let stream = BStream::from_data(vec![1, 2, 3]);
        let ptr = stream.as_com_ref::<IBStream>().unwrap();
        let mut new_pos: i64 = 7;
        // SAFETY: `new_pos` is a valid out-parameter and the stream is live.
        let rc = unsafe { ptr.seek(-1, sdk_enum_i32(kIBSeekSet), &mut new_pos) };
        assert_eq!(rc, kInvalidArgument);
        assert_eq!(new_pos, 7, "the out-parameter is untouched on refusal");
        let mut pos: i64 = -1;
        // SAFETY: `pos` is a valid out-parameter and the stream is live.
        assert_eq!(unsafe { ptr.tell(&mut pos) }, kResultOk);
        assert_eq!(pos, 0);
    }

    #[test]
    fn test_stream_from_data() {
        let data = vec![1, 2, 3, 4, 5];
        let stream = BStream::from_data(data.clone());
        assert_eq!(stream.data(), data);
    }

    #[test]
    fn test_stream_read_negative_num_bytes() {
        let stream = BStream::from_data(vec![1, 2, 3]);
        with_ptr(&stream, |ptr| {
            let mut buf = [0u8; 8];
            let mut bytes_read = 99i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.read(buf.as_mut_ptr() as *mut c_void, -1, &mut bytes_read) };
            assert_ne!(result, kResultOk);
            assert_eq!(bytes_read, 0);
        });
    }

    #[test]
    fn test_stream_read_zero_num_bytes() {
        let stream = BStream::from_data(vec![1, 2, 3]);
        with_ptr(&stream, |ptr| {
            let mut buf = [0u8; 8];
            let mut bytes_read = 99i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.read(buf.as_mut_ptr() as *mut c_void, 0, &mut bytes_read) };
            assert_eq!(result, kResultOk);
            assert_eq!(bytes_read, 0);
        });
    }

    #[test]
    fn test_stream_read_null_buffer() {
        let stream = BStream::from_data(vec![1, 2, 3]);
        with_ptr(&stream, |ptr| {
            let mut bytes_read = 99i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.read(std::ptr::null_mut(), 10, &mut bytes_read) };
            assert_ne!(result, kResultOk);
            assert_eq!(bytes_read, 0);
        });
    }

    #[test]
    fn test_stream_write_negative_num_bytes() {
        let stream = BStream::new();
        let data = [1u8, 2, 3];
        with_ptr(&stream, |ptr| {
            let mut bytes_written = 99i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.write(data.as_ptr() as *mut c_void, -5, &mut bytes_written) };
            assert_ne!(result, kResultOk);
            assert_eq!(bytes_written, 0);
        });
        assert!(stream.data().is_empty());
    }

    #[test]
    fn test_stream_write_zero_num_bytes() {
        let stream = BStream::new();
        let data = [1u8, 2, 3];
        with_ptr(&stream, |ptr| {
            let mut bytes_written = 99i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.write(data.as_ptr() as *mut c_void, 0, &mut bytes_written) };
            assert_eq!(result, kResultOk);
            assert_eq!(bytes_written, 0);
        });
    }

    #[test]
    fn test_stream_write_null_buffer() {
        let stream = BStream::new();
        with_ptr(&stream, |ptr| {
            let mut bytes_written = 99i32;
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result = unsafe { ptr.write(std::ptr::null_mut(), 10, &mut bytes_written) };
            assert_ne!(result, kResultOk);
            assert_eq!(bytes_written, 0);
        });
    }

    #[test]
    fn test_stream_read_null_bytes_read_pointer() {
        let stream = BStream::from_data(vec![1, 2, 3]);
        with_ptr(&stream, |ptr| {
            let mut buf = [0u8; 8];
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let result =
                unsafe { ptr.read(buf.as_mut_ptr() as *mut c_void, 3, std::ptr::null_mut()) };
            assert_eq!(result, kResultOk);
            assert_eq!(&buf[..3], &[1, 2, 3]);
        });
    }

    /// **A plugin cannot make the host allocate past the cap by seeking far and
    /// writing.** `Cursor<Vec<u8>>` zero-fills up to the position, so a seek to
    /// 1 TiB and a one-byte write used to ask for a terabyte.
    #[test]
    fn a_far_seek_then_a_write_is_refused() {
        let stream = BStream::new();
        with_ptr(&stream, |ptr| {
            let mut at = 0i64;
            // SAFETY: COM methods on a live object the test owns, with valid arguments.
            unsafe {
                assert_eq!(
                    ptr.seek(1 << 40, sdk_enum_i32(kIBSeekSet), &mut at),
                    kInvalidArgument
                );
                // The cap itself is a position; one more byte there is not room.
                assert_eq!(
                    ptr.seek(MAX_STREAM_BYTES as i64, sdk_enum_i32(kIBSeekSet), &mut at),
                    kResultOk
                );
                let mut written = 0i32;
                assert_eq!(
                    ptr.write(b"x".as_ptr() as *mut c_void, 1, &mut written),
                    kInvalidArgument
                );
                assert_eq!(written, 0);
            }
        });
        assert!(stream.data().len() < 1 << 20, "nothing was allocated");
    }
}
