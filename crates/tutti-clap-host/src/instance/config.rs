//! Grouped configuration state for a ClapInstance.
//!
//! The bare fields `sample_rate`, `max_frames`, `supports_f64`,
//! `input_port_channels`, `output_port_channels`, `is_active`, `is_processing`,
//! and `gui_created` each belong to one of three cohesive groups.

use clap_sys::audio_buffer::clap_audio_buffer;

/// Audio format the host presents to the plugin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioConfig {
    pub sample_rate: f64,
    pub max_frames: u32,
    pub supports_f64: bool,
}

/// Pre-allocated scratch for one sample type (f32 or f64) used by the RT
/// process path. Sized once in `activate()` from the plugin's port layout;
/// [`ClapInstance::process`](super::ClapInstance::process) reuses these
/// vectors in place — it never grows or reallocates them on the audio
/// thread.
///
/// Fields are `pub(crate)` because this struct is an internal buffer pool;
/// users interact with it only through `ClapInstance::process`.
pub struct ProcessScratch<T> {
    pub(crate) channels: Vec<Vec<T>>,
    pub(crate) input_ptrs: Vec<*mut T>,
    pub(crate) output_ptrs: Vec<*mut T>,
    pub(crate) input_bufs: Vec<clap_audio_buffer>,
    pub(crate) output_bufs: Vec<clap_audio_buffer>,
}

// The raw pointers live inside our own `channels` vec; the whole struct is
// fine to move across threads together with `ClapInstance` (which is
// already `Send`). Pointers are re-derived on each `process` call.
unsafe impl<T: Send> Send for ProcessScratch<T> {}

impl<T: Copy + Default> ProcessScratch<T> {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            input_ptrs: Vec::new(),
            output_ptrs: Vec::new(),
            input_bufs: Vec::new(),
            output_bufs: Vec::new(),
        }
    }

    /// Pre-allocate every vector. Called from `activate()` after the port
    /// layout and max-frames are known, so the RT path never grows these.
    ///
    /// Channel pool layout: the first `input_channels_total` entries serve
    /// the input side; the next `output_channels_total` serve the output
    /// side. This split keeps input and output pads independent.
    pub fn resize_for(
        &mut self,
        input_channels_total: usize,
        output_channels_total: usize,
        max_frames: usize,
        num_input_ports: usize,
        num_output_ports: usize,
    ) {
        let total_channel_bufs = input_channels_total + output_channels_total;
        self.channels.clear();
        self.channels.reserve_exact(total_channel_bufs);
        for _ in 0..total_channel_bufs {
            self.channels.push(vec![T::default(); max_frames]);
        }

        self.input_ptrs.clear();
        self.input_ptrs.reserve_exact(input_channels_total);
        self.output_ptrs.clear();
        self.output_ptrs.reserve_exact(output_channels_total);

        self.input_bufs.clear();
        self.input_bufs.reserve_exact(num_input_ports);
        self.output_bufs.clear();
        self.output_bufs.reserve_exact(num_output_ports);
    }
}

/// Per-port channel counts for audio IO.
/// E.g. `inputs = [2]` for stereo, `[2, 2]` for two stereo ports.
#[derive(Debug, Clone, Default)]
pub(crate) struct PortLayout {
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
}

impl PortLayout {
    pub fn input_channel_total(&self) -> usize {
        self.inputs.iter().map(|&c| c as usize).sum()
    }

    pub fn output_channel_total(&self) -> usize {
        self.outputs.iter().map(|&c| c as usize).sum()
    }
}

/// Tracks which lifecycle transitions have been performed. These three
/// flags form a state machine: `!active && !processing → active → processing`,
/// while `gui_created` is orthogonal.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LifecycleFlags {
    pub active: bool,
    pub processing: bool,
    pub gui_created: bool,
}
