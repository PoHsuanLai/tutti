//! The sub-chunks of a LIST chunk: their headers, bounded by the list, and the
//! pad byte after an odd-sized one (RIFF pads every chunk to an even length).

use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::error::Error;
use crate::four_cc::FourCC;
use crate::read_counter::ReadCounter;

/// The header of the next sub-chunk of a list that ends `end` bytes after the
/// counter's start: its id and its size.
///
/// # Errors
///
/// [`Error::InvalidChunkSize`] when not even a header fits in what is left of
/// the list, or the chunk's size reaches past the list's end.
pub(crate) fn next_chunk<R: Read>(
    reader: &mut ReadCounter<'_, R>,
    end: usize,
) -> Result<(FourCC, usize), Error> {
    if end.saturating_sub(reader.bytes_read()) < 8 {
        return Err(Error::InvalidChunkSize(*b"????"));
    }
    let id = reader.read_four_cc()?;
    let size = reader.read_u32()? as usize;
    if size > end.saturating_sub(reader.bytes_read()) {
        return Err(Error::InvalidChunkSize(*id.as_bytes()));
    }
    Ok((id, size))
}

/// Reads over the pad byte after a chunk of `size` bytes, if the size is odd.
/// A list that ends where the chunk does (a writer that left the pad out) is
/// accepted.
pub(crate) fn skip_pad<R: Read>(
    reader: &mut ReadCounter<'_, R>,
    size: usize,
    end: usize,
) -> Result<(), Error> {
    if size % 2 == 1 && reader.bytes_read() < end {
        reader.discard_data(1)?;
    }
    Ok(())
}

/// Checks that a chunk with a fixed size has it.
pub(crate) fn expect_size(id: FourCC, size: usize, expected: usize) -> Result<(), Error> {
    if size == expected {
        Ok(())
    } else {
        Err(Error::InvalidChunkSize(*id.as_bytes()))
    }
}
