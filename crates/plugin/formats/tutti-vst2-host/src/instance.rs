//! High-level VST2 plugin instance.
//!
//! [`Vst2Instance`] is the main entry point. Construct via
//! [`Vst2Instance::load`]; the constructor runs the full VST2 init/resume
//! sequence so the returned instance is immediately usable for
//! [`process_f32`](Self::process_f32) / [`process_f64`](Self::process_f64).
//!
//! Unlike `au-host`'s two-stage `Loaded → Ready` split, VST2's lifecycle
//! is short and atomic — `init → set_sample_rate → set_block_size →
//! resume` all happen at construction. There's no useful state between
//! "ready to load editor / params" and "ready to process audio", so we
//! collapse them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vst::host::PluginLoader;
use vst::plugin::{Category, Plugin as _};

use crate::error::{LoadStage, Result, Vst2Error};
use crate::handle::Vst2Handle;
use crate::host::{HostLink, HostState};
use crate::midi::MidiIo;
use crate::parameters::SendParams;
use crate::transport_cell::TransportCell;
use crate::types::{ChannelLayout, PluginInfo, Samples, Vst2Category};

/// Map the `vst` crate's `Category` to the shared [`Vst2Category`] mirror.
/// A free fn rather than a `From` impl: both `Category` (from `vst`) and
/// `Vst2Category` (from `tutti-plugin-types`) are foreign here, so the orphan
/// rule forbids the impl.
///
/// Takes `raw` alongside the decoded enum because `Category::Unknown` is where
/// the `vst` crate puts every code it does not name, including
/// `kPlugCategUnknown` itself. Only the number tells those apart, so it is
/// carried into [`Vst2Category::Unrecognized`] rather than discarded.
///
/// This function is never reached without a plugin having answered, so it never
/// produces [`Vst2Category::Unasked`] — that variant belongs to paths that did
/// not query at all.
fn map_category(c: Category, raw: i32) -> Vst2Category {
    match c {
        Category::Unknown => Vst2Category::Unrecognized(raw),
        Category::Effect => Vst2Category::Effect,
        Category::Synth => Vst2Category::Synth,
        Category::Analysis => Vst2Category::Analysis,
        Category::Mastering => Vst2Category::Mastering,
        Category::Spacializer => Vst2Category::Spacializer,
        Category::RoomFx => Vst2Category::RoomFx,
        Category::SurroundFx => Vst2Category::SurroundFx,
        Category::Restoration => Vst2Category::Restoration,
        Category::OfflineProcess => Vst2Category::OfflineProcess,
        Category::Shell => Vst2Category::Shell,
        Category::Generator => Vst2Category::Generator,
    }
}

/// Loaded, initialized, processing-ready VST2 plugin.
///
/// The instance owns the `vst::PluginInstance` (via `Vst2Handle`) plus
/// the channel ends fed by the host-callback `HostState`. `Send` but not
/// `Sync` — callers serialize access (subprocess server is single-threaded;
/// in-process backend uses a `parking_lot::Mutex`).
pub struct Vst2Instance {
    /// The loaded `vst::PluginInstance` (owns the editor handle + teardown).
    pub(crate) handle: Vst2Handle,
    /// The plugin's parameter object (get/set/preset access).
    pub(crate) params: SendParams,
    /// Each parameter's value as read once at load, indexed by parameter id.
    ///
    /// VST 2.4 has **no** opcode that reports a default — none of the 61 in
    /// `OpCode` returns one, and `effGetParameterProperties` (56) carries a
    /// range and step granularity but no default either. What a plugin *does*
    /// have is its own initial state: a freshly instantiated plugin sits at its
    /// defaults, so reading each parameter once before anything writes to it is
    /// the only place that value is observable.
    ///
    /// Hence the ordering invariant on [`Vst2Instance::load`]: this snapshot is
    /// taken immediately after `get_parameter_object`, before any preset load
    /// or session restore. Sampling later — which is what
    /// [`parameter_list`](Self::parameter_list) used to do, reporting the live
    /// value as the default — makes "default" follow the user's last knob move.
    pub(crate) initial_values: Vec<f32>,
    /// Host-callback channel endpoints + the shared transport snapshot.
    pub(crate) host_link: HostLink,
    /// Per-block MIDI plumbing (host→plugin staging, plugin→host drain).
    pub(crate) midi: MidiIo,
    metadata: PluginInfo,
    /// Whether the last `effMainsChanged` we dispatched carried `value=1`.
    ///
    /// Tracked because `effMainsChanged` is not documented as idempotent and
    /// real plugins reallocate rate-dependent buffers on every `resume(1)`:
    /// [`suspend_for_reconfigure`](Self::suspend_for_reconfigure) and
    /// [`restore_after_reconfigure`](Self::restore_after_reconfigure) use it to
    /// make both transitions edge-triggered, where the setters previously ran
    /// an unconditional `suspend(); set(); resume()`.
    resumed: bool,
}

