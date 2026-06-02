//! Disk-backed neural model asset.
//!
//! [`NeuralModel`] stores a path + inferred format + size for one file on
//! disk. The actual tensor parsing happens later, when the playback system
//! hands the path to a backend crate. This split mirrors how Bevy asset
//! loaders work: probe at load time, open at play time.

use std::path::{Path, PathBuf};

/// Detected model container format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeuralModelFormat {
    /// ONNX (`.onnx`). Detected via magic `"\x08" ... "onnx"` or extension.
    Onnx,
    /// Burn message-pack record (`.mpk`). Detected via extension.
    BurnMpk,
}

/// Locator + metadata for a neural model on disk.
///
/// Implements `bevy_asset::Asset` under the `bevy_asset` feature so the
/// asset pipeline can load `.onnx` / `.mpk` files directly.
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "bevy_asset",
    derive(bevy_asset::Asset, bevy_reflect::TypePath)
)]
pub struct NeuralModel {
    /// Absolute path to the model file.
    pub path: PathBuf,
    /// Display name derived from the file stem.
    pub name: String,
    /// Detected container format.
    pub format: NeuralModelFormat,
    /// File size in bytes (for UI display only).
    pub size_bytes: u64,
}

/// Errors from [`NeuralModel::probe`].
#[derive(Debug, thiserror::Error)]
pub enum NeuralModelProbeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unrecognised neural model format at {0}")]
    UnknownFormat(PathBuf),
}

impl NeuralModel {
    /// Probe `path`: stat the file, infer format from the extension.
    ///
    /// Returns [`NeuralModelProbeError::UnknownFormat`] for extensions this
    /// crate doesn't recognise. Use a backend crate's own loader (e.g.
    /// `tutti_ort::model`) to actually parse the contents.
    pub fn probe(path: &Path) -> Result<Self, NeuralModelProbeError> {
        let metadata = std::fs::metadata(path)?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let format = match path.extension().and_then(|s| s.to_str()) {
            Some("onnx") => NeuralModelFormat::Onnx,
            Some("mpk") => NeuralModelFormat::BurnMpk,
            _ => return Err(NeuralModelProbeError::UnknownFormat(path.to_path_buf())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            name,
            format,
            size_bytes: metadata.len(),
        })
    }
}

#[cfg(feature = "bevy_asset")]
impl tutti_asset::TuttiStreamingAsset for NeuralModel {
    type Error = NeuralModelProbeError;
    const EXTENSIONS: &'static [&'static str] = &["onnx", "mpk"];

    fn probe(path: &Path) -> Result<Self, Self::Error> {
        NeuralModel::probe(path)
    }
}
