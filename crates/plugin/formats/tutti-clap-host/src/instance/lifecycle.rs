//! Lifecycle transitions and queries: `ClapLoaded`'s metadata accessors and
//! `activate`, the active-side `ClapActive` methods (`process` lives in
//! [`super::audio`]), the `Deref` bridge, and the `Drop` teardown for both.

use super::config::AudioScratch;
use super::{ClapActive, ClapLoaded};
use crate::error::{ClapError, LoadStage, Result};
use crate::types::PluginInfo;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::Arc;

/// Heap reservation for pooled per-call event scratch on a `ClapActive`.
///
/// Sized once at activation so steady-state `process` calls avoid the
/// allocator.
const EVENT_SCRATCH_CAPACITY: usize = 256;
/// Heap reservation for the parameter-change queue list (one entry per
/// distinct param_id emitted by the plugin per block).
const PARAM_QUEUE_CAPACITY: usize = 16;

impl ClapLoaded {
    pub fn supports_f64(&self) -> bool {
        self.audio.supports_f64
    }

    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    pub fn sample_rate(&self) -> f64 {
        self.audio.sample_rate
    }

    pub fn block_size(&self) -> u32 {
        self.audio.max_frames
    }

    /// Debug-only guard: panic if called off the thread that created this
    /// instance's `HostState` (the main/UI thread). CLAP requires all calls
    /// except the audio-thread process path to run on the main thread; this
    /// turns a silent spec violation into a located panic in dev/test. No-op
    /// in release.
    #[inline]
    pub(crate) fn assert_main_thread(&self) {
        debug_assert_eq!(
            std::thread::current().id(),
            self.host_state.main_thread_id,
            "CLAP main-thread call made off the main thread"
        );
    }

    /// Activate the plugin, transitioning to a [`ClapActive<T>`] that can
    /// [`process`](ClapActive::process). Consumes `self`; on failure the
    /// `ClapLoaded` is handed back alongside the error so the caller can retry
    /// or fall back.
    ///
    /// `T` fixes the processing sample format. `ClapActive<f64>` requires the
    /// plugin to advertise 64-bit support; otherwise this returns
    /// [`ClapError::NotSupported`].
    pub fn activate<T: super::ClapSample>(
        mut self,
    ) -> std::result::Result<ClapActive<T>, (Self, ClapError)> {
        self.assert_main_thread();

        if T::requires_f64() && !self.audio.supports_f64 {
            let err = ClapError::NotSupported(format!(
                "Plugin '{}' does not support 64-bit audio processing",
                self.info.name
            ));
            return Err((self, err));
        }

        if let Err(e) = self.activate_plugin() {
            return Err((self, e));
        }

        // Size the single AudioScratch<T> now that the port layout is known.
        // Only one sample-type pool exists (T), so the pre-split double-sizing
        // of both f32 and f64 is gone.
        let mut scratch = AudioScratch::<T>::new();
        let input_total = self.ports.input_channel_total();
        let output_total = self.ports.output_channel_total();
        let max_frames = self.audio.max_frames as usize;
        let num_in = self.ports.inputs.len();
        let num_out = self.ports.outputs.len();
        scratch
            .process
            .resize_for(input_total, output_total, max_frames, num_in, num_out);

        // Pre-allocate event scratch so steady-state `process` calls never
        // touch the allocator.
        scratch.input_events.reserve(EVENT_SCRATCH_CAPACITY);
        scratch.output_events.reserve(EVENT_SCRATCH_CAPACITY);
        scratch.out_midi.reserve(EVENT_SCRATCH_CAPACITY);
        scratch
            .out_param_changes
            .queues
            .reserve(PARAM_QUEUE_CAPACITY);
        scratch.out_note_expressions.reserve(EVENT_SCRATCH_CAPACITY);

        Ok(ClapActive {
            scratch,
            loaded: self,
            _sample: PhantomData,
        })
    }

    /// Call the plugin's `activate` FFI. Shared by `activate<T>` and the
    /// in-place re-activation path after `set_sample_rate`.
    pub(crate) fn activate_plugin(&mut self) -> Result<()> {
        let plugin_ref = unsafe { self.plugin.as_ref() };
        let activate_fn = plugin_ref.activate.ok_or(ClapError::NotActivated)?;
        if !unsafe {
            activate_fn(
                self.plugin.as_ptr(),
                self.audio.sample_rate,
                1,
                self.audio.max_frames,
            )
        } {
            return Err(ClapError::LoadFailed {
                path: PathBuf::new(),
                stage: LoadStage::Activation,
                reason: "Activate failed".to_string(),
            });
        }
        Ok(())
    }

