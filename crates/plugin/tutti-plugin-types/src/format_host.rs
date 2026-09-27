//! The fine-grained plugin-instance capability traits every format loader
//! implements, and the [`PluginInstance`] bundle that composes them.
//!
//! A loaded plugin (VST2 / VST3 / CLAP / AU) is a *subprocess-local* object:
//! it lives entirely inside `tutti-plugin-server` and never crosses the IPC
//! wire. The surface is split along the real capability axis, one trait per
//! concern, so a loader implements only the capabilities it has and a consumer
//! depends only on the capability it uses.
//!
//! The audio methods here ([`PluginAudio`]) are the *subprocess* render path,
//! distinct from the host-side graph node (`PluginClient`) on the other end of
//! the wire.
//!
//! # A plugin is a set of capabilities, not a state machine
//!
//! The obvious way to model a plugin host is to start from the lifecycle. Every
//! format has one: a plugin is *loaded*, then *activated* against a sample rate
//! and block size, and only then may it render. Four formats, one shape — it
//! looks like the thing to put in the shared vocabulary.
//!
//! These traits do the opposite. There is no `activate` here, no `deactivate`,
//! and no state enum anywhere in this file. A plugin is described by what it can
//! *do* — [`PluginMeta`], [`PluginAudio`], [`PluginParams`], [`PluginState`],
//! [`PluginPresets`], [`PluginEditorHost`] — and never by what state it is in.
//!
//! The reason is that the shared shape is the only part the formats agree on.
//! Underneath it they disagree about what the states are called, which calls are
//! legal in each, whether a transition can fail, and — the one that really
//! hurts — whether a failed transition leaves the plugin in a state at all. A
//! trait covering all four could only offer their intersection, and the useful
//! guarantee each format provides lives precisely in what makes it different.
//! The intersection inherits the constraints of all four and the benefits of
//! none.
//!
//! Capabilities do not have that problem, because they differ *additively*. A
//! format either has presets or it does not; a trait a loader does not implement
//! is simply absent, and nothing else changes. A state machine is not local in
//! that way — it constrains when every other method may be called, so a wrong
//! guess about states contaminates the entire surface rather than one corner of
//! it.
//!
//! Then there is a constraint that settles the question regardless of taste. The
//! server erases the format deliberately: `Plugin` is a per-format enum whose
//! whole purpose is that callers above it see only `&mut dyn PluginInstance`,
//! with no downcast back. A type-state design cannot survive that, because
//! consuming transitions need `self` by value and a concrete return type, and
//! `dyn` offers neither. Whatever the formats do internally, the seam between
//! them has to be capability-shaped.
//!
//! So the lifecycle is pushed down, and the traits admit it at the two points
//! where it shows through. [`set_sample_rate`](PluginAudio::set_sample_rate) and
//! [`set_render_mode`](PluginAudio::set_render_mode) are configure-time
//! operations most formats accept only while deactivated, yet both are ordinary
//! `&mut self` methods here — the implementation owns "the deactivate/reactivate
//! bracket their format requires" and the caller never learns it happened. By
//! the time a plugin is reachable through these traits it is loaded *and*
//! activated, so a transition is never something a consumer performs.
//!
//! That turns out to be a good trade rather than merely a necessary one. Having
//! declined to model states centrally, each format crate models its own as
//! tightly as its contract allows — and because none of them has to meet in the
//! middle, each lands somewhere different. VST3 and CLAP both make a large
//! control surface legal before activation, so "loaded but not processing" is a
//! real place to work and earns its own type, with transitions that consume
//! `self` so a stale handle cannot be named. VST2 fuses everything into one
//! type, because its whole init sequence runs in the constructor and a second
//! type would carry no operations the first lacks. AU keeps an internal enum,
//! because its transitions can fail in both directions and a failed one belongs
//! to neither state — something two types cannot express but three variants can.
//!
//! Each crate argues its own case in its own docs; `tutti-plugin`'s crate-level
//! docs compare all four and give the rule for choosing between them.

use crate::{
    AudioBufferMut, EditorSize, LoadedPlugin, Normalized, ParamAddress, ParameterInfo,
    PluginDescriptor, PluginResult as Result, Preset, PresetId, ProcessContext, ProcessOutput,
    RenderMode, WindowHandle,
};

/// A plugin's catalog identity and load-time wiring snapshot.
///
/// Static identity (name, vendor, native class, editor presence) is on
/// [`descriptor`](Self::descriptor); per-bus widths, latency and capability
/// flags are on [`loaded`](Self::loaded). Both are snapshots of what the plugin
/// reported at load time.
pub trait PluginMeta {
    /// Returns the static catalog identity reported at load: name, vendor,
    /// native class and whether an editor exists.
    fn descriptor(&self) -> &PluginDescriptor;

