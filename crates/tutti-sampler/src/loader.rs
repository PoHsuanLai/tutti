//! Bevy `AssetLoader` impls for the sampler's loadable asset types.
//!
//! - [`WaveAssetLoader`] reads the whole file into memory and decodes it
//!   into a shared [`WaveAsset`] (short, fits-in-RAM samples).
//! - [`StreamingSampleLoader`] only probes the file header into a
//!   [`StreamingSample`] locator; the Butler thread opens its own handle for
//!   streaming playback.

use bevy_asset::{
    io::Reader, io::file::FileAssetReader, AssetLoader, LoadContext,
};
use bevy_reflect::TypePath;
use tutti_core::WaveAsset;

use crate::asset::{StreamingSample, StreamingSampleProbeError};

/// In-memory loader for [`WaveAsset`]. Reads the entire payload, then
/// delegates to [`WaveAsset::from_bytes`].
#[derive(Default, TypePath)]
pub struct WaveAssetLoader;

#[derive(Debug, thiserror::Error)]
pub enum WaveAssetLoaderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Decode(tutti_core::WaveError),
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

/// Path-probing loader for [`StreamingSample`]. Resolves the load context
/// path to a local filesystem path and probes the header via
/// [`StreamingSample::probe`]; the resulting asset is a locator + metadata.
#[derive(Default, TypePath)]
pub struct StreamingSampleLoader;

#[derive(Debug, thiserror::Error)]
pub enum StreamingSampleLoaderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Probe(StreamingSampleProbeError),
}

impl AssetLoader for StreamingSampleLoader {
    type Asset = StreamingSample;
    type Settings = ();
    type Error = StreamingSampleLoaderError;

    async fn load(
        &self,
        _reader: &mut dyn Reader,
        _settings: &Self::Settings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let full_path = FileAssetReader::get_base_path().join(load_context.path().path());
        StreamingSample::probe(&full_path).map_err(StreamingSampleLoaderError::Probe)
    }

    fn extensions(&self) -> &[&str] {
        StreamingSample::EXTENSIONS
    }
}
