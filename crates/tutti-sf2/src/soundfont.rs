use std::io::Read;

use crate::LoopMode;
use crate::binary_reader::ReadExt;
use crate::error::SoundFontError;
use crate::four_cc::FourCC;
use crate::instrument::Instrument;
use crate::preset::Preset;
use crate::sample_header::SampleHeader;
use crate::soundfont_info::SoundFontInfo;
use crate::soundfont_parameters::SoundFontParameters;
use crate::soundfont_sampledata::SoundFontSampleData;

/// A parsed SoundFont (.sf2): its samples, instruments and presets.
#[derive(Debug)]
#[non_exhaustive]
pub struct SoundFont {
    pub(crate) info: SoundFontInfo,
    pub(crate) bits_per_sample: i32,
    pub(crate) wave_data: Vec<i16>,
    pub(crate) sample_headers: Vec<SampleHeader>,
    pub(crate) presets: Vec<Preset>,
    pub(crate) instruments: Vec<Instrument>,
}

impl SoundFont {
    /// Reads a SoundFont from `reader`.
    ///
    /// # Errors
    ///
    /// Returns [`SoundFontError`] if the stream cannot be read, is not a
    /// SoundFont 2 file, has compressed samples, or has a region that reaches
    /// outside the sample data.
    pub fn new<R: Read>(reader: &mut R) -> Result<Self, SoundFontError> {
        let chunk_id = reader.read_four_cc()?;
        if chunk_id != b"RIFF" {
            return Err(SoundFontError::RiffChunkNotFound);
        }

        let _size = reader.read_i32()?;

        let form_type = reader.read_four_cc()?;
        if form_type != b"sfbk" {
            return Err(SoundFontError::InvalidRiffChunkType {
                expected: FourCC::from_bytes(*b"sfbk"),
                actual: form_type,
            });
        }

        let info = SoundFontInfo::new(reader)?;
        let sample_data = SoundFontSampleData::new(reader)?;
        let parameters = SoundFontParameters::new(reader)?;

        let sound_font = Self {
            info,
            bits_per_sample: sample_data.bits_per_sample,
            wave_data: sample_data.wave_data,
            sample_headers: parameters.sample_headers,
            presets: parameters.presets,
            instruments: parameters.instruments,
        };

        sound_font.sanity_check()?;

        Ok(sound_font)
    }

    fn sanity_check(&self) -> Result<(), SoundFontError> {
        // https://github.com/sinshu/rustysynth/issues/22
        // https://github.com/sinshu/rustysynth/issues/33
        // https://github.com/sinshu/rustysynth/pull/51
        for instrument in &self.instruments {
            for region in &instrument.regions {
                let start = region.sample_start();
                let end = region.sample_end();
                let start_loop = region.sample_start_loop();
                let end_loop = region.sample_end_loop();
                let loop_mode = region.sample_modes();

                if start < 0
                    || start_loop < 0
                    || end as usize >= self.wave_data.len()
                    || end_loop as usize >= self.wave_data.len()
                    || end <= start
                    || end_loop < start_loop
                    || (loop_mode != LoopMode::NoLoop && start_loop >= end_loop)
                {
                    return Err(SoundFontError::SanityCheckFailed);
                }
            }
        }

        Ok(())
    }

    /// The file's INFO list.
    #[must_use]
    pub fn info(&self) -> &SoundFontInfo {
        &self.info
    }

    /// The bit depth of the sample data.
    #[must_use]
    pub fn bits_per_sample(&self) -> i32 {
        self.bits_per_sample
    }

    /// The 16-bit sample data every sample header indexes into.
    #[must_use]
    pub fn wave_data(&self) -> &[i16] {
        &self.wave_data[..]
    }

    /// The samples of the SoundFont.
    #[must_use]
    pub fn sample_headers(&self) -> &[SampleHeader] {
        &self.sample_headers[..]
    }

    /// The presets of the SoundFont.
    #[must_use]
    pub fn presets(&self) -> &[Preset] {
        &self.presets[..]
    }

    /// The instruments of the SoundFont.
    #[must_use]
    pub fn instruments(&self) -> &[Instrument] {
        &self.instruments[..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{fs::File, path::PathBuf};

    fn samples_dir_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("samples")
    }

    #[test]
    fn test_load_reject_sf3() {
        let path = samples_dir_path().join("dummy.sf3");
        let mut file = File::open(&path).unwrap();
        assert!(matches!(
            SoundFont::new(&mut file),
            Err(SoundFontError::UnsupportedSampleFormat)
        ));
    }

    // smpl sub-chunk exists, but is zero-length.
    #[test]
    fn test_load_empty_samples() {
        let path = samples_dir_path().join("test_empty_samples.sf2");
        let mut file = File::open(&path).unwrap();
        assert!(matches!(
            SoundFont::new(&mut file),
            Err(SoundFontError::SampleDataNotFound)
        ));
    }
}