    /// Returns the load-time wiring snapshot: per-bus channel widths, reported
    /// latency and capability flags.
    fn loaded(&self) -> &LoadedPlugin;
}

/// Renders audio blocks through a loaded plugin.
///
/// This is the loader-side render path inside the plugin process; the host
/// side reaches it through `tutti-plugin`'s graph node across the IPC
/// boundary.
pub trait PluginAudio: Send {
    /// Processes one audio block.
    ///
    /// The buffer carries the negotiated sample format (f32 or f64) as a
    /// tagged enum, so the trait stays dyn-compatible while implementations
    /// branch once and delegate into a single generic inner body. Called on
    /// the audio thread; implementations must not allocate or block.
    ///
    /// # `out` is the caller's, and is reused
    ///
    /// The block's non-audio outputs — emitted MIDI, parameter changes, note
    /// expression — are written into `out` rather than returned. **An
    /// implementation must clear it before filling**, since the caller hands
    /// back the same value every block precisely so its heap capacity survives.
    ///
    /// Returning `ProcessOutput` by value looks equivalent and is not: this
    /// runs on the realtime audio thread, and a fresh return value has no
    /// capacity to reuse, so every block that emits more than the inline
    /// `SmallVec` capacity allocates inside the audio callback. A borrowed
    /// return would avoid that, but its lifetime would come from `&mut self`
    /// and hold the plugin borrowed across everything the caller does with the
    /// block.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Process`](crate::PluginError::Process) (or
    /// another variant) when the plugin fails to render the block.
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
        out: &mut ProcessOutput,
    ) -> Result<()>;

    /// Sets the render sample rate in Hz.
    ///
    /// A raw `f64` rather than `Hz`: this is the value handed straight to a C
    /// ABI (`effSetSampleRate`, `IComponent::setupProcessing`), which is where
    /// the unit types stop. Configure-time — most formats accept it only while
    /// the plugin is deactivated.
    fn set_sample_rate(&mut self, rate: f64);

    /// Tells the plugin whether it is rendering under realtime pressure.
    ///
    /// Configure-time, beside [`set_sample_rate`](Self::set_sample_rate), and
    /// for the same reason: three of the four formats can only accept it while
    /// the plugin is deactivated, and a plugin may size buffers from it. See
    /// [`RenderMode`] for why this is not a per-block field.
    ///
    /// Returns whether the plugin *accepted* the mode, so a caller can tell a
    /// refusal from a plugin that was never asked — the same
    /// absent-vs-reported split
    /// [`FeatureReport`](crate::FeatureReport) makes one level up. The default
    /// returns `false`: a host that has not implemented this for a format must
    /// not claim the plugin is honouring it.
    ///
    /// Implementations are responsible for the deactivate/reactivate bracket
    /// their format requires, and should be a no-op when the mode is unchanged
    /// — an offline bounce sets it once, but a caller is entitled to be
    /// idempotent.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        let _ = mode;
        false
    }
}

/// Parameter enumeration, read, and write.
pub trait PluginParams {
    /// Returns a parameter's value, **normalized `0..=1`**, for every format.
    ///
    /// This is the host's authoring convention, and the same one
    /// [`ParameterPoint::value`](crate::ParameterPoint) carries: the direct path
    /// and the automation path speak one vocabulary.
    ///
    /// The formats do not agree, and this trait is where the disagreement stops:
    ///
    /// - **VST2, VST3** are natively normalized (`AEffect::getParameter`;
    ///   VST3's `normalizedParamToPlain` exists precisely because the wire
    ///   value is normalized). Their impls pass the value through.
    /// - **CLAP, AU** take a plain value in the parameter's declared range, so
    ///   their impls convert against the range table each already keeps for
    ///   automation.
    ///
    /// Leaving the domain to the *caller* is unimplementable for a caller that
    /// does not know the format: writing `1.0` to Apple's AUDelay "Lowpass
    /// Cutoff" (`[10, 22050]` Hz) as a plain value sets **1 Hz**, not full scale
    /// — an inaudible filter that reads as "the plugin ignored me". Conversion
    /// is therefore a *loader's* obligation, discharged where the range is
    /// known, exactly as the automation path does it. Callers that hold a
    /// [`ParameterInfo`] and want the plain value ask for it explicitly with
    /// [`ParameterInfo::to_plain`].
    ///
    /// A parameter whose range the plugin never declared
    /// ([`ParamRange::Normalized`](crate::ParamRange::Normalized)) is already
    /// normalized, so the conversion is the identity and no information is
    /// invented.
    ///
    /// `id` is the [`ParamAddress`] from [`ParameterInfo::id`]. For VST3, CLAP
    /// and AU that is an opaque plugin-chosen handle under no obligation to be
    /// dense or ordered, so a loop counter reads whichever parameter happens to
    /// own that numeric slot; only VST2 addresses by position.
    ///
    /// An address whose model does not match the implementing format addresses
    /// no parameter — a VST2 index means nothing to a CLAP plugin. Each impl
    /// reports that as it reports any unknown parameter: `0.0` here, a dropped
    /// write in [`set_parameter`](Self::set_parameter). Neither invents a cast.
    fn get_parameter(&self, id: ParamAddress) -> f64;

