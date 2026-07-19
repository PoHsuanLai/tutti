//! IBStream COM implementation backed by an in-memory `Cursor<Vec<u8>>`.

use std::ffi::c_void;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use parking_lot::Mutex;
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, tresult, IBStream, IBStreamTrait,
    IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet},
};
use vst3::{Class, ComWrapper};

/// COM-wrapped in-memory stream.
///
/// Construct via [`BStream::new`] or [`BStream::from_data`]; the returned
/// [`ComWrapper`] is the owned reference (it holds the object's initial +1
/// refcount) and can be converted to a raw `FUnknown`-compatible pointer
/// via [`BStream::as_com_ptr`].
pub struct BStream {
    cursor: Mutex<Cursor<Vec<u8>>>,
}

impl Class for BStream {
    type Interfaces = (IBStream,);
}

impl BStream {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            cursor: Mutex::new(Cursor::new(Vec::new())),
        })
    }

    pub fn from_data(data: Vec<u8>) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            cursor: Mutex::new(Cursor::new(data)),
        })
    }

    pub fn data(&self) -> Vec<u8> {
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
        if num_bytes == 0 {
            if !num_bytes_read.is_null() {
                *num_bytes_read = 0;
            }
            return kResultOk;
        }
        if num_bytes < 0 || buffer.is_null() {
            if !num_bytes_read.is_null() {
                *num_bytes_read = 0;
            }
            return kInvalidArgument;
        }
        let mut cursor = self.cursor.lock();
        let buf = std::slice::from_raw_parts_mut(buffer as *mut u8, num_bytes as usize);
        match cursor.read(buf) {
            Ok(n) => {
                if !num_bytes_read.is_null() {
                    *num_bytes_read = n as i32;
                }
                kResultOk
            }
            Err(_) => kInvalidArgument,
        }
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> tresult {
        if num_bytes == 0 {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kResultOk;
        }
        if num_bytes < 0 || buffer.is_null() {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kInvalidArgument;
        }
        let mut cursor = self.cursor.lock();
        let buf = std::slice::from_raw_parts(buffer as *const u8, num_bytes as usize);
        match cursor.write(buf) {
            Ok(n) => {
                if !num_bytes_written.is_null() {
                    *num_bytes_written = n as i32;
                }
                kResultOk
            }
            Err(_) => kInvalidArgument,
        }
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        let seek_from = match mode {
            m if m == kIBSeekSet as i32 => SeekFrom::Start(pos as u64),
            m if m == kIBSeekCur as i32 => SeekFrom::Current(pos),
            m if m == kIBSeekEnd as i32 => SeekFrom::End(pos),
            _ => return kInvalidArgument,
        };
        let mut cursor = self.cursor.lock();
        match cursor.seek(seek_from) {
            Ok(new_pos) => {
                if !result.is_null() {
                    *result = new_pos as i64;
                }
                kResultOk
            }
            Err(_) => kInvalidArgument,
        }
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        let cursor = self.cursor.lock();
        if !pos.is_null() {
            *pos = cursor.position() as i64;
        }
        kResultOk
    }
}

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
            let result = unsafe {
                ptr.write(
                    data.as_ptr() as *mut c_void,
                    data.len() as i32,
                    &mut written,
                )
            };
            assert_eq!(result, kResultOk);
            assert_eq!(written, data.len() as i32);

            let mut new_pos = 0i64;
            let result = unsafe { ptr.seek(0, kIBSeekSet as i32, &mut new_pos) };
            assert_eq!(result, kResultOk);
            assert_eq!(new_pos, 0);

            let mut buffer = [0u8; 32];
            let mut bytes_read = 0i32;
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
        });
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
            let result =
                unsafe { ptr.read(buf.as_mut_ptr() as *mut c_void, 3, std::ptr::null_mut()) };
            assert_eq!(result, kResultOk);
            assert_eq!(&buf[..3], &[1, 2, 3]);
        });
    }
}
