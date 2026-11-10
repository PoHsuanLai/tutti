//! The plugin after `effOpen` and before `effMainsChanged(1)`: [`Vst2Loaded`].
//!
//! [`Vst2Loaded::load`] opens the plugin and reads what it declares (pins,
//! parameters, programs, MIDI capability). Nothing is rendered until
//! [`Loaded::activate`](tutti_plugin_types::Loaded::activate) splits it into a
//! [`Vst2Processor`](crate::Vst2Processor) and a
//! [`Vst2Controller`](crate::Vst2Controller); the rate, block size and
//! precision are that activation's, and `effSetSampleRate`, `effSetBlockSize`
//! and `effSetProcessPrecision` are sent there, while the plugin is suspended.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tutti_plugin_types::MainThread;

use vst::host::{PluginInstance, PluginLoader};
use vst::plugin::Category;

use tutti_plugin_types::{PluginDescriptor, Request};

use crate::error::{LoadStage, Result, Vst2Error};
use crate::host::{HostState, MIDI_OUT_QUEUE_CAPACITY, PARAM_QUEUE_CAPACITY, ParameterChange};
use crate::midi::MidiIo;
use crate::transport_cell::TransportWriter;
use crate::types::{ChannelLayout, PluginInfo, PluginTail, Samples, Vst2Category};

/// The rate and block size the plugin is told nothing of before its first
/// activation, and answers `audioMasterGetSampleRate` / `GetBlockSize` with
/// until then.
const UNACTIVATED_RATE: f64 = 44_100.0;
const UNACTIVATED_BLOCK: usize = 512;

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
/// which is why this cannot go through [`PluginTail::from_vst3_samples`]: that maps
/// `0 => None` for CLAP and VST3, so a VST2 answer fed through it reports
/// "unknown" as "silent". A bounce sizing its render from that adds no decay
/// and truncates the reverb — the failure this decode exists to prevent.
///
/// A negative is not a length. VST2 gives no meaning to one, so it is read as
/// "the plugin said nothing intelligible" rather than clamped to zero, which
/// would be indistinguishable from a declared silence.
pub(crate) fn decode_tail(raw: isize) -> PluginTail {
    match raw {
        0 => PluginTail::Unknown,
        1 => PluginTail::None,
        n if n > 1 => PluginTail::Finite(Samples(n as usize)),
        _ => PluginTail::Unknown,
    }
}

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