    /// Call the plugin's `deactivate` FFI. Shared by `ClapActive::deactivate`
    /// and the in-place re-activation path.
    pub(crate) fn deactivate_plugin(&mut self) {
        let plugin_ref = unsafe { self.plugin.as_ref() };
        if let Some(deactivate_fn) = plugin_ref.deactivate {
            unsafe { deactivate_fn(self.plugin.as_ptr()) };
        }
    }
}

impl<T: super::ClapSample> ClapActive<T> {
    pub fn is_processing(&self) -> bool {
        self.loaded.flags.processing
    }

    /// Stop processing (if started) and deactivate, transitioning back to a
    /// non-processing [`ClapLoaded`]. Consumes `self`.
    pub fn deactivate(mut self) -> ClapLoaded {
        self.stop_processing();
        self.loaded.deactivate_plugin();
        // Move `loaded` out without running `ClapActive::drop` (which would
        // stop/deactivate a second time). `scratch` is dropped explicitly so
        // the RT buffers release before `loaded`'s plugin handle is returned.
        let loaded = unsafe { std::ptr::read(&self.loaded) };
        unsafe { std::ptr::drop_in_place(&mut self.scratch) };
        std::mem::forget(self);
        loaded
    }

    /// Ensure the plugin's `start_processing` has run. Called at the top of
    /// `process` (and is a no-op once started), so an instance returned by
    /// `activate` or rebuilt after `set_sample_rate` self-starts on first use.
    pub(crate) fn ensure_processing(&mut self) -> Result<()> {
        if self.loaded.flags.processing {
            return Ok(());
        }

        // Publish the current thread as the audio thread before the plugin's
        // start_processing runs — plugins commonly call is_audio_thread from
        // inside it. This is the only place we pay the Arc allocation; the
        // per-buffer do_process path only reads the ArcSwapOption.
        self.loaded
            .host_state
            .audio_thread_id
            .store(Some(Arc::new(std::thread::current().id())));

        let plugin_ref = unsafe { self.loaded.plugin.as_ref() };
        if let Some(start_fn) = plugin_ref.start_processing {
            if !unsafe { start_fn(self.loaded.plugin.as_ptr()) } {
                return Err(ClapError::ProcessError(
                    "Start processing failed".to_string(),
                ));
            }
        }

        self.loaded.flags.processing = true;
        Ok(())
    }

    pub(crate) fn stop_processing(&mut self) {
        if !self.loaded.flags.processing {
            return;
        }
        let plugin_ref = unsafe { self.loaded.plugin.as_ref() };
        if let Some(stop_fn) = plugin_ref.stop_processing {
            unsafe { stop_fn(self.loaded.plugin.as_ptr()) };
        }
        self.loaded.host_state.audio_thread_id.store(None);
        self.loaded.flags.processing = false;
    }

    /// Change the sample rate in place. CLAP requires deactivation around a
    /// sample-rate change, so this stops processing and re-activates the plugin
    /// at the new rate; the next `process` call self-starts processing again.
    /// Setup-time only — never call on the audio thread.
    pub fn set_sample_rate(&mut self, sample_rate: f64) -> &mut Self {
        if (self.loaded.audio.sample_rate - sample_rate).abs() < f64::EPSILON {
            return self;
        }
        self.loaded.assert_main_thread();
        self.stop_processing();
        self.loaded.deactivate_plugin();
        self.loaded.audio.sample_rate = sample_rate;
        // Re-activate at the new rate; scratch is already sized for max_frames,
        // which is unchanged, so no reallocation is needed.
        let _ = self.loaded.activate_plugin();
        self
    }
}

impl<T: super::ClapSample> Deref for ClapActive<T> {
    type Target = ClapLoaded;
    fn deref(&self) -> &ClapLoaded {
        &self.loaded
    }
}

impl<T: super::ClapSample> DerefMut for ClapActive<T> {
    fn deref_mut(&mut self) -> &mut ClapLoaded {
        &mut self.loaded
    }
}

impl Drop for ClapLoaded {
    fn drop(&mut self) {
        // Destroy GUI before tearing down the plugin — CLAP spec requires
        // gui.destroy() before plugin.destroy(). A `ClapLoaded` is never
        // active (activation moves into `ClapActive`, which deactivates the
        // plugin before its `loaded` field drops), so there is no
        // stop/deactivate to do here.
        self.close_editor();

        // PluginHandle::Drop calls destroy(); EntryGuard::Drop is a no-op;
        // library unloads last.
    }
}

impl<T: super::ClapSample> Drop for ClapActive<T> {
    fn drop(&mut self) {
        // Stop + deactivate the plugin while `loaded` (and thus the plugin
        // handle) is still alive. `loaded`'s own Drop then closes the editor
        // and destroys the plugin. `scratch` drops first (field order), so the
        // RT buffers are released before the plugin is torn down.
        self.stop_processing();
        self.loaded.deactivate_plugin();
    }
}
