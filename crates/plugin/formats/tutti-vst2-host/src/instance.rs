//! High-level VST2 plugin instance.
//!
//! [`Vst2Instance`] is the main entry point. Construct via
//! [`Vst2Instance::load`]; the constructor runs the full VST2 init/resume
//! sequence so the returned instance is immediately usable for
//! [`process_f32`](Self::process_f32) / [`process_f64`](Self::process_f64).
//!
//! The lifecycle is short and atomic — `init → set_sample_rate →
//! set_block_size → resume` all happen at construction — so there is no useful
//! state between "ready to load editor / params" and "ready to process audio",
//! and the type carries no lifecycle stage. Suspend and resume are a
//! reconfiguration bracket around a rate or block-size change, not a stage a
//! host parks in; the reasoning for both is in the crate docs under *Why one
//! type carries the whole lifecycle*.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vst::host::PluginLoader;
use vst::plugin::{Category, Plugin as _};

use crate::error::{LoadStage, Result, Vst2Error};
use crate::handle::Vst2Handle;
use crate::host::{HostLink, HostState, MIDI_OUT_QUEUE_CAPACITY, PARAM_QUEUE_CAPACITY};
use crate::midi::MidiIo;
use crate::parameters::SendParams;
use crate::transport_cell::TransportCell;
use crate::types::{ChannelLayout, PluginInfo, PluginTail, Samples, Vst2Category};

/// Maps the `vst` crate's `Category` to the shared [`Vst2Category`] mirror.
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
/// Decode `effGetTailSize` into the shared [`PluginTail`] vocabulary.
///
/// **VST2 is the one format whose zero does not mean "no tail".** The 2.4 spec
/// reads the wire value as:
///
/// | raw | meaning                              | decoded            |
/// |-----|--------------------------------------|--------------------|
/// | `0` | no tail *information*; host decides  | `Unknown`          |
/// | `1` | no tail at all                       | `None`             |
/// | `n` | `n` samples of ring-out              | `Finite(n)`        |
///
/// which is why this cannot go through [`PluginTail::from_samples`]: that maps
/// `0 => None` for CLAP and VST3, so a VST2 answer fed through it reports
/// "unknown" as "silent". A bounce sizing its render from that adds no decay
/// and truncates the reverb — the failure this decode exists to prevent.
///
/// A negative is not a length. VST2 gives no meaning to one, so it is read as
/// "the plugin said nothing intelligible" rather than clamped to zero, which
/// would be indistinguishable from a declared silence.
fn decode_tail(raw: isize) -> PluginTail {
    match raw {
        0 => PluginTail::Unknown,
        1 => PluginTail::None,
        n if n > 1 => PluginTail::Finite(Samples(n as usize)),
        _ => PluginTail::Unknown,
    }
}

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

/// A loaded, initialized VST2 plugin, ready to process audio.
///
/// Created by [`load`](Self::load), which runs the whole VST2 start-up
/// sequence, so there is no separate "loaded but not active" type. One value
/// carries audio and MIDI processing ([`process_f32`](Self::process_f32),
/// [`process_f64`](Self::process_f64)), parameters, programs, state
/// ([`get_state`](Self::get_state) / [`set_state`](Self::set_state)) and the
/// native editor ([`open_editor`](Self::open_editor)).
///
/// # Threading
///
/// The type is `Send` and `Sync`, but the plugin behind it is not thread-safe:
/// callers must serialize access themselves (for example behind a mutex, or by
/// owning the instance on a single thread). Methods documented as main-thread
/// only must be called from the thread registered with
/// [`tutti_plugin_types::mark_main_thread`]; debug builds assert it.
///
/// Dropping the instance closes the editor, suspends the plugin and dispatches
/// `effClose`. The plugin's shared library is never unloaded, because
/// unloading runs static destructors that crash many JUCE-based plugins.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use tutti_vst2_host::Vst2Instance;
///
/// let plugin = Vst2Instance::load(Path::new("/usr/lib/vst/MyPlugin.so"), 48_000.0, 512)?;
/// println!("{} by {}", plugin.metadata().name, plugin.metadata().vendor);
/// # Ok::<(), tutti_vst2_host::Vst2Error>(())
/// ```
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
    /// taken immediately after `get_parameter_object`, before any preset load or
    /// session restore. Sampling later reports the *live* value as the default,
    /// which makes "default" follow the user's last knob move.
    pub(crate) initial_values: Vec<f32>,
    /// Host-callback channel endpoints + the shared transport snapshot.
    pub(crate) host_link: HostLink,
    /// Per-block MIDI plumbing (host→plugin staging, plugin→host drain).
    pub(crate) midi: MidiIo,
    metadata: PluginInfo,
    /// Whether the last `effMainsChanged` dispatched carried `value=1`.
    ///
    /// Tracked because `effMainsChanged` is not documented as idempotent and real
    /// plugins reallocate rate-dependent buffers on every `resume(1)`. The
    /// reconfigure pair reads this to make both transitions edge-triggered; an
    /// unconditional `suspend(); set(); resume()` would churn those buffers on
    /// every setter call.
    resumed: bool,
}

