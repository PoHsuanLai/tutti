//! Generic Bevy `AssetLoader` adapters over the host-agnostic tutti asset
//! traits. One impl each covers every current and future tutti loadable —
//! replaces the per-type `TuttiAudioLoader` / `Sf2Loader` wrapper pattern.

use std::marker::PhantomData;

use bevy_asset::{Asset, AssetLoader, LoadContext, io::Reader, io::file::FileAssetReader};
use bevy_reflect::TypePath;
use tutti_asset::{TuttiAsset, TuttiStreamingAsset};

/// Generic `AssetLoader` for any [`TuttiAsset`] that is also a Bevy [`Asset`].
/// Reads the entire payload into memory then delegates to
/// [`TuttiAsset::from_bytes`].
#[derive(TypePath)]
pub struct TuttiLoader<A: TuttiAsset + Asset + TypePath>(PhantomData<fn() -> A>);

impl<A: TuttiAsset + Asset + TypePath> Default for TuttiLoader<A> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TuttiLoaderError<E: std::error::Error + Send + Sync + 'static> {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(E),
}

impl<A: TuttiAsset + Asset + TypePath> AssetLoader for TuttiLoader<A> {
    type Asset = A;
    type Settings = ();
    type Error = TuttiLoaderError<A::Error>;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        A::from_bytes(&bytes).map_err(TuttiLoaderError::Parse)
    }

    fn extensions(&self) -> &[&str] {
        A::EXTENSIONS
    }
}

/// Generic `AssetLoader` for any [`TuttiStreamingAsset`] that is also a Bevy
/// [`Asset`]. Resolves `LoadContext::path()` to a local filesystem path and
/// delegates to [`TuttiStreamingAsset::probe`]. The resulting asset is a
/// locator + metadata; the engine opens its own file handle later.
#[derive(TypePath)]
pub struct TuttiStreamingLoader<A: TuttiStreamingAsset + Asset + TypePath>(PhantomData<fn() -> A>);

impl<A: TuttiStreamingAsset + Asset + TypePath> Default for TuttiStreamingLoader<A> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TuttiStreamingLoaderError<E: std::error::Error + Send + Sync + 'static> {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Probe(E),
}

impl<A: TuttiStreamingAsset + Asset + TypePath> AssetLoader for TuttiStreamingLoader<A> {
    type Asset = A;
    type Settings = ();
    type Error = TuttiStreamingLoaderError<A::Error>;

    async fn load(
        &self,
        _reader: &mut dyn Reader,
        _settings: &Self::Settings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let full_path = FileAssetReader::get_base_path().join(load_context.path().path());
        A::probe(&full_path).map_err(TuttiStreamingLoaderError::Probe)
    }

    fn extensions(&self) -> &[&str] {
        A::EXTENSIONS
    }
}
