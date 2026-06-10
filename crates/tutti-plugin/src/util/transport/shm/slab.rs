//! Named, cross-process audio storage. `channels × samples_per_channel`
//! of `f32`/`f64` samples in shared memory.
//!
//! No synchronization here — the control channel handshake (in the layer
//! above) is what tells each side when the other is done writing.

use crate::protocol::audio::Sample;
use crate::error::{BridgeError, Result};
use crate::protocol::{SampleFormat, SlabLayout};
use memmap2::MmapMut;
use std::fs::OpenOptions;

use super::mmap::{as_bytes, as_bytes_mut, MmapCell};

fn sample_size(format: SampleFormat) -> usize {
    match format {
        SampleFormat::Float32 => std::mem::size_of::<f32>(),
        SampleFormat::Float64 => std::mem::size_of::<f64>(),
    }
}

fn channel_offset(layout: &SlabLayout, channel: usize) -> usize {
    channel * layout.samples_per_channel * sample_size(layout.format)
}

enum Ownership {
    Owner,
    View,
}

/// Named mmap region of `channels × samples_per_channel` audio samples.
///
/// Create on one side, open on the other with a matching [`SlabLayout`].
/// The creator owns the backing file and unlinks it on drop; the opener
/// is a view.
pub struct AudioSlab {
    mmap: MmapCell,
    name: String,
    layout: SlabLayout,
    ownership: Ownership,
}

impl AudioSlab {
    pub fn create(name: impl Into<String>, layout: SlabLayout) -> Result<Self> {
        let name = name.into();
        let mmap = open_mmap(&name, layout.byte_size(), Open::Create)?;
        Ok(Self {
            mmap: MmapCell::new(mmap),
            name,
            layout,
            ownership: Ownership::Owner,
        })
    }

