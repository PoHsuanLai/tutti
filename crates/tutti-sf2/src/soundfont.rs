use std::io::Read;

use crate::LoopMode;
use crate::binary_reader::ReadExt;
use crate::error::Error;
use crate::instrument::Instrument;
use crate::preset::Preset;
use crate::sample_header::SampleHeader;
use crate::soundfont_info::SoundFontInfo;
use crate::soundfont_parameters::SoundFontParameters;
use crate::soundfont_sampledata::{Pool, SamplePool, SoundFontSampleData};

/// A parsed SoundFont (.sf2): its samples, instruments and presets.
#[derive(Debug)]
#[non_exhaustive]
pub struct SoundFont {
    pub(crate) info: SoundFontInfo,
    pub(crate) pool: Pool,
    pub(crate) sample_headers: Vec<SampleHeader>,
    pub(crate) presets: Vec<Preset>,
    pub(crate) instruments: Vec<Instrument>,
}

impl SoundFont {
    /// Reads the SoundFont at `path`.
    ///
    /// The file is read through a buffer; there is nothing to flush or close.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be opened, and everything
    /// [`from_reader`](Self::from_reader) returns.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        Self::from_reader(std::io::BufReader::new(std::fs::File::open(path)?))
    }

    /// Reads a SoundFont from `reader`.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if the stream cannot be read, is not a
    /// SoundFont 2 file, has compressed samples, or has a region that reaches
    /// outside the sample data.
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self, Error> {
        let reader = &mut reader;
        let chunk_id = reader.read_four_cc()?;
        if chunk_id != b"RIFF" {
            return Err(Error::RiffChunkNotFound);
        }

        // The RIFF size counts the form type and the lists after it: nothing
        // past it is read, and a list that claims more than it is a short read.
        let size = reader.read_u32()?;
        let reader = &mut reader.take(u64::from(size));

        let form_type = reader.read_four_cc()?;
        if form_type != b"sfbk" {
            return Err(Error::InvalidRiffChunkType {
                expected: *b"sfbk",
                actual: *form_type.as_bytes(),
            });
        }

        let info = SoundFontInfo::new(reader)?;
        let sample_data = SoundFontSampleData::new(reader, info.version())?;
        let parameters = SoundFontParameters::new(reader)?;

        let sound_font = Self {
            info,
            pool: sample_data.pool,
            sample_headers: parameters.sample_headers,
            presets: parameters.presets,
            instruments: parameters.instruments,
        };

        sound_font.sanity_check()?;

        Ok(sound_font)
    }

    fn sanity_check(&self) -> Result<(), Error> {
        for instrument in &self.instruments {
            for region in &instrument.regions {
                let start = region.raw_sample_start();
                let end = region.raw_sample_end();
                let start_loop = region.raw_sample_start_loop();
                let end_loop = region.raw_sample_end_loop();
                let loop_mode = region.sample_modes();

                // Signed, 64 bits: a header's DWORD and an offset can take an
                // address below 0 or past 2^31, and either is refused.
                let len = self.pool.len() as i64;
                if start < 0
                    || start_loop < 0
                    || end >= len
                    || end_loop >= len
                    || end <= start
                    || end_loop < start_loop
                    || (loop_mode != LoopMode::NoLoop && start_loop >= end_loop)
                {
                    return Err(Error::SanityCheckFailed);
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

    /// The bit depth of the sample data: 24 when an `sm24` chunk counts, else 16.
    #[must_use]
    pub fn bits_per_sample(&self) -> u8 {
        match self.pool {
            Pool::Pcm16(_) => 16,
            Pool::Pcm24 { .. } => 24,
        }
    }

    /// The 16-bit `smpl` points every sample header indexes into: the high 16
    /// bits of each point of a 24-bit pool.
    #[must_use]
    pub fn wave_data(&self) -> &[i16] {
        self.pool.high()
    }

    /// The sample points as they play: 16-bit, or 24-bit with their low bytes.
    #[must_use]
    pub fn sample_pool(&self) -> SamplePool<'_> {
        self.pool.view()
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
            SoundFont::from_reader(&mut file),
            Err(Error::UnsupportedSampleFormat)
        ));
    }
}
