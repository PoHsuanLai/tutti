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
        instance.resume();

        let info = instance.get_info();
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

        // `midi_inputs` / `midi_outputs` are hardcoded to 0 in the fork (the
        // host never dispatches `effGetNumMidiInputOutputChannels`), so the pin
        // terms are dead constants today. Kept so that wiring the opcode up is a
        // change to `Info`'s construction alone.
        let midi_pins_declared = info.midi_inputs > 0 || info.midi_outputs > 0;
        let receives_midi = resolve(
            instance.can_do(CanDo::ReceiveMidiEvent),
            midi_pins_declared || matches!(info.category, Category::Synth),
        );
        let emits_midi = resolve(instance.can_do(CanDo::SendMidiEvent), info.midi_outputs > 0);
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
            latency_samples: Samples(info.initial_delay.max(0) as usize),
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
                _state: host,
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
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        let was_resumed = self.suspend_for_reconfigure();
        self.handle.instance.set_sample_rate(sample_rate as f32);
        self.restore_after_reconfigure(was_resumed);
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
