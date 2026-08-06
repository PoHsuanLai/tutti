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
            // A parameter with no declared bounds has nothing to denormalize
            // against; dropping it leaves its automation to pass through
            // untouched, which is what `add_param_changes` does for a param
            // missing from this map. Every CLAP parameter declares a range, so
            // this filters nothing today — but inventing bounds if one ever
            // didn't would write a wrong plain value to the plugin.
            .filter_map(|p| {
                let (min, max) = p.range.bounds()?;
                // `param_ranges` is matched against `ParameterQueue.param_id`,
                // which stays a bare `u32` because it crosses the IPC wire, and
                // is handed to `ClapEvent::param_value` as the ABI's `u32`.
                // Unwrapping here keeps the whole path between those two in the
                // one type they both speak. Every CLAP param is opaque, so the
                // `None` arm is unreachable rather than a filter.
                Some((p.id.opaque()?.get(), min as f32, max as f32))
            })
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

        // Pre-allocate the growable event scratch so steady-state `process`
        // calls never touch the allocator.
        //
        // `out_midi` / `out_note_expressions` are absent here on purpose: they
        // are `RtVec`, whose capacity is inline and fixed at the type level, so
        // there is nothing to reserve. Reserving was how those two *used* to
        // avoid allocating — an off-RT bound that the on-RT push path then had
        // to be trusted to respect. It isn't trusted any more; it's enforced.
        scratch.input_events.reserve(EVENT_SCRATCH_CAPACITY);
        scratch.output_events.reserve(EVENT_SCRATCH_CAPACITY);
        scratch
            .out_param_changes
            .queues
            .reserve(PARAM_QUEUE_CAPACITY);

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

    /// Clear the plugin's processing state — buffers, filters, oscillators,
    /// envelopes, LFOs — and kill its voices. Parameter values are unchanged.
    ///
    /// Call this on any playback discontinuity the host creates: a locate, a
    /// loop wrap, a punch. Without it a reverb tail, a ringing filter or a hung
    /// voice bleeds across the jump into material it never belonged to.
    ///
    /// Also resets the `steady_time` counter this instance passes to
    /// `clap_process`. CLAP permits `steady_time` to jump backward **because**
    /// `reset` was called, so the two belong in one call; `stop_processing`
    /// zeroes it for the same reason, at the other discontinuity a host makes.
    ///
    /// # Threading
    /// CLAP marks `reset` `[audio-thread & active]`. It lives on `ClapActive`
    /// because that is the type whose existence *is* the `active` half — a
    /// `ClapLoaded` has not activated, and the spec gives `reset` no
    /// inactive-instance contract the way `params.flush` has. The
    /// `[audio-thread]` half is taken here rather than asserted: the call runs
    /// under an [`AudioThreadClaim`], so the calling OS thread becomes the
    /// audio thread for its duration (which the spec permits for any thread)
    /// and blocks until any in-flight `process` has returned. A host driving
    /// this from its UI thread on a locate is therefore serialized against the
    /// audio thread rather than racing it.
    pub fn reset(&mut self) {
        // Clone the Arc into a local so the claim borrows the local rather than
        // `self` — the plugin call below needs `&mut self`. An Arc clone is an
        // atomic increment, no allocation.
        let host_state = Arc::clone(&self.loaded.host_state);
        let _claim = host_state.claim_audio_thread();

        let plugin_ref = unsafe { self.loaded.plugin.as_ref() };
        if let Some(reset_fn) = plugin_ref.reset {
            unsafe { reset_fn(self.loaded.plugin.as_ptr()) };
        }
        self.scratch.steady_time = 0;
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
                // Allocation-free: this runs on the audio thread, and a plugin
                // that refuses to start typically refuses on every block.
                return Err(ClapError::StartProcessingFailed);
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
    /// ceiling; the instance is **rolled back** to the previous `max_frames`
    /// and left active there. [`ClapError::LoadFailed`] if the rollback also
    /// fails. See [`Self::reconfigure`].
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
    /// Same contract as [`Self::set_max_block_size`].
    pub fn set_sample_rate(&mut self, sample_rate: f64) -> Result<()> {
        if (self.loaded.audio.sample_rate - sample_rate).abs() < f64::EPSILON {
            return Ok(());
        }
        self.reconfigure(sample_rate, self.loaded.audio.max_frames)
    }

    /// Deactivate → re-activate at `(sample_rate, max_frames)`, restoring the
    /// previous configuration if the plugin refuses the new one.
    ///
    /// A failed `activate` must not be discarded (both setters used to write
    /// `let _ =`): `activate_plugin` returns early without setting
    /// `flags.active`, leaving a `ClapActive` whose plugin is deactivated —
    /// exactly the state the type exists to rule out. `process` would then
    /// `start_processing` on a deactivated plugin, and
    /// [`flush_params`](ClapLoaded::flush_params) would pick its thread
    /// contract off the stale flag.
    ///
    /// Rolling back to the previous configuration is the one recovery available
    /// — the plugin accepted it once — and it restores `flags.active == true`.
    /// The caller sees the refusal as `Err` and can read
    /// [`sample_rate`](ClapLoaded::sample_rate) /
    /// [`block_size`](ClapLoaded::block_size) for what it is still running at.
    /// If the rollback also fails there is nothing left to fall back to: the
    /// instance is genuinely inactive and the `Err` is
    /// [`ClapError::LoadFailed`] with [`LoadStage::Activation`], distinct from
    /// the plain refusal. `Drop` still tears it down safely.
    fn reconfigure(&mut self, sample_rate: f64, max_frames: u32) -> Result<()> {
        self.loaded.assert_main_thread();

        let prev_rate = self.loaded.audio.sample_rate;
        let prev_frames = self.loaded.audio.max_frames;

        self.stop_processing();
        self.loaded.deactivate_plugin();

        self.loaded.audio.sample_rate = sample_rate;
        self.loaded.audio.max_frames = max_frames;
        // Port layout and channel counts are unchanged; only per-channel length
        // tracks `max_frames`. A pure rate change passes the same value back
        // in, so this is a no-op there.
        self.resize_scratch(max_frames);

        if self.loaded.activate_plugin().is_ok() {
            return Ok(());
        }

        // Refused. Put back what the plugin already accepted once.
        self.loaded.audio.sample_rate = prev_rate;
        self.loaded.audio.max_frames = prev_frames;
        self.resize_scratch(prev_frames);

        // A refused rollback is a different (and worse) fact than the
        // `NotSupported` below, so it propagates rather than folding into it.
        self.loaded.activate_plugin()?;

        Err(ClapError::NotSupported(format!(
            "Plugin '{}' refused activation at sample_rate {sample_rate} / \
             max_frames {max_frames}; still active at {prev_rate} / {prev_frames}",
            self.loaded.info.name
        )))
    }

    /// Re-size the RT channel scratch for `max_frames`; the port layout is
    /// fixed at load. The rollback path calls this a second time, paying a
    /// second allocation off the audio thread — worth it, since scratch sized
    /// for a ceiling the plugin is *not* activated at would let `process`
    /// accept a block the plugin never agreed to.
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

        // PluginPtr::Drop calls destroy(); EntryGuard::Drop is a no-op;
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