    /// Writes a parameter value.
    ///
    /// See [`get_parameter`](Self::get_parameter) for why
    /// every format speaks the normalized convention here, and for how an
    /// address of the wrong model is treated.
    ///
    /// The domain is in the signature rather than only in this sentence:
    /// [`Normalized`] cannot be built from a plain value without passing its
    /// clamp, so the plain-for-normalized mistake described above is not
    /// expressible at this seam. The clamp is silent, so a plain value passed
    /// anyway arrives saturated rather than rejected.
    fn set_parameter(&mut self, id: ParamAddress, value: Normalized);

    /// Returns the plugin's own display string for `value` (`"800 Hz"`,
    /// `"Bandpass"`, `"-inf dB"`), or `None` if it will not say.
    ///
    /// Asking the plugin is the only way to get this: formatting the number
    /// host-side cannot recover it, because only the plugin knows its own taper,
    /// that index `3` is `"Bandpass"`, or that its minimum reads `"-inf"` rather
    /// than `"-120.0"`. Without it a UI reading
    /// [`get_parameter`](Self::get_parameter) holds a bare `0.5` and no way to
    /// learn the plugin would have written `"800 Hz"`.
    ///
    /// `value` is normalized, per [`get_parameter`](Self::get_parameter); the
    /// plain-native loaders (AU, CLAP) denormalize against the same range table
    /// their [`set_parameter`](Self::set_parameter) uses, so the text describes
    /// the value the caller named rather than one a domain mix-up produced.
    ///
    /// `None` means **this plugin did not answer** — not "the value has no
    /// text". A caller renders the raw number instead. The two are worth
    /// keeping apart: `Some("")` would be a plugin claiming the empty string is
    /// the right label, and the default below is `None` so a format that cannot
    /// ask is never mistaken for a plugin that declined.
    ///
    /// Defaulted rather than required so a loader opts in as its format's call
    /// is bound, and an out-of-tree implementor keeps compiling.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        let _ = (id, value);
        None
    }

    /// Parses `text` into a value using the plugin's own interpretation.
    ///
    /// The inverse of [`parameter_text`](Self::parameter_text): it lets a user
    /// type `"800 Hz"` or `"Bandpass"` into a field rather than hunting for the
    /// raw float.
    ///
    /// Asking the plugin rather than running a host-side `str::parse` is the
    /// point: only the plugin knows that `"Bandpass"` is index `3`, or where on
    /// its own range `"-6 dB"` falls.
    ///
    /// Returns [`Normalized`], matching [`parameter_text`](Self::parameter_text),
    /// so the pair round-trips and the result can be handed straight to
    /// [`set_parameter`](Self::set_parameter). AU and CLAP answer in plain units
    /// and re-normalize at their own edge.
    ///
    /// `None` when the plugin cannot parse the string. A caller must then leave
    /// the field where it was rather than substituting a fallback — a
    /// mis-parsed `0.0` would be committed to the user's preset silently.
    ///
    /// **Not guaranteed side-effect-free.** VST 2.4's `effString2Parameter` is a
    /// *setter* with no parse-only counterpart, so on that format asking applies
    /// the value; the other three parse without writing. A caller that wants a
    /// preview before committing cannot get one on every format, and one that
    /// does not intend to write must not call this speculatively.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        let _ = (id, text);
        None
    }

    /// Tells the plugin the host's current [`AutomationMode`](crate::AutomationMode).
    ///
    /// Fire-and-forget; the default no-op covers formats without an
    /// automation-state concept. A format that supports it (VST3's
    /// `IAutomationState`) encodes the mode onto its own ABI.
    fn set_automation_state(&mut self, _mode: crate::AutomationMode) {}

    /// Returns every parameter the plugin advertises, in the plugin's own order.
    ///
    /// The order is presentation, not addressing: index `n` in this vector is
    /// not parameter id `n` for VST3, CLAP or AU. Address through each entry's
    /// [`ParameterInfo::id`].
    fn get_parameter_list(&self) -> Vec<ParameterInfo>;
}