// SAFETY: every field is either `Send` or its non-`Send`-ness has been
// addressed via a wrapper (`SendEditor`, `SendParams`). The raw pointers
// inside `vst::host::PluginInstance` point at heap memory we own;
// callers serialize access externally (subprocess server is single-
// threaded; in-process backend uses `parking_lot::Mutex`). `Sync` is
// needed by the in-process callers — fundsp's `dyn AudioUnit` requires
// `Send + Sync` and the audio unit holds `Arc<Mutex<Vst2Instance>>`.
unsafe impl Send for Vst2Instance {}
unsafe impl Sync for Vst2Instance {}

impl Vst2Instance {
    /// Load a VST2 bundle and run the full init/resume sequence.
    ///
    /// On macOS `.vst` is a bundle directory; this constructor probes
    /// `Contents/MacOS/` for the actual binary. Plain shared-library
    /// paths (Linux `.so`, Windows `.dll`) are used as-is.
    ///
    /// `block_size` is the maximum number of samples per process call;
    /// callers may render fewer per call, but never more.
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        // Loading runs the plugin's init/resume sequence and probes its
        // editor — VST2 requires this happen on the host main thread. A
        // no-op until `mark_main_thread()` has been called (headless tests
        // are safe).
        tutti_plugin_types::assert_main_thread();

        let resolved = resolve_bundle(path);

        let (param_tx, param_rx) = crossbeam_channel::unbounded();
        let (midi_out_tx, midi_out_rx) = crossbeam_channel::unbounded();
        let time_info = Arc::new(TransportCell::new());
        // A bare `Arc`, not `Arc<Mutex<_>>`: the plugin calls
        // `audioMasterGetTime` from inside `processReplacing` on the audio
        // thread and `audioMasterSizeWindow` / `audioMasterUpdateDisplay` from
        // the GUI thread, so a shared lock here is a priority inversion.
        // `HostState`'s fields are already lock-free (a seqlock + crossbeam
        // senders), so the lock bought nothing.
        let host = Arc::new(HostState::new(
            param_tx,
            midi_out_tx,
            Arc::clone(&time_info),
            block_size,
            sample_rate,
        ));

