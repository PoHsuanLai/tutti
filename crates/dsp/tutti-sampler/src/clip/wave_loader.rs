//! Bevy `AssetLoader` for the in-memory [`WaveAsset`] (short, fits-in-RAM
//! samples). Reads the whole file into memory and decodes it into a shared
//! [`WaveAsset`]. The asset itself lives in `tutti-core`; this is its loader.

use bevy_asset::{io::Reader, AssetLoader, LoadContext};
use bevy_reflect::TypePath;
use tutti_core::WaveAsset;

/// In-memory loader for [`WaveAsset`]. Reads the entire payload, then
/// delegates to [`WaveAsset::from_bytes`].
#[derive(Default, TypePath)]
pub struct WaveAssetLoader;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WaveAssetLoaderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Decode(#[from] tutti_core::WaveError),
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