/// A loaded VST2 plugin, not rendering.
///
/// Holds the plugin after `effOpen`: its parameters, programs, state and editor
/// are reachable, and [`Loaded::activate`](tutti_plugin_types::Loaded::activate)
/// splits it into the realtime [`Vst2Processor`](crate::Vst2Processor) and the
/// main-thread [`Vst2Controller`](crate::Vst2Controller), which derefs back to
/// this type.
///
/// # Threading
///
/// The type is `Send`, not `Sync`: the plugin behind it is not thread-safe, and
/// the caller owns it on one thread at a time. Methods documented as main-thread
/// only take a [`MainThread`], the proof that the caller is on the process's
/// main thread.
///
/// Dropping it closes the editor, stops processing if it is on, suspends the plugin if it
/// is resumed and dispatches `effClose`, all in the drop of its
/// [`PluginInstance`](vst::host::PluginInstance), **on the thread it was loaded on**. A
/// drop has no token, so the instance records that thread and, dropped on any other, leaks
/// the plugin rather than closing it (see the instance's docs). The plugin's shared
/// library is never unloaded, because unloading runs static destructors that
/// crash many JUCE-based plugins.
///
/// Not `Sync`: the instance behind it is not, and an entry the format puts on the main
/// thread takes a [`MainThread`] (see [`PluginInstance`](vst::host::PluginInstance)).
///
/// The doc test below is the guard (mutation, run: an `unsafe impl Sync` for the
/// type makes it compile, and the test fails):
///
/// ```compile_fail
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<tutti_vst2_host::Vst2Loaded>();
/// ```
///
/// An entry the format puts on the main thread takes the proof, so safe code without one
/// cannot make the call (mutation, run: give `set_bypass` back its token-free signature and
/// the doctest compiles and fails):
///
/// ```compile_fail,E0061
/// fn go(loaded: &mut tutti_vst2_host::Vst2Loaded) {
///     loaded.set_bypass(true);
/// }
/// ```
///
/// # Examples
///
/// ```no_run
/// use tutti_vst2_host::Vst2Loaded;
///
/// let main = tutti_plugin_types::MainThread::claim().expect("on the UI thread");
/// let plugin = Vst2Loaded::load(&main, "/usr/lib/vst/MyPlugin.so")?;
/// println!("{} by {}", plugin.metadata().name, plugin.metadata().vendor);
/// # Ok::<(), tutti_vst2_host::Vst2Error>(())
/// ```
pub struct Vst2Loaded {
    /// The main-thread handle on the plugin: the one owner, which closes it when it drops.
    pub(crate) instance: PluginInstance,
    /// Each parameter's value as read once at load, indexed by parameter id.
    ///
    /// VST 2.4 has **no** opcode that reports a default — none of the 61 in
    /// `OpCode` returns one, and `effGetParameterProperties` (56) carries a
    /// range and step granularity but no default either. What a plugin *does*
    /// have is its own initial state: a freshly instantiated plugin sits at its
    /// defaults, so reading each parameter once before anything writes to it is
    /// the only place that value is observable.
    ///
    /// Hence the ordering invariant on [`Vst2Loaded::load`]: this snapshot is
    /// taken immediately after the instance is made, before any preset load or
    /// session restore. Sampling later reports the *live* value as the default,
    /// which makes "default" follow the user's last knob move.
    pub(crate) initial_values: Vec<f32>,
    /// The host's end of the plugin's callbacks, shared with the plugin through
    /// the `Host` it was loaded with.
    pub(crate) host: Arc<HostState>,
    /// Inbox for `audioMasterAutomate` parameter changes (editor knob moves).
    ///
    /// The same queue [`HostState`] pushes into — an `ArrayQueue` is MPMC, so
    /// both ends share one `Arc` rather than splitting into sender/receiver
    /// halves.
    pub(crate) param_rx: Arc<crossbeam_queue::ArrayQueue<ParameterChange>>,
    /// The audio side's state, which a Processor owns while the plugin is
    /// active and which a rejoin gives back. `None` while a Processor holds it.
    pub(crate) audio: Option<AudioSide>,
    metadata: PluginInfo,
    descriptor: PluginDescriptor,
}

/// What the realtime half keeps between activations: the transport snapshot the
/// plugin reads back (whose last value is the next block's predecessor) and the
/// MIDI plumbing with its counters.
pub(crate) struct AudioSide {
    /// The one writer of the transport snapshot the plugin reads via
    /// `get_time_info` ([`HostState`] holds the read side). A seqlock, not an
    /// `ArcSwap`: the push happens on the audio thread every block, and
    /// `ArcSwap::store` would allocate the new value and free the retired one
    /// there. See [`TransportWriter`].
    pub(crate) time_info: TransportWriter,
    /// Per-block MIDI plumbing (host→plugin staging, plugin→host drain).
    pub(crate) midi: MidiIo,
}

impl std::fmt::Debug for Vst2Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst2Loaded")
            .field("initial_values", &self.initial_values.len())
            .field("metadata", &self.metadata)
            .field("resumed", &self.instance.is_resumed())
            .finish_non_exhaustive()
    }
}