    pub fn open(name: impl Into<String>, layout: SlabLayout) -> Result<Self> {
        let name = name.into();
        let mmap = open_mmap(&name, layout.byte_size(), Open::Existing)?;
        Ok(Self {
            mmap: MmapCell::new(mmap),
            name,
            layout,
            ownership: Ownership::View,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn layout(&self) -> SlabLayout {
        self.layout.clone()
    }

    /// Borrow the layout without cloning its (heap-backed) bus list — for the
    /// allocation-free audio path. `layout()` (which clones) is for callers
    /// that need to own a copy.
    pub fn layout_ref(&self) -> &SlabLayout {
        &self.layout
    }

    /// Caller must ensure single-writer access per channel.
    pub fn write_channel<T: Sample>(&self, channel: usize, data: &[T]) -> Result<()> {
        self.check_channel(channel)?;
        if data.len() > self.layout.samples_per_channel {
            return Err(oob("data length exceeds buffer capacity"));
        }
        let offset = channel_offset(&self.layout, channel);
        let bytes = as_bytes(data);
        self.mmap.as_mut_slice()[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Zero-copy read into `output`. Returns the number of samples copied.
    pub fn read_channel_into<T: Sample>(&self, channel: usize, output: &mut [T]) -> Result<usize> {
        self.check_channel(channel)?;
        let copy_samples = self.layout.samples_per_channel.min(output.len());
        let copy_bytes = copy_samples * std::mem::size_of::<T>();
        let offset = channel_offset(&self.layout, channel);
        let src = &self.mmap.as_slice()[offset..offset + copy_bytes];
        as_bytes_mut(&mut output[..copy_samples]).copy_from_slice(src);
        Ok(copy_samples)
    }

    fn check_channel(&self, channel: usize) -> Result<()> {
        if channel >= self.layout.channels {
            Err(oob("channel index out of bounds"))
        } else {
            Ok(())
        }
    }
}

impl Clone for AudioSlab {
    fn clone(&self) -> Self {
        Self::open(self.name.clone(), self.layout.clone())
            .expect("failed to reopen shared-memory slab for clone")
    }
}

impl Drop for AudioSlab {
    fn drop(&mut self) {
        if matches!(self.ownership, Ownership::Owner) {
            let _ = std::fs::remove_file(shm_path(&self.name));
        }
    }
}

fn oob(msg: &'static str) -> BridgeError {
    BridgeError::SharedMemoryError(msg.into())
}

enum Open {
    Create,
    Existing,
}

fn open_mmap(name: &str, size: usize, mode: Open) -> Result<MmapMut> {
    let path = shm_path(name);
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    match mode {
        Open::Create => {
            opts.create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
        }
        Open::Existing => {}
    }
    let file = opts.open(&path).map_err(|e| {
        BridgeError::SharedMemoryError(format!("failed to open shared memory file: {e}"))
    })?;
    if matches!(mode, Open::Create) {
        file.set_len(size as u64)
            .map_err(|e| BridgeError::SharedMemoryError(format!("failed to set file size: {e}")))?;
    }
    unsafe { MmapMut::map_mut(&file) }
        .map_err(|e| BridgeError::SharedMemoryError(format!("failed to map shared memory: {e}")))
}

#[cfg(unix)]
fn shm_path(name: &str) -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    let base = std::path::PathBuf::from("/dev/shm");
    #[cfg(target_os = "macos")]
    let base = std::env::temp_dir();
    base.join(format!("tutti_{name}"))
}

#[cfg(windows)]
fn shm_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tutti_{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(channels: usize, samples: usize, format: SampleFormat) -> SlabLayout {
        SlabLayout {
            channels,
            samples_per_channel: samples,
            format,
            inputs: Default::default(),
            outputs: Default::default(),
        }
    }

    #[test]
    fn roundtrip_f32() {
        let name = format!("slab_f32_{}", std::process::id());
        let layout = layout(2, 128, SampleFormat::Float32);
        let writer = AudioSlab::create(name.clone(), layout.clone()).unwrap();

        let data: Vec<f32> = (0..128).map(|i| i as f32 * 0.1).collect();
        writer.write_channel(0, &data).unwrap();

        let reader = AudioSlab::open(name, layout).unwrap();
        let mut out = vec![0.0f32; 128];
        let n = reader.read_channel_into(0, &mut out).unwrap();
        assert_eq!(n, 128);
        assert_eq!(data, out);
    }

    #[test]
    fn roundtrip_f64() {
        let name = format!("slab_f64_{}", std::process::id());
        let layout = layout(1, 64, SampleFormat::Float64);
        let writer = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let data: Vec<f64> = (0..64).map(|i| (i as f64).sin()).collect();
        writer.write_channel(0, &data).unwrap();

        let reader = AudioSlab::open(name, layout).unwrap();
        let mut out = vec![0.0f64; 64];
        reader.read_channel_into(0, &mut out).unwrap();
        assert_eq!(data, out);
    }

    #[test]
    fn clone_preserves_layout() {
        let name = format!("slab_clone_{}", std::process::id());
        let layout = layout(2, 32, SampleFormat::Float64);
        let original = AudioSlab::create(name, layout.clone()).unwrap();
        let cloned = original.clone();
        assert_eq!(cloned.layout(), layout);
    }

    #[test]
    fn channel_out_of_bounds() {
        let name = format!("slab_oob_{}", std::process::id());
        let slab = AudioSlab::create(name, layout(2, 64, SampleFormat::Float32)).unwrap();
        let data = vec![0.0f32; 64];
        assert!(slab.write_channel(2, &data).is_err());
        let mut out = vec![0.0f32; 64];
        assert!(slab.read_channel_into(5, &mut out).is_err());
    }

    #[test]
    fn data_exceeds_capacity() {
        let name = format!("slab_exceed_{}", std::process::id());
        let slab = AudioSlab::create(name, layout(1, 32, SampleFormat::Float32)).unwrap();
        let data = vec![0.0f32; 64];
        assert!(slab.write_channel(0, &data).is_err());
    }

    #[test]
    fn getters() {
        let name = format!("slab_getters_{}", std::process::id());
        let l = layout(4, 256, SampleFormat::Float32);
        let slab = AudioSlab::create(name.clone(), l.clone()).unwrap();
        assert_eq!(slab.name(), name);
        assert_eq!(slab.layout(), l);
    }
}
