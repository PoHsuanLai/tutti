//! Host-agnostic asset-loading vocabulary for Tutti.
//!
//! Two traits describe how a type is loaded:
//!
//! - [`TuttiAsset`] — fully parsed from an in-memory byte slice. One-shot,
//!   fits-in-RAM assets (short samples, SoundFonts).
//! - [`TuttiStreamingAsset`] — probed from a filesystem path, returning a
//!   metadata locator. The engine opens its own handle later when the data
//!   is actually needed (large streaming samples).
//!
//! Hosts (Bevy, Unity, CLI) implement a single generic loader against these
//! traits instead of one wrapper per asset type. Tutti types implement these
//! traits unconditionally; host-specific derives (e.g. `bevy_asset::Asset`)
//! are feature-gated in the owning crate.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

use core::error::Error;

/// One-shot asset parsed from a byte slice.
pub trait TuttiAsset: Sized + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    /// File extensions this asset recognises (without the leading dot).
    const EXTENSIONS: &'static [&'static str];

    /// Parse a complete asset from an in-memory slice.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Error>;

    /// Convenience: read the whole byte stream then delegate to
    /// [`from_bytes`](Self::from_bytes). Useful when the host hands the
    /// loader an opaque reader.
    #[cfg(feature = "std")]
    fn from_reader<R: std::io::Read>(mut reader: R) -> Result<Self, LoadError<Self::Error>> {
        let mut buf = alloc::vec::Vec::new();
        reader.read_to_end(&mut buf).map_err(LoadError::Io)?;
        Self::from_bytes(&buf).map_err(LoadError::Parse)
    }
}

/// Path-backed asset. `probe` reads metadata only; the engine opens the file
/// itself when the data is actually needed.
#[cfg(feature = "std")]
pub trait TuttiStreamingAsset: Sized + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    const EXTENSIONS: &'static [&'static str];

    fn probe(path: &std::path::Path) -> Result<Self, Self::Error>;
}

/// Wrapper error for [`TuttiAsset::from_reader`]. Hosts typically flatten
/// this into their own loader error via `?` + `From` impls.
#[cfg(feature = "std")]
#[derive(Debug, thiserror::Error)]
pub enum LoadError<E: Error + Send + Sync + 'static> {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(E),
}