        let mut loader = PluginLoader::load(&resolved, Arc::clone(&host)).map_err(|e| {
            Vst2Error::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Factory,
                reason: format!("PluginLoader::load failed: {:?}", e),
            }
        })?;

        let mut instance = loader.instance().map_err(|e| Vst2Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Instantiation,
            reason: format!("loader.instance failed: {:?}", e),
        })?;

        instance.init();
        instance.set_sample_rate(sample_rate as f32);
        instance.set_block_size(block_size as i64);

        // Read before `resume` because the precision announcement below has to
        // happen while the plugin is still suspended.
        let info = instance.get_info();

        // `effSetProcessPrecision` is a suspended-state opcode, and a plugin
        // that switches its internal precision on it reallocates the same
        // buffers `effMainsChanged` does — so it goes here, between
        // `effSetBlockSize` and the first resume.
        //
        // Announcing what the plugin declared, rather than a fixed width: this
        // host renders through whichever entry point the caller asks for, and
        // `process_f64` narrows to f32 exactly when the plugin cannot do f64
        // (see `process.rs`). So the widest width the plugin will ever be
        // entered at is the one its own `effFlagsCanDoubleReplacing` claims.
        // Declaring 64-bit to a plugin that cannot do it would configure it for
        // a call it never receives.
        instance.set_precision(info.f64_precision);

        instance.resume();

        // MIDI classification. Pin count / category is the primary signal, but
        // MIDI-effect plugins routinely declare 0 MIDI pins and advertise
        // capability only via `canDo`, so those weaker signals are OR-ed in.
        use vst::api::Supported;
        use vst::plugin::CanDo;

        // `effCanDo` has three answers and they are not interchangeable: `1` =
        // yes, `0` = "don't know", `-1` = explicitly no. Folding `No` in with
        // `Maybe` and then OR-ing the inference let a plugin answering `-1` be
        // classified as MIDI-capable anyway the moment it declared
        // `Category::Synth`. So `Yes` asserts, `No` vetoes, and the rest defer
        // to `inferred` — including `Custom(n)`, an undocumented integer that
        // is neither an affirmative nor a refusal worth acting on.
        fn resolve(answer: Supported, inferred: bool) -> bool {
            match answer {
                Supported::Yes => true,
                Supported::No => false,
                Supported::Maybe | Supported::Custom(_) => inferred,
            }
        }

        // Pin counts come from the live opcodes, not from `info`: `get_info()`
        // hardcodes both to 0 (its snapshot predates `effOpen`, and the fork
        // never filled them), so reading them there made every pin term a dead
        // `false`. That was not merely a missing signal for `emits_midi` — it
        // was its *only* inferred term, so a plugin answering `Maybe` to
        // `sendVstMidiEvent` resolved to `false` and had its MIDI output
        // dropped.
        //
        // A declined opcode is `None`, which is distinct from `Some(0)` and
        // from a declared pin. Absence must not read as a denial: it leaves the
        // pin term contributing nothing, so `Maybe` falls through to the
        // remaining evidence rather than to `false`.
        let midi_channels = instance.read_midi_channels();
        let midi_in_pins = midi_channels.inputs.is_some_and(|n| n > 0);
        let midi_out_pins = midi_channels.outputs.is_some_and(|n| n > 0);

        let receives_midi = resolve(
            instance.can_do(CanDo::ReceiveMidiEvent),
            midi_in_pins || midi_out_pins || matches!(info.category, Category::Synth),
        );
        // A plugin that declares MIDI output pins but only answers `Maybe` to
        // `sendVstMidiEvent` is emitting MIDI; a plugin that declares none and
        // says `Maybe` is an ordinary effect. `Category::Synth` is deliberately
        // *not* an inference here — a synth emitting audio says nothing about
        // whether it emits MIDI, and treating it as evidence would classify
        // every instrument as a MIDI source.
        let emits_midi = resolve(instance.can_do(CanDo::SendMidiEvent), midi_out_pins);
        let metadata = PluginInfo {
            id: format!("vst2.{}", info.unique_id),
            name: info.name.clone(),
            vendor: info.vendor.clone(),
            version: info.version.to_string(),
            num_inputs: ChannelLayout::from(info.inputs.max(0) as u16),
            num_outputs: ChannelLayout::from(info.outputs.max(0) as u16),
            category: map_category(info.category, info.category_code),
            receives_midi,
            emits_midi,
            has_editor: false, // overwritten below once we ask the handle
            // Read live, not from `info`. `get_info()` returns a snapshot taken
            // in `PluginInstance::new` — before `effOpen`, `effSetSampleRate`
            // and `effMainsChanged` — and a plugin sets its latency during
            // those: a linear-phase EQ does not know its filter length until it
            // knows the sample rate. So `info.initial_delay` reads 0 for
            // exactly the plugins that have latency, and PDC silently
            // compensated nothing for them.
            latency_samples: Samples(instance.read_initial_delay().max(0) as usize),
            supports_f64: info.f64_precision,
        };

        let params = SendParams(instance.get_parameter_object());
        // The default snapshot. Taken HERE, before the handle is built and
        // before any caller can reach `set_parameter` or load a preset — see
        // `initial_values` for why this is the only observable default in
        // VST 2.4. A plugin exposing no `getParameter` yields 0.0, the same
        // neutral the listing path already uses.
        let initial_values: Vec<f32> = (0..info.parameters)
            .map(|i| params.get_parameter(i).unwrap_or(0.0))
            .collect();
        let handle = Vst2Handle::new(instance);
        let mut metadata = metadata;
        metadata.has_editor = handle.has_editor();

        Ok(Self {
            handle,
            params,
            initial_values,
            host_link: HostLink {
                state: host,
                time_info,
                param_rx,
            },
            midi: MidiIo::new(midi_out_rx),
            metadata,
            // `load` dispatched `resume()` above.
            resumed: true,
        })
    }

    /// Plugin metadata snapshot captured at load time.
    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    /// Whether the plugin is currently resumed (processing enabled).
    pub fn is_resumed(&self) -> bool {
        self.resumed
    }

    /// Take the plugin out of the processing state, returning whether a suspend
    /// was actually dispatched.
    ///
    /// Idempotent: a no-op when already suspended. VST 2.4 does not document
    /// `effMainsChanged` as idempotent and real plugins free or reallocate
    /// buffers on each transition, so the host must not issue a redundant one.
    pub fn suspend(&mut self) -> bool {
        self.suspend_for_reconfigure()
    }

    /// Put the plugin back into the processing state. Idempotent for the same
    /// reason as [`suspend`](Self::suspend); returns whether a resume was
    /// actually dispatched.
    pub fn resume(&mut self) -> bool {
        if self.resumed {
            return false;
        }
        self.restore_after_reconfigure(true);
        true
    }

    /// Cycle the plugin through suspend and resume — `effStopProcess` →
    /// `effMainsChanged(0)` → `effMainsChanged(1)` → `effStartProcess` — to
    /// clear whatever processing state it chooses to clear on those edges.
    ///
    /// This is as close as VST 2.4 comes to a state clear, and it is not close.
    /// The `OpCode` enum has no discrete reset. The only two opcodes that touch
    /// DSP state are `effMainsChanged`, where plugins allocate and free their
    /// rate-dependent buffers, and the `effStartProcess`/`effStopProcess` pair,
    /// which announces a processing interruption and is only legal while
    /// resumed. A plugin is obliged to clear nothing on either edge, so a
    /// caller gets the cycle, not a guarantee.
    ///
    /// Returns whether the cycle was dispatched. `false` for a plugin that was
    /// already suspended: it has no processing state to interrupt, and
    /// `effMainsChanged` is not documented as idempotent, so a redundant
    /// suspend would be a second buffer teardown rather than a no-op.
    ///
    /// # Threading
    /// Main thread only, asserted. `effMainsChanged` allocates, so no
    /// audio-thread caller may reach this — a host handling a locate or a loop
    /// wrap calls it from the thread that owns the editor, between blocks.
    pub fn reset_processing_state(&mut self) -> bool {
        tutti_plugin_types::assert_main_thread();
        let was_resumed = self.suspend_for_reconfigure();
        self.restore_after_reconfigure(was_resumed);
        was_resumed
    }

    /// Take the plugin out of the processing state, if it is in it, in the
    /// SDK's teardown order: `effStopProcess` → `effMainsChanged(0)`.
    ///
    /// Returns whether a suspend was actually issued, so the caller can restore
    /// exactly the state it found rather than assuming it was resumed.
    fn suspend_for_reconfigure(&mut self) -> bool {
        if !self.resumed {
            return false;
        }
        // Only legal while resumed, per `Plugin::start_process`'s contract —
        // hence inside this branch, not above it.
        self.handle.instance.stop_process();
        self.handle.instance.suspend();
        self.resumed = false;
        true
    }

    /// Inverse of [`suspend_for_reconfigure`](Self::suspend_for_reconfigure),
    /// in the SDK's startup order: `effMainsChanged(1)` → `effStartProcess`.
    ///
    /// `was_resumed` is that function's return value. Passing `false` leaves the
    /// plugin suspended — a reconfigure must not start a stopped plugin.
    fn restore_after_reconfigure(&mut self, was_resumed: bool) {
        if !was_resumed || self.resumed {
            return;
        }
        self.handle.instance.resume();
        self.resumed = true;
        self.handle.instance.start_process();
    }

    /// Notify the plugin of a sample-rate change.
    ///
    /// The VST2 SDK requires the plugin be suspended around a rate change —
    /// many plugins reallocate rate-dependent buffers in `effSetSampleRate`
    /// and assume they are not concurrently processing. We bracket the call so
    /// callers don't have to. The bracket is edge-triggered (see
    /// [`suspend_for_reconfigure`](Self::suspend_for_reconfigure)), so a plugin
    /// suspended on entry stays suspended on exit.
    ///
    /// `as f32` is not a unit-type regression: `effSetSampleRate` passes the
    /// rate in the dispatcher's `opt` field, a C `float` — an FFI boundary.
    ///
    /// # Threading
    /// Main thread only, asserted. The bracket's `effMainsChanged` is where
    /// plugins allocate and free, so this is not reachable from an audio-thread
    /// caller — one wanting to change rate parks it for a main-thread drain.
    /// The guard is here rather than only at the call sites because this is the
    /// function that dispatches: a future caller inherits it without knowing to
    /// ask.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        tutti_plugin_types::assert_main_thread();
        let was_resumed = self.suspend_for_reconfigure();
        self.handle.instance.set_sample_rate(sample_rate as f32);
        self.restore_after_reconfigure(was_resumed);
    }

    /// Set whether the host reports itself as rendering offline.
    ///
    /// Unlike the other three formats there is nothing to push: VST2 carries
    /// this through `audioMasterGetCurrentProcessLevel`, a callback the plugin
    /// makes whenever it likes. So this stores the answer the host will give,
    /// and no plugin can decline it — there is no query to refuse.
    ///
    /// No suspend/resume bracket for the same reason: nothing is delivered to
    /// the plugin at call time, so there is no buffer for it to re-size.
    pub fn set_offline_render(&self, offline: bool) {
        self.host_link.state.set_offline(offline);
    }

    /// Whether the host is currently reporting offline.
    pub fn is_offline_render(&self) -> bool {
        self.host_link.state.is_offline()
    }

    /// Notify the plugin of a maximum-block-size change. Bracketed for the same
    /// reason as [`set_sample_rate`](Self::set_sample_rate) (block size drives
    /// per-block buffer sizing), with the same edge-triggered semantics.
    ///
    /// No in-tree caller today — block size is fixed at [`load`](Self::load) and
    /// the engine re-loads rather than re-sizing — but it stays public as part
    /// of the VST2 host contract.
    pub fn set_block_size(&mut self, block_size: usize) {
        let was_resumed = self.suspend_for_reconfigure();
        self.handle.instance.set_block_size(block_size as i64);
        self.restore_after_reconfigure(was_resumed);
    }

    /// Whether the plugin has asked the host to refresh what it displays,
    /// consuming the request.
    ///
    /// Raised by `audioMasterUpdateDisplay`, which a plugin fires after
    /// changing preset or program from its own editor. VST 2.4 carries no
    /// detail with it, so the answer is to re-read: [`parameter_list`] and the
    /// current values may all have moved.
    ///
    /// Consuming, so a caller polling each frame acts once per request rather
    /// than re-reading forever after the first one.
    ///
    /// [`parameter_list`]: Self::parameter_list
    pub fn take_display_stale(&self) -> bool {
        self.host_link.state.take_display_stale()
    }

    /// The plugin's latency **as it currently stands**.
    ///
    /// Re-read from the live `AEffect` rather than returned from the load-time
    /// metadata, because VST2 gives a plugin no way to announce a change: there
    /// is no latency-changed callback in the ABI. `audioMasterIOChanged` is the
    /// nearest thing and is about I/O configuration; a plugin that alters
    /// `initialDelay` on a sample-rate change may not send anything at all.
    ///
    /// So the host has to ask. [`set_sample_rate`](Self::set_sample_rate) and
    /// [`set_block_size`](Self::set_block_size) both suspend and resume, which
    /// is exactly when a plugin recomputes a filter length — call this after
    /// either and re-plan compensation if the answer moved.
    pub fn latency(&self) -> Samples {
        Samples(self.handle.instance.read_initial_delay().max(0) as usize)
    }

    /// What the plugin answers for its MIDI channel counts, right now.
    ///
    /// `None` on a field means the plugin declined the opcode, which is
    /// **not** the same as answering zero — see
    /// [`MidiChannelCounts`](vst::host::MidiChannelCounts). The load-time read
    /// of these is what [`metadata`](Self::metadata)'s `receives_midi` /
    /// `emits_midi` are inferred from; this exposes the raw answer for a caller
    /// that needs the distinction rather than the verdict.
    pub fn midi_channel_counts(&self) -> vst::host::MidiChannelCounts {
        self.handle.instance.read_midi_channels()
    }
}

