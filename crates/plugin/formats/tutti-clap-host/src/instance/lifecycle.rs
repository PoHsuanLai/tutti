//! Lifecycle transitions and queries: `ClapLoaded`'s metadata accessors and
//! `activate`, the active-side `ClapActive` methods (`process` lives in
//! [`super::audio`]), the `Deref` bridge, and the `Drop` teardown for both.

use super::config::AudioScratch;
use super::{ClapActive, ClapLoaded};
use crate::error::{ClapError, LoadStage, Result};
use crate::host::AudioThreadClaim;
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
    // The `Err` variant deliberately hands `self` (a large `ClapLoaded`) back so
    // the caller can retry or fall back; boxing it would defeat that ownership
    // return and add a heap alloc on the (rare) failure path.
    #[allow(clippy::result_large_err)]
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
        // Cache each param's native range for denormalizing incoming automation
        // (host authors normalized 0..1; CLAP wants plain). Done once here, off
        // the audio thread — `parameter_list()` queries the plugin.
        scratch.param_ranges = self
            .parameter_list()
            .into_iter()
            .map(|p| (p.id, p.min_value as f32, p.max_value as f32))
            .collect();
        // Read the plugin's own count, not the length of the map above: they
        // differ exactly when `parameters()` truncated at a hole, which is the
        // case `add_param_changes` has to tell apart from a params-less plugin.
        scratch.plugin_claims_params = self.parameter_count() > 0;
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
        // The plugin now considers itself active, which changes the thread
        // contract of every `[active ? audio-thread : main-thread]` method — see
        // `LifecycleFlags::active`.
        self.flags.active = true;
        Ok(())
    }

    /// Call the plugin's `deactivate` FFI. Shared by `ClapActive::deactivate`
    /// and the in-place re-activation path.
    pub(crate) fn deactivate_plugin(&mut self) {
        let plugin_ref = unsafe { self.plugin.as_ref() };
        if let Some(deactivate_fn) = plugin_ref.deactivate {
            unsafe { deactivate_fn(self.plugin.as_ptr()) };
        }
        self.flags.active = false;
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
    ///
    /// # Threading (C1)
    /// CLAP marks `start_processing` `[audio-thread & active & !processing]`.
    /// The audio-thread is symbolic: any OS thread may take the role provided
    /// only one holds it at a time, so this takes an [`AudioThreadClaim`] for
    /// the duration of the call. Under the claim `is_audio_thread()` is `true`
    /// and `is_main_thread()` is `false` on this thread — no dual identity —
    /// and no other thread can be inside a `[audio-thread]` plugin call.
    ///
    /// `claim` is threaded in by the caller rather than taken here so the RT
    /// `do_process` path takes the lock **once** per block and covers both
    /// `start_processing` and `process` with a single claim.
    pub(crate) fn ensure_processing(&mut self, _claim: &AudioThreadClaim<'_>) -> Result<()> {
        if self.loaded.flags.processing {
            return Ok(());
        }

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

    /// Stop processing under a fresh [`AudioThreadClaim`] (C1).
    ///
    /// Every caller here is a setup-time / teardown path on the main thread
    /// (`deactivate`, `set_sample_rate`, `set_max_block_size`, `Drop`). Taking
    /// the claim makes this OS thread *the* audio thread for the duration of
    /// the `stop_processing` call — which is what the spec's `[audio-thread]`
    /// tag actually requires — and blocks until any in-flight `process` on a
    /// real audio thread has returned.
    pub(crate) fn stop_processing(&mut self) {
        // Clone the Arc into a local so the claim borrows the local, not
        // `self` — `stop_processing_claimed` needs `&mut self`. Arc clone is an
        // atomic increment, no allocation.
        let host_state = Arc::clone(&self.loaded.host_state);
        let claim = host_state.claim_audio_thread();
        self.stop_processing_claimed(&claim);
    }

    /// `stop_processing` for a caller that already holds the claim.
    pub(crate) fn stop_processing_claimed(&mut self, _claim: &AudioThreadClaim<'_>) {
        if !self.loaded.flags.processing {
            return;
        }
        let plugin_ref = unsafe { self.loaded.plugin.as_ref() };
        if let Some(stop_fn) = plugin_ref.stop_processing {
            unsafe { stop_fn(self.loaded.plugin.as_ptr()) };
        }
        self.loaded.flags.processing = false;
        // H2: the CLAP steady_time counter is per start/stop cycle — reset it so
        // the next start_processing begins the monotonic sequence at 0.
        self.scratch.steady_time = 0;
    }

    /// Grow the activated maximum block size to `max_frames`, resizing the RT
    /// scratch to match (C1). Only GROWS: a request no larger than the current
    /// ceiling is a no-op, so shrinking never strands allocated capacity.
    ///
    /// CLAP fixes `max_frames` at `activate()`, so a genuine grow must
    /// deactivate → re-activate the plugin at the new ceiling. **Main-thread /
    /// setup only** — never call on the audio thread (it deactivates the
    /// plugin and reallocates). The next `process` self-starts processing.
    ///
    /// # Errors
    /// [`ClapError::NotSupported`] if the plugin refuses `activate` at the new
    /// ceiling. The instance is then **rolled back** to the previous
    /// `max_frames` and left active there, so it remains usable — see
    /// [`Self::reconfigure`] for why a refusal cannot be discarded.
    ///
    /// [`ClapError::LoadFailed`] if the rollback *also* fails. See
    /// [`Self::reconfigure`].
    pub fn set_max_block_size(&mut self, max_frames: u32) -> Result<()> {
        if max_frames <= self.loaded.audio.max_frames {
            return Ok(());
        }
        self.reconfigure(self.loaded.audio.sample_rate, max_frames)
    }

    /// Change the sample rate in place. CLAP requires deactivation around a
    /// sample-rate change, so this stops processing and re-activates the plugin
    /// at the new rate; the next `process` call self-starts processing again.
    /// Setup-time only — never call on the audio thread.
    ///
    /// # Errors
    /// Same contract as [`Self::set_max_block_size`]: a plugin that refuses the
    /// new rate leaves the instance rolled back to the previous rate and still
    /// active, and says so through [`ClapError::NotSupported`].
    pub fn set_sample_rate(&mut self, sample_rate: f64) -> Result<()> {
        if (self.loaded.audio.sample_rate - sample_rate).abs() < f64::EPSILON {
            return Ok(());
        }
        self.reconfigure(sample_rate, self.loaded.audio.max_frames)
    }

    /// Deactivate → re-activate at `(sample_rate, max_frames)`, restoring the
    /// previous configuration if the plugin refuses the new one.
    ///
    /// ## Why a refusal cannot be discarded
    ///
    /// CLAP's `activate` returns `false` to mean **"no, not at this
    /// configuration"** — a plugin is entitled to reject a rate or block size
    /// it cannot run. That is a *refusal*, not a "not applicable". Both
    /// setters used to write `let _ = self.loaded.activate_plugin();`, and the
    /// consequences of dropping that `Err` were not cosmetic:
    ///
    /// - `activate_plugin` returns early **without** setting
    ///   `flags.active`, so the instance was left `active == false` while its
    ///   *type* was still `ClapActive` — the one state the type is supposed to
    ///   make unrepresentable.
    /// - The next `process` therefore called `ensure_processing` →
    ///   `start_processing` on a **deactivated** plugin, violating CLAP's
    ///   `[audio-thread & active & !processing]` tag on that entry point.
    /// - [`flush_params`](ClapLoaded::flush_params) branches on `flags.active`
    ///   to pick between the audio-thread and main-thread contract, so it would
    ///   have taken the main-thread branch against a plugin that (from the
    ///   host's own bookkeeping) was mid-reconfiguration.
    ///
    /// This is the CLAP twin of the VST3 host bug where `setActive` returning
    /// `kResultFalse` was read as "the plugin has nothing to say".
    ///
    /// ## Why roll back rather than surface a dead instance
    ///
    /// The previous configuration is one the plugin already accepted, so
    /// re-activating there is the one recovery a host can actually perform —
    /// and it restores the `ClapActive` invariant (`flags.active == true`)
    /// instead of handing the caller a value whose type lies about its state.
    /// The caller learns the request was denied from the `Err`, and can read
    /// [`sample_rate`](ClapLoaded::sample_rate) /
    /// [`block_size`](ClapLoaded::block_size) to see what it is still running
    /// at.
    ///
    /// If the rollback *also* fails the plugin has refused a configuration it
    /// previously accepted; there is nothing left to fall back to. The
    /// resulting `Err` is [`ClapError::LoadFailed`] with
    /// [`LoadStage::Activation`] — distinct from the plain refusal — and the
    /// instance is genuinely inactive. `Drop` still tears it down safely
    /// (`deactivate_plugin` on an inactive plugin is what a CLAP host does
    /// after a failed `activate` anyway), and `process` will report the failed
    /// `start_processing` rather than corrupting anything.
    fn reconfigure(&mut self, sample_rate: f64, max_frames: u32) -> Result<()> {
        self.loaded.assert_main_thread();

        let prev_rate = self.loaded.audio.sample_rate;
        let prev_frames = self.loaded.audio.max_frames;

        self.stop_processing();
        self.loaded.deactivate_plugin();

        self.loaded.audio.sample_rate = sample_rate;
        self.loaded.audio.max_frames = max_frames;
        // Re-size the channel scratch. Port layout and channel counts are
        // unchanged; only per-channel length tracks `max_frames`. A pure
        // sample-rate change passes the same `max_frames` back in, so this is a
        // no-op there.
        self.resize_scratch(max_frames);

        if self.loaded.activate_plugin().is_ok() {
            return Ok(());
        }

        // Refused. Put back what the plugin already accepted once.
        self.loaded.audio.sample_rate = prev_rate;
        self.loaded.audio.max_frames = prev_frames;
        self.resize_scratch(prev_frames);

        // A refused rollback means the plugin has now declined a configuration it
        // previously accepted; that `LoadFailed` is a different (and worse) fact
        // than the `NotSupported` below, so it propagates rather than being
        // folded into it.
        self.loaded.activate_plugin()?;

        Err(ClapError::NotSupported(format!(
            "Plugin '{}' refused activation at sample_rate {sample_rate} / \
             max_frames {max_frames}; still active at {prev_rate} / {prev_frames}",
            self.loaded.info.name
        )))
    }

    /// Re-size the RT channel scratch for `max_frames`. Split out of
    /// [`Self::reconfigure`] because the rollback path needs the identical
    /// call with the previous ceiling. Only per-channel length changes — the
    /// port layout is fixed at load.
    ///
    /// This rebuilds the channel vectors rather than growing them in place, so
    /// the rollback path pays a second allocation. That is acceptable: it runs
    /// only when a plugin refused a configuration, off the audio thread, and
    /// leaving the scratch sized for a ceiling the plugin is *not* activated at
    /// would let `process` accept a block the plugin never agreed to.
    fn resize_scratch(&mut self, max_frames: u32) {
        let input_total = self.loaded.ports.input_channel_total();
        let output_total = self.loaded.ports.output_channel_total();
        let num_in = self.loaded.ports.inputs.len();
        let num_out = self.loaded.ports.outputs.len();
        self.scratch.process.resize_for(
            input_total,
            output_total,
            max_frames as usize,
            num_in,
            num_out,
        );
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
