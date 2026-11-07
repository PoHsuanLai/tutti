//! Bevy `AssetLoader` for [`WaveAsset`].
//!
//! The **resident** tier only: the whole file is read into memory and decoded
//! up front, so this is for samples short enough to hold whole. A clip too long
//! for that goes through the disk streamer instead, which never becomes an
//! asset. The asset is [`WaveAsset`]; this is its loader.

use std::sync::Arc;

use bevy_asset::{AssetLoader, LoadContext, io::Reader};
use bevy_reflect::TypePath;
use tutti_codec::{DecodeError, Wave};

/// A decoded [`Wave`] as a Bevy asset.
///
/// Wraps an `Arc<Wave>`, so cloning shares the samples; derefs to [`Wave`].
#[derive(Clone, bevy_asset::Asset, TypePath, Debug)]
pub struct WaveAsset(pub Arc<Wave>);

impl core::ops::Deref for WaveAsset {
    type Target = Wave;
    fn deref(&self) -> &Wave {
        &self.0
    }
}

impl WaveAsset {
    /// File extensions the loader recognises.
    pub const EXTENSIONS: &'static [&'static str] = &["wav", "flac", "mp3", "ogg"];

    /// Decodes a complete file held in memory. The container is identified
    /// from the bytes alone.
    ///
    /// # Errors
    ///
    /// As `Wave::read`.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, DecodeError> {
        Wave::from_bytes(bytes).map(|w| Self(Arc::new(w)))
    }
}

/// In-memory loader for [`WaveAsset`]. Reads the entire payload, then
/// delegates to [`WaveAsset::from_bytes`].
#[derive(Default, TypePath, Debug)]
pub struct WaveAssetLoader;

/// Why a `.wav` failed to load.
///
/// `#[non_exhaustive]`: match with a `_` arm, since a future decoder may add
/// variants.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WaveAssetLoaderError {
    /// The bytes could not be read off the asset source.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// The bytes were read but are not a wave this decoder accepts.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

impl AssetLoader for WaveAssetLoader {
    type Asset = WaveAsset;
    type Settings = ();
    type Error = WaveAssetLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        WaveAsset::from_bytes(bytes).map_err(WaveAssetLoaderError::Decode)
    }

    fn extensions(&self) -> &[&str] {
        WaveAsset::EXTENSIONS
    }
}