/// Resolve a `.vst` bundle directory to its inner Mach-O / ELF binary.
///
/// Plain files and nonexistent paths pass through unchanged — the caller
/// surfaces the load failure with its own diagnostic. Bundle layouts:
/// macOS `Contents/MacOS/<stem>`, Linux `Contents/x86_64-linux/<stem>.so`,
/// Windows `Contents/x86_64-win/<stem>.dll`.
pub fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() || !path.is_dir() {
        return path.to_path_buf();
    }

    #[cfg(target_os = "macos")]
    let resolved = probe_subdir(path, "MacOS", None);

    #[cfg(target_os = "linux")]
    let resolved = probe_subdir(path, "x86_64-linux", Some("so"));

    #[cfg(target_os = "windows")]
    let resolved = probe_subdir(path, "x86_64-win", Some("dll"));

    resolved.unwrap_or_else(|| path.to_path_buf())
}

fn probe_subdir(bundle: &Path, arch_dir: &str, ext: Option<&str>) -> Option<PathBuf> {
    let dir = bundle.join("Contents").join(arch_dir);
    let stem = bundle.file_stem()?;

    let candidate = dir.join(stem);
    if candidate.is_file() {
        return Some(candidate);
    }

    if let Some(ext) = ext {
        let with_ext = dir.join(format!("{}.{}", stem.to_str()?, ext));
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }

    if let Some(name) = bundle.file_name() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}
