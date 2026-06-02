//! Render-time buffer management.
//!
//! Allocates the variable-length `AudioBufferList` backing storage and the
//! per-channel scratch buffers used to marshal samples in/out of AudioToolbox.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use crate::stream::ChannelLayout;
use crate::types::*;

/// Backing storage for an `AudioBufferList` plus its trailing `AudioBuffer`s.
///
/// The real C struct ends with a flexible-array member, so we allocate a
/// correctly-sized byte slab and reinterpret it.
pub(crate) struct RenderBufferList {
    storage: Box<[u8]>,
    channels: usize,
}

impl RenderBufferList {
    pub fn new(channels: usize) -> Self {
        let bytes = std::mem::size_of::<u32>() + channels * std::mem::size_of::<AudioBuffer>();
        Self {
            storage: vec![0u8; bytes].into_boxed_slice(),
            channels,
        }
    }

    /// Point the list at `buffers` (one `Vec<f32>` per channel) and return a
    /// pointer suitable for `AudioUnitRender` or the input render callback.
    pub fn bind(&mut self, buffers: &mut [Vec<f32>], frames: u32) -> *mut AudioBufferList {
        let ptr = self.storage.as_mut_ptr() as *mut AudioBufferList;
        unsafe {
            (*ptr).number_buffers = self.channels as u32;
            for (ch, buf) in buffers.iter_mut().take(self.channels).enumerate() {
                let audio_buf = &mut *((&mut (*ptr).buffers[0] as *mut AudioBuffer).add(ch));
                audio_buf.number_channels = 1;
                audio_buf.data_byte_size = frames * std::mem::size_of::<f32>() as u32;
                audio_buf.data = buf.as_mut_ptr() as *mut c_void;
            }
        }
        ptr
    }
}

/// Iterate the `AudioBuffer`s inside a raw `AudioBufferList`.
///
/// # Safety
/// `abl` must be a valid, well-formed `AudioBufferList` with at least
/// `number_buffers` trailing `AudioBuffer`s in contiguous memory.
pub(crate) unsafe fn iter_buffers_mut<'a>(
    abl: *mut AudioBufferList,
) -> impl Iterator<Item = &'a mut AudioBuffer> {
    let count = (*abl).number_buffers as usize;
    let base = &mut (*abl).buffers[0] as *mut AudioBuffer;
    (0..count).map(move |i| &mut *base.add(i))
}

/// Render scratch area: output buffers, input buffers, and a monotonically
/// advancing sample position used for timestamps.
pub(crate) struct RenderScratch {
    list: RenderBufferList,
    pub outputs: Vec<Vec<f32>>,
    pub inputs: Vec<Vec<f32>>,
    sample_position: f64,
}

impl RenderScratch {
    pub fn new(layout: ChannelLayout, block_size: u32) -> Self {
        let out_ch = layout.outputs as usize;
        let in_ch = layout.inputs as usize;
        let size = block_size as usize;

        let outputs = (0..out_ch).map(|_| vec![0.0f32; size]).collect();
        // Allocate at least as many input channels as outputs so effects that
        // report 0 input channels but still pull stereo input don't OOB.
        let inputs = (0..in_ch.max(out_ch)).map(|_| vec![0.0f32; size]).collect();

        Self {
            list: RenderBufferList::new(out_ch),
            outputs,
            inputs,
            sample_position: 0.0,
        }
    }

    pub fn bind_output(&mut self, frames: u32) -> *mut AudioBufferList {
        self.list.bind(&mut self.outputs, frames)
    }

    pub fn stage_input(&mut self, input: &[&[f32]], frames: u32) {
        for (ch, src) in input.iter().enumerate() {
            if let Some(dst) = self.inputs.get_mut(ch) {
                let len = (frames as usize).min(src.len()).min(dst.len());
                dst[..len].copy_from_slice(&src[..len]);
            }
        }
    }

    pub fn emit_output(&self, output: &mut [&mut [f32]], frames: u32) {
        for (ch, dst) in output.iter_mut().enumerate() {
            if let Some(src) = self.outputs.get(ch) {
                let len = (frames as usize).min(dst.len()).min(src.len());
                dst[..len].copy_from_slice(&src[..len]);
            }
        }
    }

    /// Return the pre-advance sample position and move the cursor forward by
    /// `frames`. The pre-advance value is what AudioToolbox expects for the
    /// current block's timestamp.
    pub fn advance(&mut self, frames: u32) -> f64 {
        let prev = self.sample_position;
        self.sample_position += frames as f64;
        prev
    }
}