// SAFETY: every field is either `Send` or its non-`Send`-ness has been
// addressed via a wrapper (`SendEditor`, `SendParams`). The raw pointers
// inside `vst::host::PluginInstance` point at heap memory this crate owns;
// callers serialize access externally (subprocess server is single-
// threaded; in-process backend uses `parking_lot::Mutex`). `Sync` rests on
// the same external serialization; a graph node itself needs only `Send`.
unsafe impl Send for Vst2Instance {}
unsafe impl Sync for Vst2Instance {}

impl Vst2Instance {
    /// Loads a VST2 plugin and runs its full start-up sequence.
    ///
    /// `path` may be a `.vst` bundle directory (the native binary inside it is
    /// found automatically) or a plain shared library (`.so`, `.dll`, or a
    /// Mach-O file). `sample_rate` is in Hz. `block_size` is the maximum number
    /// of samples per process call; callers may render fewer per call, but
    /// never more.
    ///
    /// The plugin is opened, configured with the rate and block size, told the
    /// processing precision, and resumed, so the returned instance can process
    /// immediately. Parameter defaults are captured here, before anything can
    /// write to the plugin (see [`ParameterInfo`](crate::ParameterInfo)).
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::LoadFailed`] if the file cannot be opened as a VST2
    /// module ([`LoadStage::Factory`]) or the plugin's entry point fails to
    /// produce an instance ([`LoadStage::Instantiation`]).
    ///
    /// # Panics
    ///
    /// Main thread only. In debug builds, panics if called off the thread
    /// registered with [`tutti_plugin_types::mark_main_thread`].
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        // Loading runs the plugin's init/resume sequence and probes its
        // editor — VST2 requires this happen on the host main thread. A
        // no-op until `mark_main_thread()` has been called (headless tests
        // are safe).
        tutti_plugin_types::assert_main_thread();

        let resolved = resolve_bundle(path);

        // Bounded, allocated once here. `automate` and `process_events` push
        // into these from inside the plugin's `processReplacing`, so neither
        // may allocate or grow — see `host.rs`'s *Why the queues are bounded*.
        // MPMC, so each queue is one `Arc` shared by both ends rather than a
        // sender/receiver pair.
        let param_q = Arc::new(crossbeam_queue::ArrayQueue::new(PARAM_QUEUE_CAPACITY));
        let midi_out_q = Arc::new(crossbeam_queue::ArrayQueue::new(MIDI_OUT_QUEUE_CAPACITY));
        let time_info = Arc::new(TransportCell::new());
        // A bare `Arc`, not `Arc<Mutex<_>>`: the plugin calls
        // `audioMasterGetTime` from inside `processReplacing` on the audio
        // thread and `audioMasterSizeWindow` / `audioMasterUpdateDisplay` from
        // the GUI thread, so a shared lock here is a priority inversion.
        // `HostState`'s fields are already lock-free (a seqlock + two
        // `ArrayQueue`s), so the lock bought nothing.
        let host = Arc::new(HostState::new(
            Arc::clone(&param_q),
            Arc::clone(&midi_out_q),
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
        // does not fill them), so reading them there would make every pin term
        // a dead `false`. For `emits_midi` that would be its *only* inferred
        // term, so a plugin answering `Maybe` to `sendVstMidiEvent` would
        // resolve to `false` and have its MIDI output dropped.
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
            has_editor: false, // overwritten below, once the handle is asked
            // Read live, not from `info`. `get_info()` returns a snapshot taken
            // in `PluginInstance::new` — before `effOpen`, `effSetSampleRate`
            // and `effMainsChanged` — and a plugin sets its latency during
            // those: a linear-phase EQ does not know its filter length until it
            // knows the sample rate. So `info.initial_delay` reads 0 for
            // exactly the plugins that have latency, and PDC silently
            // compensated nothing for them.
            latency_samples: Samples(instance.read_initial_delay().max(0) as usize),
            // Read live for the same reason as the latency above, and decoded
            // here rather than through `PluginTail::from_samples` because VST2
            // inverts the convention — see `decode_tail`.
            tail: decode_tail(instance.read_tail_size()),
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
                param_rx: param_q,
            },
            midi: MidiIo::new(midi_out_q),
            metadata,
            // `load` dispatched `resume()` above.
            resumed: true,
        })
    }

    /// Returns the plugin metadata captured at load time.
    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    /// Returns `true` while the plugin is resumed (processing enabled).
    pub fn is_resumed(&self) -> bool {
        self.resumed
    }

    /// Takes the plugin out of the processing state, returning whether a
    /// suspend was actually dispatched.
    ///
    /// Idempotent: a no-op when already suspended. VST 2.4 does not document
    /// `effMainsChanged` as idempotent and real plugins free or reallocate
    /// buffers on each transition, so the host must not issue a redundant one.
    pub fn suspend(&mut self) -> bool {
        self.suspend_for_reconfigure()
    }

    /// Puts the plugin back into the processing state, returning whether a
    /// resume was actually dispatched.
    ///
    /// Idempotent for the same reason as [`suspend`](Self::suspend).
    pub fn resume(&mut self) -> bool {
        if self.resumed {
            return false;
        }
        self.restore_after_reconfigure(true);
        true
    }

    /// Cycles the plugin through suspend and resume to clear its processing
    /// state, as far as VST2 allows.
    ///
    /// The sequence is `effStopProcess` →
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
    /// # Panics
    ///
    /// Main thread only: `effMainsChanged` allocates, so a host handling a
    /// locate or a loop wrap calls this between blocks, never from the audio
    /// thread. In debug builds, panics if called off the thread registered
    /// with [`tutti_plugin_types::mark_main_thread`].
    pub fn reset_processing_state(&mut self) -> bool {
        tutti_plugin_types::assert_main_thread();
        let was_resumed = self.suspend_for_reconfigure();
        self.restore_after_reconfigure(was_resumed);
        was_resumed
    }

    /// Takes the plugin out of the processing state, if it is in it, in the
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

    /// Changes the plugin's sample rate, in Hz.
    ///
    /// The VST2 SDK requires the plugin be suspended around a rate change — many
    /// plugins reallocate rate-dependent buffers in `effSetSampleRate` and assume
    /// they are not concurrently processing. This brackets the call so a caller
    /// need not. The bracket is edge-triggered, so a plugin suspended on entry
    /// stays suspended on exit.
    ///
    /// The rate reaches the plugin as a C `float`, so it is narrowed to `f32`.
    /// The plugin may change its latency in response; re-read
    /// [`latency`](Self::latency) afterwards.
    ///
    /// # Panics
    ///
    /// Main thread only: the bracket's `effMainsChanged` is where plugins
    /// allocate and free. In debug builds, panics if called off the thread
    /// registered with [`tutti_plugin_types::mark_main_thread`].
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        tutti_plugin_types::assert_main_thread();
        let was_resumed = self.suspend_for_reconfigure();
        self.handle.instance.set_sample_rate(sample_rate as f32);
        self.restore_after_reconfigure(was_resumed);
    }

    /// Sets whether the host reports itself as rendering offline.
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

    /// Returns `true` while the host reports itself as rendering offline.
    pub fn is_offline_render(&self) -> bool {
        self.host_link.state.is_offline()
    }

    /// Changes the maximum number of samples per process call.
    ///
    /// Bracketed by suspend/resume for the same reason as
    /// [`set_sample_rate`](Self::set_sample_rate) (block size drives per-block
    /// buffer sizing), with the same edge-triggered semantics. A
    /// [`RenderScratch`](crate::RenderScratch) sized for the old block size
    /// must be rebuilt if the new one is larger.
    pub fn set_block_size(&mut self, block_size: usize) {
        let was_resumed = self.suspend_for_reconfigure();
        self.handle.instance.set_block_size(block_size as i64);
        self.restore_after_reconfigure(was_resumed);
    }

    /// Returns whether the plugin has asked the host to refresh what it
    /// displays, and clears the request.
    ///
    /// Raised by `audioMasterUpdateDisplay`, which a plugin fires after
    /// changing preset or program from its own editor. VST 2.4 carries no
    /// detail with it, so the answer is to re-read: [`get_parameter_list`] and the
    /// current values may all have moved.
    ///
    /// Consuming, so a caller polling each frame acts once per request rather
    /// than re-reading forever after the first one.
    ///
    /// [`get_parameter_list`]: Self::get_parameter_list
    pub fn take_display_stale(&self) -> bool {
        self.host_link.state.take_display_stale()
    }

    /// Returns the plugin's current latency, in samples.
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

    /// Returns the plugin's programs as `(index, name)` pairs.
    ///
    /// VST2 programs are genuinely positional — `effProgramChange` takes an
    /// index in `[0, numPrograms)` — so unlike AU's sparse selectors the index
    /// *is* the identifier.
    ///
    /// **A plugin may advertise more programs than it will name.** That is the
    /// same enumeration hole `parameter_list` walks on the parameter axis: a
    /// host that trusts `numPrograms` and reads every slot gets names the
    /// plugin never had. An unnamed slot yields an empty string and is kept,
    /// not skipped — dropping it would renumber every program after it, and the
    /// number is the identifier.
    pub fn programs(&self) -> Vec<(i32, String)> {
        let count = self.handle.instance.read_num_programs().max(0);
        (0..count)
            .map(|i| (i, self.params.get_preset_name(i)))
            .collect()
    }

    /// Returns the index of the program the plugin reports as active.
    pub fn current_program(&self) -> i32 {
        self.params.get_preset_num()
    }

    /// Switches to program `index`, bracketed by `effBeginSetProgram` /
    /// `effEndSetProgram`.
    ///
    /// The bracket is why this is on the instance rather than a bare
    /// `params.change_preset`: a switch moves many parameters at once, and
    /// unbracketed each one reaches the host as an individual
    /// `audioMasterAutomate` edit. This host has that callback wired and
    /// draining, so an unbracketed switch really does flood it.
    ///
    /// Returns `false` for an out-of-range index without dispatching anything.
    /// VST2 gives no confirmation that an in-range change took — `effProgramChange`
    /// returns nothing — so `true` means "dispatched", not "the plugin moved".
    pub fn set_program(&mut self, index: i32) -> bool {
        if index < 0 || index >= self.handle.instance.read_num_programs() {
            return false;
        }
        let params = self.params.0.clone();
        self.handle
            .instance
            .with_preset_bracket(|_| params.change_preset(index));
        true
    }

    /// Returns the plugin's current answer for its MIDI channel counts.
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

    /// Returns `true` if the plugin advertises its own soft bypass via
    /// `effCanDo("bypass")`.
    ///
    /// Ask before [`set_bypass`](Self::set_bypass): `Maybe` is the common
    /// answer and is not a yes. A plugin that does not advertise one has to be
    /// bypassed by the host instead, by not routing audio through it.
    pub fn supports_soft_bypass(&self) -> bool {
        use vst::api::Supported;
        use vst::plugin::{CanDo, Plugin as _};
        matches!(self.handle.instance.can_do(CanDo::Bypass), Supported::Yes)
    }

    /// Asks the plugin to enter or leave its own soft bypass.
    ///
    /// Returns whether the plugin accepted. `false` means it refused or does
    /// not implement `effSetBypass` — indistinguishable, since an unimplemented
    /// opcode returns 0, which is also "no".
    ///
    /// Soft bypass is preferable to a host mute where a plugin offers one: the
    /// plugin crossfades and flushes its tail rather than having its reverb cut
    /// mid-decay. It is an *alternative* to the host's own bypass, not a
    /// replacement, so the answer is returned rather than swallowed — a caller
    /// that ignores a `false` leaves the plugin processing while its UI says
    /// bypassed, and that surfaces as "the bypass button does nothing".
    pub fn set_bypass(&mut self, bypass: bool) -> bool {
        self.handle.instance.set_bypass(bypass)
    }
}

/// Resolves a `.vst` bundle directory to its inner Mach-O / ELF binary.
///
/// Plain files and nonexistent paths pass through unchanged — the caller
/// surfaces the load failure with its own diagnostic. Bundle layouts:
/// macOS `Contents/MacOS/<stem>`, Linux `Contents/x86_64-linux/<stem>.so`,
/// Windows `Contents/x86_64-win/<stem>.dll`.
pub fn resolve_bundle(path: &Path) -> PathBuf {
    // `native_*`: a VST2 module for another CPU cannot be loaded here, and the
    // caller's own diagnostic on the unchanged path says more than a wrong hit.
    tutti_plugin_types::bundle::native_module_in_bundle(
        path,
        tutti_plugin_types::bundle::ModuleKind::Vst2,
    )
    .unwrap_or_else(|| path.to_path_buf())
}
