//! AIFF encoder (hand-rolled IFF chunks + 80-bit IEEE 754 extended sample
//! rate). No streaming support — AIFF requires total size up front.

use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::process::ProcessedAudio;
use std::io::Write;

pub(crate) fn encode(audio: ProcessedAudio, request: &EncodeRequest<'_>) -> Result<()> {
    if request.bit_depth == BitDepth::Float32 {
        return Err(Error::UnsupportedFormat(
            "AIFF does not support 32-bit float (use AIFF-C for float)".into(),
        ));
    }

    let channels: Vec<&[f32]> = match &audio {
        ProcessedAudio::Stereo { left, right } => vec![left, right],
        ProcessedAudio::Mono(samples) => vec![samples],
    };

    write_aiff(request, &channels)
}

fn write_aiff(request: &EncodeRequest<'_>, channels: &[&[f32]]) -> Result<()> {
    let num_channels = channels.len() as u16;
    let num_frames = channels[0].len() as u32;
    let bits = request.bit_depth.bits();
    let bytes_per_sample = (bits as u32).div_ceil(8);

    let sound_data_size = num_frames * num_channels as u32 * bytes_per_sample;
    let ssnd_chunk_size = 8 + sound_data_size; // offset(4) + blockSize(4) + data
    let comm_chunk_size: u32 = 18;
    let form_size: u32 = 4 + (8 + comm_chunk_size) + (8 + ssnd_chunk_size);

    let mut file = std::fs::File::create(request.path)?;

    file.write_all(b"FORM")?;
    file.write_all(&form_size.to_be_bytes())?;
    file.write_all(b"AIFF")?;

    file.write_all(b"COMM")?;
    file.write_all(&comm_chunk_size.to_be_bytes())?;
    file.write_all(&num_channels.to_be_bytes())?;
    file.write_all(&num_frames.to_be_bytes())?;
    file.write_all(&bits.to_be_bytes())?;
    file.write_all(&f64_to_ieee_extended(request.sample_rate as f64))?;

    file.write_all(b"SSND")?;
    file.write_all(&ssnd_chunk_size.to_be_bytes())?;
    file.write_all(&0u32.to_be_bytes())?; // offset
    file.write_all(&0u32.to_be_bytes())?; // block size

    for frame in 0..num_frames as usize {
        for ch in channels {
            let sample = ch[frame];
            match request.bit_depth {
                BitDepth::Int16 => {
                    file.write_all(&f32_to_i16(sample).to_be_bytes())?;
                }
                BitDepth::Int24 => {
                    let bytes = f32_to_i24(sample).to_be_bytes();
                    file.write_all(&bytes[1..4])?; // top 3 bytes of i32
                }
                BitDepth::Float32 => unreachable!(),
            }
        }
    }

    Ok(())
}

/// Convert f64 to 80-bit IEEE 754 extended precision (big-endian).
fn f64_to_ieee_extended(value: f64) -> [u8; 10] {
    if value == 0.0 {
        return [0u8; 10];
    }

    let mut result = [0u8; 10];
    let negative = value < 0.0;
    let val = value.abs();

    let bits = val.to_bits();
    let exponent_f64 = ((bits >> 52) & 0x7FF) as i32 - 1023;
    let mantissa_f64 = bits & 0x000F_FFFF_FFFF_FFFF;

    let exponent_ext = (exponent_f64 + 16383) as u16;
    let sign_exp = if negative {
        0x8000 | exponent_ext
    } else {
        exponent_ext
    };

    result[0] = (sign_exp >> 8) as u8;
    result[1] = (sign_exp & 0xFF) as u8;

    let mantissa_ext: u64 = 0x8000_0000_0000_0000 | (mantissa_f64 << 11);

    result[2] = (mantissa_ext >> 56) as u8;
    result[3] = (mantissa_ext >> 48) as u8;
    result[4] = (mantissa_ext >> 40) as u8;
    result[5] = (mantissa_ext >> 32) as u8;
    result[6] = (mantissa_ext >> 24) as u8;
    result[7] = (mantissa_ext >> 16) as u8;
    result[8] = (mantissa_ext >> 8) as u8;
    result[9] = mantissa_ext as u8;

    result
}

#[cfg(test)]
fn ieee_extended_to_f64(bytes: &[u8; 10]) -> f64 {
    let sign_exp = ((bytes[0] as u16) << 8) | bytes[1] as u16;
    let sign = (sign_exp & 0x8000) != 0;
    let exponent = (sign_exp & 0x7FFF) as i32;

    let mut mantissa: u64 = 0;
    for &b in &bytes[2..10] {
        mantissa = (mantissa << 8) | b as u64;
    }

    if exponent == 0 && mantissa == 0 {
        return 0.0;
    }

    let f = mantissa as f64 / (1u64 << 63) as f64;
    let val = f * 2.0f64.powi(exponent - 16383);
    if sign {
        -val
    } else {
        val
    }
}

#[inline]
fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

#[inline]
fn f32_to_i24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8388607.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ieee_extended_roundtrip() {
        for &rate in &[44100.0, 48000.0, 96000.0, 22050.0] {
            let encoded = f64_to_ieee_extended(rate);
            let decoded = ieee_extended_to_f64(&encoded);
            assert!(
                (decoded - rate).abs() < 0.01,
                "roundtrip failed for {rate}: got {decoded}"
            );
        }
    }

    #[test]
    fn aiff_file_has_form_aiff_comm_ssnd() {
        use crate::Export;

        let left = vec![0.0f32, 0.5, -0.5, 0.25];
        let right = vec![0.1, -0.1, 0.0, 0.75];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.aiff");
        Export::buffers(left, right, 44100.0)
            .bit_depth(BitDepth::Int16)
            .to_file(&path)
            .run()
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"FORM");
        assert_eq!(&bytes[8..12], b"AIFF");
        assert_eq!(&bytes[12..16], b"COMM");

        let comm_size = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let ssnd_offset = 12 + 8 + comm_size as usize;
        assert_eq!(&bytes[ssnd_offset..ssnd_offset + 4], b"SSND");

        // First sample (left) should be 0 — 0.0f32 → 0i16.
        let data_offset = ssnd_offset + 8 + 8;
        let first_l = i16::from_be_bytes([bytes[data_offset], bytes[data_offset + 1]]);
        assert_eq!(first_l, 0);
    }

    #[test]
    fn aiff_rejects_float32() {
        use crate::Export;

        let left = vec![0.0; 10];
        let right = vec![0.0; 10];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.aiff");
        let result = Export::buffers(left, right, 44100.0)
            .bit_depth(BitDepth::Float32)
            .to_file(&path)
            .run();
        assert!(result.is_err());
    }
}
