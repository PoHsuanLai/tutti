//! Grouped configuration and scratch state for the CLAP instance types.
//!
//! [`AudioConfig`] / [`PortLayout`] / [`LifecycleFlags`] live on the
//! pre-activation `ClapLoaded`; [`AudioScratch`] holds all per-block RT scratch
//! and lives only on the active `ClapActive<T>` (it exists solely for
//! `process`).

use crate::events::{InputEventList, OutputEventList};
use crate::types::{ClapNoteExpression, MidiEvent, ParameterChanges};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::process::{clap_process_status, CLAP_PROCESS_CONTINUE};
use smallvec::SmallVec;

/// Audio format the host presents to the plugin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioConfig {
    pub sample_rate: f64,
    pub max_frames: u32,
    pub supports_f64: bool,
}

/// Pre-allocated scratch for one sample type (f32 or f64) used by the RT
/// process path. Sized once in `activate()` from the plugin's port layout;
/// [`ClapActive::process`](super::ClapActive::process) reuses these
/// vectors in place — it never grows or reallocates them on the audio
/// thread.
///
/// Fields are `pub(crate)` because this struct is an internal buffer pool;
/// users interact with it only through `ClapActive::process`.
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

/// All per-block RT scratch for the committed sample type `T`, grouped so it
/// can live on the active type and drop before the plugin handle.
///
/// Sized once in `ClapActive::activate`; every `process` call reuses these in
/// place and never allocates. Because `ClapActive<T>` is monomorphised by `T`,
/// there is exactly one `ProcessScratch<T>` here — the pre-split single type
/// had to carry both an f32 and an f64 pool.
pub(crate) struct AudioScratch<T: super::ClapSample> {
    /// Channel pool + per-port `clap_audio_buffer` descriptors.
    pub process: ProcessScratch<T>,
    /// Refilled per call (UMP → CLAP). Cleared in place; capacity persists.
    pub input_events: InputEventList,
    /// Per-parameter native range `(param_id, min, max)`, cached at activation.
    /// CLAP parameter events carry the plugin's **plain** value (CLAP has no
    /// normalization), but host-side automation authors values normalized
    /// `0..1` — so incoming param points are denormalized `min + v·(max-min)`
    /// against this map before the plugin sees them. Empty ⇒ pass-through.
    pub param_ranges: Vec<(u32, f32, f32)>,
    /// Filled by the plugin's `try_push` callback during `process`.
    pub output_events: OutputEventList,
    /// Return pools: `process` drains the plugin's emitted events into these so
    /// the call can hand back borrowed slices.
    pub out_midi: SmallVec<[MidiEvent; 64]>,
    pub out_param_changes: ParameterChanges,
    pub out_note_expressions: SmallVec<[ClapNoteExpression; 16]>,
    /// Monotonic sample counter fed to CLAP's `steady_time`. Init 0 at
    /// activate; advances by `frames_count` each processed block; reset to 0
    /// on stop_processing/reactivate. Never derived from transport seconds.
    pub steady_time: i64,
    /// The `clap_process_status` the plugin returned on the most recent block.
    /// Drives the TAIL/SLEEP transition logging in `process`; the shared output
    /// vocabulary carries no status field.
    pub last_process_status: clap_process_status,
}

impl<T: super::ClapSample> AudioScratch<T> {
    pub fn new() -> Self {
        Self {
            process: ProcessScratch::new(),
            input_events: InputEventList::new(),
            param_ranges: Vec::new(),
            output_events: OutputEventList::new(),
            out_midi: SmallVec::new(),
            out_param_changes: ParameterChanges::new(),
            out_note_expressions: SmallVec::new(),
            steady_time: 0,
            last_process_status: CLAP_PROCESS_CONTINUE,
        }
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

/// Lifecycle sub-state not encoded by the type.
///
/// The active-vs-loaded distinction is the type (`ClapActive` vs `ClapLoaded`)
/// for *callers*, but methods defined on `ClapLoaded` are reachable from both
/// through `Deref` and cannot see which they were called on. `active` carries
/// that fact down to them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LifecycleFlags {
    /// The plugin's `activate()` has run and `deactivate()` has not.
    ///
    /// Distinct from `processing`, and the distinction is load-bearing: CLAP tags
    /// methods like `params.flush` as `[active ? audio-thread : main-thread]`,
    /// keyed on *activation* — not on whether audio has started flowing. Gating
    /// those on `processing` treats the window between `activate()` and the first
    /// `process()` as inactive, and a plugin that knows it is active reports the
    /// host for calling on the wrong thread.
    pub active: bool,
    pub processing: bool,
    pub gui_created: bool,
}

#[cfg(test)]
mod lifecycle_flag_tests {
    use super::LifecycleFlags;

    /// `active` and `processing` are different facts, and the window where they
    /// disagree is the one that matters.
    ///
    /// CLAP tags `params.flush` `[active ? audio-thread : main-thread]`. Between
    /// `activate()` and the first `process()` a plugin is active but not
    /// processing — and that is exactly when a host pushes initial parameter
    /// values. Gating the thread choice on `processing` took the main-thread
    /// branch there against an active plugin, which TAL-Reverb-4's validation
    /// layer reported as `clap_plugin_params.flush() was called on the wrong
    /// thread`.
    ///
    /// This pins the state machine rather than the call site: if a later edit
    /// collapses the two flags back into one, the sequence below stops being
    /// representable and this test stops compiling or starts failing.
    #[test]
    fn active_and_processing_are_independent() {
        let mut flags = LifecycleFlags::default();
        assert!(
            !flags.active && !flags.processing,
            "loaded, not yet activated"
        );

        // activate() — the plugin is active, but no audio has flowed.
        flags.active = true;
        assert!(
            flags.active && !flags.processing,
            "the window where the two disagree: `flush` is already an \
             audio-thread call here, though `processing` is still false"
        );

        // First process() — start_processing runs.
        flags.processing = true;
        assert!(flags.active && flags.processing);

        // stop_processing() without deactivating: back to the disagreeing window.
        flags.processing = false;
        assert!(
            flags.active,
            "stop_processing does not deactivate — `flush` stays an audio-thread \
             call until deactivate()"
        );

        flags.active = false;
        assert!(!flags.active && !flags.processing);
    }
}