/// Saves and restores a plugin's full state as an opaque chunk.
pub trait PluginState: Send {
    /// Returns the plugin's full state as an opaque chunk, for the host to
    /// persist.
    ///
    /// The bytes are the plugin's own format and carry no host-readable
    /// structure; only the same plugin can interpret them.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::State`](crate::PluginError::State) when the
    /// plugin cannot produce its state.
    fn get_state(&mut self) -> Result<Vec<u8>>;

    /// Restores state from a chunk [`get_state`](Self::get_state) produced.
    ///
    /// `&mut self` because every format applies this to the live instance.
    /// Handing a chunk from a different plugin is the caller's error to avoid —
    /// nothing here can validate the opaque bytes.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::State`](crate::PluginError::State) when the
    /// plugin rejects the chunk.
    fn set_state(&mut self, data: &[u8]) -> Result<()>;
}

/// Enumerates and loads a plugin's own presets.
///
/// Every
/// method is defaulted to "this format cannot", so a loader implements only
/// what its format actually offers — which matters more here than for the other
/// capabilities, because **no format offers both halves unconditionally**:
///
/// - **CLAP** loads by path but cannot enumerate: discovery is a factory-level
///   extension this host does not bind.
/// - **VST3** has no load call; a loader implements the load by writing the
///   parameter flagged `kIsProgramChange` that owns the program's list.
/// - **AU** and **VST2** offer both.
///
/// That split is what [`Features::PRESET_LIST`](crate::Features::PRESET_LIST)
/// and [`Features::PRESET_LOAD`](crate::Features::PRESET_LOAD) report, and why
/// they are two bits rather than one.
pub trait PluginPresets: Send {
    /// Returns every preset the plugin advertises, in the plugin's own order.
    ///
    /// Empty is the default and is the honest answer for a format that cannot
    /// enumerate. It is **not** the same as "this plugin has no presets" — a
    /// caller separates the two by reading `Features::PRESET_LIST`.
    fn get_presets(&mut self) -> Vec<Preset> {
        Vec::new()
    }

    /// Loads a preset by an id [`get_presets`](Self::get_presets) produced.
    ///
    /// Returns whether the plugin accepted. `false` is the default, for a
    /// format that cannot load presets.
    ///
    /// An id whose shape this format does not use addresses nothing — a CLAP
    /// path names no AU preset — and must be refused rather than coerced into
    /// whatever number is nearest. See [`PresetId`].
    fn load_preset(&mut self, _id: &PresetId) -> bool {
        false
    }

    /// Returns the preset the plugin considers current, when it will say.
    ///
    /// `None` means the format has no query or the plugin declined — never
    /// "the first one".
    fn get_current_preset(&mut self) -> Option<PresetId> {
        None
    }
}

/// Opens and closes a loaded plugin's editor inside the plugin process.
///
/// The editor runs on the platform GUI toolkit's own run loop. Idle ticking is
/// not part of this trait: editors are pumped through `tutti-plugin`'s
/// host-side editor surface.
pub trait PluginEditorHost {
    /// Embeds the plugin's editor into `parent`, returning the size it asks for.
    ///
    /// Runs on the platform GUI toolkit's run loop inside the plugin-server
    /// subprocess, never on the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Editor`](crate::PluginError::Editor) when the
    /// plugin has no editor or fails to open it.
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;

    /// Tears the editor down.
    ///
    /// Infallible and idempotent: a caller unwinding from a failed
    /// [`open_editor`](Self::open_editor) has nothing better to do than ask, so
    /// closing an editor that was never opened must be a no-op.
    fn close_editor(&mut self);
}

/// The full capability bundle a loaded plugin instance implements.
///
/// This is a marker supertrait over the fine-grained capabilities, with a
/// blanket impl — a loader implements the small traits and gets
/// `PluginInstance` for free, and a consumer that needs "the whole plugin"
/// depends on this one bound. Consumers that need only
/// one capability should depend on that trait alone.
///
/// [`PluginPresets`] is in the bundle even though every one of its methods is
/// defaulted: a loader that offers no presets writes `impl PluginPresets for X
/// {}` and the defaults report "cannot", which is the honest answer. Leaving it
/// out would mean the dispatch could not reach presets on a loader that *does*
/// offer them without a second bound at every call site.
pub trait PluginInstance:
    PluginMeta + PluginAudio + PluginParams + PluginState + PluginEditorHost + PluginPresets + Send
{
}

impl<T> PluginInstance for T where
    T: PluginMeta
        + PluginAudio
        + PluginParams
        + PluginState
        + PluginEditorHost
        + PluginPresets
        + Send
{
}