impl Vst2Loaded {
    /// Opens a VST2 plugin: `effOpen`, and a read of what it declares.
    ///
    /// `path` may be a `.vst` bundle directory (the native binary inside it is
    /// found automatically) or a plain shared library (`.so`, `.dll`, or a
    /// Mach-O file). The plugin is not told a rate, block size or precision, and
    /// is not resumed: those are the activation's. Parameter defaults are
    /// captured here, before anything can write to the plugin (see
    /// [`ParameterInfo`](crate::ParameterInfo)).
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::LoadFailed`] if the file cannot be opened as a VST2
    /// module ([`LoadStage::Factory`]) or the plugin's entry point fails to
    /// produce an instance ([`LoadStage::Instantiation`]).
    ///
    pub fn load(main: &MainThread, path: impl AsRef<Path>) -> Result<Self> {
        // Loading runs the plugin's init sequence and probes its editor — VST2
        // requires this happen on the host main thread, which `main` proves.

        let path = path.as_ref();
        let resolved = resolve_bundle(path);

        // Bounded, allocated once here. `automate` and `process_events` push
        // into these from inside the plugin's `processReplacing`, so neither
        // may allocate or grow — see `host.rs`'s *Why the queues are bounded*.
        // MPMC, so each queue is one `Arc` shared by both ends rather than a
        // sender/receiver pair.
        let param_q = Arc::new(crossbeam_queue::ArrayQueue::new(PARAM_QUEUE_CAPACITY));
        let midi_out_q = Arc::new(crossbeam_queue::ArrayQueue::new(MIDI_OUT_QUEUE_CAPACITY));
        let time_info = TransportWriter::new();
        // A bare `Arc`, not `Arc<Mutex<_>>`: the plugin calls
        // `audioMasterGetTime` from inside `processReplacing` on the audio
        // thread and `audioMasterSizeWindow` / `audioMasterUpdateDisplay` from
        // the GUI thread, so a shared lock here is a priority inversion.
        // `HostState`'s fields are already lock-free (a seqlock, a request word
        // and two `ArrayQueue`s), so the lock bought nothing.
        let host = Arc::new(HostState::new(
            Arc::clone(&param_q),
            Arc::clone(&midi_out_q),
            time_info.reader(),
            UNACTIVATED_BLOCK,
            UNACTIVATED_RATE,
        ));

        let mut loader = PluginLoader::load(&resolved, Arc::clone(&host)).map_err(|e| {
            Vst2Error::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Factory,
                reason: format!("PluginLoader::load failed: {:?}", e),
            }
        })?;

        let mut instance = loader.instance(main).map_err(|e| Vst2Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Instantiation,
            reason: format!("loader.instance failed: {:?}", e),
        })?;

        instance.init(main);

        // The pin counts are settled in `effOpen`: a count no layout holds
        // fails the load here, naming the side, rather than wrapping later.
        let (pin_inputs, pin_outputs) = instance.read_pin_counts();
        check_pin_counts(pin_inputs, pin_outputs).map_err(|reason| Vst2Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Initialization,
            reason,
        })?;

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
        let midi_channels = instance.read_midi_channels(main);
        let midi_in_pins = midi_channels.inputs.is_some_and(|n| n > 0);
        let midi_out_pins = midi_channels.outputs.is_some_and(|n| n > 0);

        let receives_midi = resolve(
            instance.can_do(main, CanDo::ReceiveMidiEvent),
            midi_in_pins || midi_out_pins || matches!(info.category, Category::Synth),
        );
        // A plugin that declares MIDI output pins but only answers `Maybe` to
        // `sendVstMidiEvent` is emitting MIDI; a plugin that declares none and
        // says `Maybe` is an ordinary effect. `Category::Synth` is deliberately
        // *not* an inference here — a synth emitting audio says nothing about
        // whether it emits MIDI, and treating it as evidence would classify
        // every instrument as a MIDI source.
        let emits_midi = resolve(instance.can_do(main, CanDo::SendMidiEvent), midi_out_pins);
        let mut metadata = PluginInfo {
            id: format!("vst2.{}", info.unique_id),
            name: info.name.clone(),
            vendor: info.vendor.clone(),
            version: info.version.to_string(),
            category: map_category(info.category, info.category_code),
            receives_midi,
            emits_midi,
            has_editor: false, // overwritten below, once the handle is asked
            supports_f64: info.f64_precision,
        };

        // The default snapshot. Taken HERE, before anything else can reach the plugin and
        // before any caller can reach `set_parameter` or load a preset — see
        // `initial_values` for why this is the only observable default in
        // VST 2.4. A plugin exposing no `getParameter` yields 0.0, the same
        // neutral the listing path already uses.
        let initial_values: Vec<f32> = (0..instance.read_num_params().max(0))
            .map(|i| instance.get_parameter(i).unwrap_or(0.0))
            .collect();
        metadata.has_editor = instance.has_editor();
        let descriptor = crate::control::descriptor_of(&metadata);

        Ok(Self {
            instance,
            initial_values,
            host,
            param_rx: param_q,
            audio: Some(AudioSide {
                time_info,
                midi: MidiIo::new(midi_out_q),
            }),
            metadata,
            descriptor,
        })
    }

    /// What the plugin declared at load. The pin counts, latency and tail it
    /// reports now are [`pins`](Self::pins), [`latency_samples`](Self::latency_samples)
    /// and [`tail`](Self::tail): they move after an activation, and this does
    /// not.
    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    /// The catalog descriptor: identity, class and whether it has an editor.
    pub(crate) fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// The declared audio widths, `(inputs, outputs)`, read from the plugin now:
    /// a plugin settles them in `effOpen` and may change them, and tells the host
    /// with `audioMasterIOChanged`.
    #[must_use]
    pub fn pins(&self) -> (ChannelLayout, ChannelLayout) {
        let (inputs, outputs) = self.instance.read_pin_counts();
        // `pins` is an infallible re-read with no error path to refuse through:
        // a negative count is no channels and one past `u16::MAX` saturates
        // there (a plugin that wide fails wherever it is wired), never wraps.
        let width = |count: i32| ChannelLayout::clamped(usize::try_from(count).unwrap_or(0));
        (width(inputs), width(outputs))
    }

    /// Returns `true` while the plugin is resumed (`effMainsChanged(1)`): from an
    /// activation to the rejoin that ends it.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.instance.is_resumed()
    }

    /// Sets whether the host reports itself as rendering offline.
    ///
    /// Unlike the other three formats there is nothing to push: VST2 carries
    /// this through `audioMasterGetCurrentProcessLevel`, a callback the plugin
    /// makes whenever it likes. So this stores the answer the host will give,
    /// and no plugin can decline it — there is no query to refuse. That is also
    /// why a render-mode change needs no restart here.
    ///
    /// `&self` and atomic: the plugin may ask from its audio thread while the
    /// control thread sets it.
    pub fn set_offline_render(&self, offline: bool) {
        self.host.set_offline(offline);
    }

    /// Returns `true` while the host reports itself as rendering offline.
    #[must_use]
    pub fn is_offline_render(&self) -> bool {
        self.host.is_offline()
    }

    /// Takes the requests the plugin raised since the last call, in the order
    /// [`taken`](tutti_plugin_types::taken) yields them, into `out`.
    ///
    /// VST 2.4 has one notice, `audioMasterIOChanged`, for the pins, the latency
    /// and the tail alike, so it raises [`Request::IoChanged`],
    /// [`Request::LatencyChanged`] and [`Request::TailChanged`] together;
    /// `audioMasterUpdateDisplay` raises a rescan of the parameters' values and
    /// of their names. Feed them to [`Lifecycle::drive`](tutti_plugin_types::Lifecycle::drive);
    /// the core reads the figures again rather than restarting the plugin
    /// ([`RestartRules::NONE`](tutti_plugin_types::RestartRules::NONE)).
    pub fn take_requests(&self, _main: &MainThread, out: &mut Vec<Request>) {
        // Control side, so the one warning about a plugin that grew its pins past the
        // activation (S7: it renders silence until re-activated) is logged here.
        let _ = self.instance.report_pin_refusals();
        out.extend(tutti_plugin_types::taken(self.host.requests()));
    }

    /// Returns the plugin's current latency, in samples, read from the live
    /// `AEffect` rather than from anything cached.
    ///
    /// VST2 has no latency-changed callback: `audioMasterIOChanged` is the
    /// nearest thing, and a plugin that alters `initialDelay` on a sample-rate
    /// change may send nothing at all. So the core asks again after every
    /// activation and every request ([`Request::LatencyChanged`]); this is the
    /// read it makes.
    #[must_use]
    pub fn latency_samples(&self) -> Samples {
        Samples(self.instance.read_initial_delay().max(0) as usize)
    }

    /// Returns the plugin's tail, read from the plugin now and decoded from
    /// VST2's inverted convention (see [`PluginInfo`]).
    #[must_use]
    pub fn tail(&self, main: &MainThread) -> PluginTail {
        decode_tail(self.instance.read_tail_size(main))
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
    pub fn programs(&self, main: &MainThread) -> Vec<(i32, String)> {
        let count = self.instance.read_num_programs().max(0);
        (0..count)
            .map(|i| (i, self.instance.get_preset_name(main, i)))
            .collect()
    }

    /// Returns how many programs the plugin declares (`numPrograms`, read live;
    /// `0` for a negative answer).
    ///
    /// [`programs`](Self::programs) without asking for every name: what a
    /// caller that only needs to know whether a program is part of the
    /// plugin's state reads.
    pub fn program_count(&self) -> i32 {
        self.instance.read_num_programs().max(0)
    }

    /// Returns the index of the program the plugin reports as active.
    pub fn current_program(&self, main: &MainThread) -> i32 {
        self.instance.get_preset_num(main)
    }

    /// Switches to program `index`, bracketed by `effBeginSetProgram` /
    /// `effEndSetProgram`.
    ///
    /// The bracket is why this is on the plugin rather than a bare
    /// `params.change_preset`: a switch moves many parameters at once, and
    /// unbracketed each one reaches the host as an individual
    /// `audioMasterAutomate` edit. This host has that callback wired and
    /// draining, so an unbracketed switch really does flood it.
    ///
    /// Returns `false` for an out-of-range index without dispatching anything.
    /// VST2 gives no confirmation that an in-range change took — `effProgramChange`
    /// returns nothing — so `true` means "dispatched", not "the plugin moved".
    pub fn set_program(&mut self, main: &MainThread, index: i32) -> bool {
        if index < 0 || index >= self.instance.read_num_programs() {
            return false;
        }
        self.instance
            .with_preset_bracket(main, |instance| instance.change_preset(main, index));
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
    pub fn midi_channel_counts(&self, main: &MainThread) -> vst::host::MidiChannelCounts {
        self.instance.read_midi_channels(main)
    }

    /// Returns `true` if the plugin advertises its own soft bypass via
    /// `effCanDo("bypass")`.
    ///
    /// Ask before [`set_bypass`](Self::set_bypass): `Maybe` is the common
    /// answer and is not a yes. A plugin that does not advertise one has to be
    /// bypassed by the host instead, by not routing audio through it.
    #[must_use]
    pub fn supports_soft_bypass(&self, main: &MainThread) -> bool {
        use vst::api::Supported;
        use vst::plugin::CanDo;
        matches!(self.instance.can_do(main, CanDo::Bypass), Supported::Yes)
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
    pub fn set_bypass(&mut self, main: &MainThread, bypass: bool) -> bool {
        self.instance.set_bypass(main, bypass)
    }
}

/// Resolves a `.vst` bundle directory to its inner Mach-O / ELF binary.
///
/// Plain files and nonexistent paths pass through unchanged — the caller
/// surfaces the load failure with its own diagnostic. Bundle layouts:
/// macOS `Contents/MacOS/<stem>`, Linux `Contents/x86_64-linux/<stem>.so`,
/// Windows `Contents/x86_64-win/<stem>.dll`.
fn resolve_bundle(path: &Path) -> PathBuf {
    // `native_*`: a VST2 module for another CPU cannot be loaded here, and the
    // caller's own diagnostic on the unchanged path says more than a wrong hit.
    tutti_plugin_types::bundle::native_module_in_bundle(
        path,
        tutti_plugin_types::bundle::ModuleKind::Vst2,
    )
    .unwrap_or_else(|| path.to_path_buf())
}

/// Refuses a pin count a layout cannot hold (negative, or above `u16::MAX`),
/// naming the side. `pins()` saturates for the infallible re-read; the load is
/// where a plugin that wide is turned away.
fn check_pin_counts(inputs: i32, outputs: i32) -> std::result::Result<(), String> {
    for (side, count) in [("input", inputs), ("output", outputs)] {
        if u16::try_from(count).is_err() {
            return Err(format!(
                "the plugin reports {count} {side} channels; at most {} are supported",
                u16::MAX
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod pin_count_tests {
    use super::check_pin_counts;

    /// The widest a layout holds passes; one past it, or a negative count, is
    /// refused naming the side.
    ///
    /// Mutation (run): `check_pin_counts` returning `Ok(())` always (or testing
    /// only `inputs`) fails the refusals here.
    #[test]
    fn a_pin_count_no_layout_holds_is_refused_naming_the_side() {
        assert!(check_pin_counts(0, i32::from(u16::MAX)).is_ok());
        let err = check_pin_counts(2, 65_536).unwrap_err();
        assert!(err.contains("65536 output"), "{err}");
        let err = check_pin_counts(65_538, 2).unwrap_err();
        assert!(err.contains("65538 input"), "{err}");
        assert!(check_pin_counts(-1, 2).unwrap_err().contains("-1 input"));
        assert!(check_pin_counts(2, i32::MAX).is_err());
    }
}
