//! SoundFont assets and the systems that turn them into playing voices.

pub mod soundfont;

pub use soundfont::{
    promote_pending_soundfonts, soundfont_playback_system, PendingSoundFontUnit, PlaySoundFont,
    SoundFontAssetLoader, SoundFontAssetLoaderError, TuttiSoundFontPlugin,
};
