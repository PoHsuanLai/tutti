//! Fluent builder for instantiating [`SoundFontUnit`]s from a
//! [`SoundFontSystem`](super::SoundFontSystem) by file path.

use super::{SoundFontSystem, SoundFontUnit};
use crate::error::Result;
use std::path::PathBuf;

/// Starts an [`Sf2Builder`] from a shared [`SoundFontSystem`] and a path to
/// an `.sf2` file.
///
/// ```ignore
/// let unit = tutti_synth::sf2(&engine.soundfont, "piano.sf2")
///     .preset(0)
///     .build()?;
/// ```
pub fn sf2(system: &SoundFontSystem, path: impl Into<PathBuf>) -> Sf2Builder<'_> {
    Sf2Builder::new(system, path.into())
}

/// Fluent builder for SoundFont-based synthesis.
///
/// Holds a borrow of the [`SoundFontSystem`] so the underlying file cache
/// is shared across builders; construct via [`sf2`].
pub struct Sf2Builder<'a> {
    system: &'a SoundFontSystem,
    path: PathBuf,
    preset: i32,
    channel: i32,
}

impl<'a> Sf2Builder<'a> {
    fn new(system: &'a SoundFontSystem, path: PathBuf) -> Self {
        Self {
            system,
            path,
            preset: 0,
            channel: 0,
        }
    }

    /// Selects the SoundFont preset to play.
    ///
    /// Default: `0` (piano on most General MIDI SoundFonts).
    pub fn preset(mut self, preset: i32) -> Self {
        self.preset = preset;
        self
    }

    /// Selects the MIDI channel (0–15) the preset is assigned to.
    ///
    /// Default: `0`.
    pub fn channel(mut self, channel: i32) -> Self {
        self.channel = channel;
        self
    }

    /// Builds the [`SoundFontUnit`], loading and caching the file on the
    /// [`SoundFontSystem`] if it isn't already cached, then issues a program
    /// change for the configured preset and channel.
    pub fn build(self) -> Result<SoundFontUnit> {
        let handle = self.system.load(&self.path)?;

        let soundfont = self.system.get(&handle).ok_or_else(|| {
            crate::error::Error::SoundFont("SoundFont not found in cache".to_string())
        })?;

        let settings = self.system.default_settings();
        let mut unit = SoundFontUnit::new(soundfont, &settings)?;
        unit.program_change(self.channel, self.preset);
        Ok(unit)
    }
}
