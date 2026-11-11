use std::io::{self, Read};

use crate::four_cc::FourCC;

/// The fixed-width little-endian reads a RIFF file is made of.
pub(crate) trait ReadExt: Read {
    fn read_fixed<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut data = [0; N];
        self.read_exact(&mut data)?;
        Ok(data)
    }

    fn read_i8(&mut self) -> io::Result<i8> {
        self.read_fixed().map(i8::from_le_bytes)
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        self.read_fixed().map(u8::from_le_bytes)
    }

    fn read_i16(&mut self) -> io::Result<i16> {
        self.read_fixed().map(i16::from_le_bytes)
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        self.read_fixed().map(u16::from_le_bytes)
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        self.read_fixed().map(u32::from_le_bytes)
    }

    fn read_four_cc(&mut self) -> io::Result<FourCC> {
        self.read_fixed().map(FourCC::from_bytes)
    }

    /// A NUL-padded ASCII string of `length` bytes. Bytes outside tab to `~`
    /// read as `?`.
    fn read_fixed_length_string(&mut self, length: usize) -> io::Result<String> {
        let data = self.read_bytes(length)?;
        Ok(data
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| if (9..=126).contains(&byte) { byte } else { b'?' } as char)
            .collect())
    }

    /// Reads `size` bytes and drops them.
    fn discard_data(&mut self, size: usize) -> io::Result<()> {
        self.read_bytes(size).map(drop)
    }

    /// `size` bytes of 16-bit little-endian samples. A trailing odd byte is
    /// read and dropped.
    fn read_wave_data(&mut self, size: usize) -> io::Result<Vec<i16>> {
        const CHUNK: usize = 8192;
        // Not sized by `size`: it is the file's claim, and the stream may be
        // shorter than it.
        let mut samples = Vec::with_capacity((size / 2).min(CHUNK));
        let mut buffer = [0; CHUNK];
        let mut remaining = size;
        while remaining > 0 {
            let chunk = &mut buffer[..remaining.min(CHUNK)];
            self.read_exact(chunk)?;
            samples.extend(
                chunk
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| i16::from_le_bytes(pair)),
            );
            remaining -= chunk.len();
        }
        Ok(samples)
    }

    fn read_bytes(&mut self, size: usize) -> io::Result<Vec<u8>> {
        // Read up to `size` and check what came, instead of allocating `size`
        // up front: it is the file's claim, up to 4 GiB.
        let mut data = Vec::new();
        self.take(size as u64).read_to_end(&mut data)?;
        if data.len() < size {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(data)
    }
}

impl<R: Read + ?Sized> ReadExt for R {}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, ErrorKind};

    use super::*;

    #[test]
    fn wave_data_is_little_endian_pairs() {
        let mut bytes = Cursor::new([0x01, 0x02, 0xFF, 0xFF]);
        assert_eq!(bytes.read_wave_data(4).unwrap(), [0x0201, -1]);
    }

    #[test]
    fn an_odd_trailing_byte_is_read_and_dropped() {
        let mut bytes = Cursor::new([1, 0, 2, 0, 9, 0xAA]);
        assert_eq!(bytes.read_wave_data(5).unwrap(), [1, 2]);
        assert_eq!(bytes.position(), 5, "the odd byte is consumed");
    }

    #[test]
    fn wave_data_longer_than_one_read_chunk_keeps_every_sample() {
        let count = 8192 + 100;
        let bytes: Vec<u8> = (0..count as u16).flat_map(u16::to_le_bytes).collect();
        let samples = Cursor::new(bytes).read_wave_data(count * 2).unwrap();
        assert_eq!(samples.len(), count);
        assert!(
            samples
                .iter()
                .enumerate()
                .all(|(i, &s)| s as u16 == i as u16)
        );
    }

    #[test]
    fn wave_data_past_the_end_of_the_stream_is_unexpected_eof() {
        let error = Cursor::new([0; 6]).read_wave_data(8).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_fixed_length_string_stops_at_the_nul_and_masks_control_bytes() {
        let mut bytes = Cursor::new([b'a', 0x01, b'b', 0, b'x']);
        assert_eq!(bytes.read_fixed_length_string(5).unwrap(), "a?b");
    }
}
