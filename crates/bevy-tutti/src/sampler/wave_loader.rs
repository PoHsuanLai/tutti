//! Bevy `AssetLoader` for [`WaveAsset`].
//!
//! The **resident** tier only: the whole file is read into memory and decoded
//! up front, so this is for samples short enough to hold whole. A clip too long
//! for that goes through the disk streamer instead, which never becomes an
//! asset. The asset type itself lives in `tutti-core`; this is only its loader.

use bevy_asset::{io::Reader, AssetLoader, LoadContext};
use bevy_reflect::TypePath;
use tutti_io::WaveAsset;

/// In-memory loader for [`WaveAsset`]. Reads the entire payload, then
/// delegates to [`WaveAsset::from_bytes`].
#[derive(Default, TypePath)]
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
    Decode(#[from] tutti_io::WaveError),
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
        WaveAsset::from_bytes(&bytes).map_err(WaveAssetLoaderError::Decode)
    }

    fn extensions(&self) -> &[&str] {
        WaveAsset::EXTENSIONS
    }
}
