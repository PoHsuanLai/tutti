//! Broadcast WAV (BEXT) chunk injection.
//!
//! BWAV is a strict superset of WAV: a plain WAV file with one extra `bext`
//! chunk inserted right after the `WAVE` marker. This module is a post-
//! encode patch rather than a format in its own right — we let the WAV
//! encoder write a file normally, then re-open it and insert the chunk.

use crate::error::{Error, Result};
use crate::options::BroadcastWavMetadata;
use std::path::Path;

/// Insert a BEXT chunk into an existing WAV file at `path`.
pub(crate) fn inject(path: &Path, metadata: &BroadcastWavMetadata) -> Result<()> {
    let original = std::fs::read(path)?;
    if original.len() < 12 || &original[0..4] != b"RIFF" || &original[8..12] != b"WAVE" {
        return Err(Error::InvalidData("Not a valid WAV file".into()));
    }

    let bext_data = build_bext_chunk(metadata);
    let bext_chunk_size = bext_data.len() as u32;
    let bext_total = 8 + bext_data.len(); // "bext" + size(4) + data

    let old_riff_size = u32::from_le_bytes([original[4], original[5], original[6], original[7]]);
    let new_riff_size = old_riff_size + bext_total as u32;

    let mut output = Vec::with_capacity(original.len() + bext_total);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&new_riff_size.to_le_bytes());
    output.extend_from_slice(b"WAVE");
    output.extend_from_slice(b"bext");
    output.extend_from_slice(&bext_chunk_size.to_le_bytes());
    output.extend_from_slice(&bext_data);
    // Copy remaining chunks from original (everything after the WAVE identifier)
    output.extend_from_slice(&original[12..]);

    std::fs::write(path, &output)?;
    Ok(())
}

fn build_bext_chunk(meta: &BroadcastWavMetadata) -> Vec<u8> {
    let mut data = Vec::with_capacity(602);

    write_fixed_str(&mut data, "", 256); // Description
    write_fixed_str(&mut data, &meta.originator, 32); // Originator
    write_fixed_str(&mut data, &meta.originator_reference, 32); // OriginatorReference
    write_fixed_str(&mut data, &meta.origination_date, 10); // yyyy-mm-dd
    write_fixed_str(&mut data, &meta.origination_time, 8); // hh:mm:ss
    data.extend_from_slice(&meta.time_reference.to_le_bytes()); // sample count since midnight
    data.extend_from_slice(&2u16.to_le_bytes()); // Version 2 (loudness fields)
    data.extend(std::iter::repeat_n(0u8, 64)); // UMID
    data.extend_from_slice(&((meta.loudness_value * 100.0) as i16).to_le_bytes());
    data.extend_from_slice(&((meta.loudness_range * 100.0) as i16).to_le_bytes());
    data.extend_from_slice(&((meta.max_true_peak_level * 100.0) as i16).to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes()); // MaxMomentaryLoudness
    data.extend_from_slice(&0i16.to_le_bytes()); // MaxShortTermLoudness
    data.extend(std::iter::repeat_n(0u8, 180)); // Reserved

    data
}

fn write_fixed_str(buf: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    let copy_len = bytes.len().min(len);
    buf.extend_from_slice(&bytes[..copy_len]);
    buf.extend(std::iter::repeat_n(0u8, len - copy_len));
}

#[cfg(test)]
#[allow(dead_code)] // retained for ad-hoc test validation.
pub(crate) fn has_bext_chunk(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    let mut pos = 12; // skip RIFF header + WAVE
    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        if chunk_id == b"bext" {
            return true;
        }
        pos += 8 + chunk_size;
        if pos % 2 != 0 {
            pos += 1; // chunks are word-aligned
        }
    }
    false
}
