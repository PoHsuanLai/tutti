//! A sink that fills up, for the tests.

use std::io::{self, Cursor, Seek, SeekFrom, Write};

/// Memory that holds at most `capacity` bytes: a write that would grow it past
/// that fails with `StorageFull`, as a full disk does, while one that
/// overwrites bytes already there (a header back-patch) succeeds.
pub(crate) struct Full {
    inner: Cursor<Vec<u8>>,
    capacity: usize,
}

impl Full {
    pub(crate) fn holding(capacity: usize) -> Self {
        Self {
            inner: Cursor::new(Vec::new()),
            capacity,
        }
    }

    #[cfg_attr(not(feature = "wav"), allow(dead_code))]
    pub(crate) fn bytes(&self) -> &[u8] {
        self.inner.get_ref()
    }
}

impl Write for Full {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self.inner.position() as usize + buf.len();
        if end > self.capacity {
            return Err(io::ErrorKind::StorageFull.into());
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Full {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}
