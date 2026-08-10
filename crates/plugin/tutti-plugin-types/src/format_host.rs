//! The fine-grained plugin-instance capability traits every format loader
//! implements, and the [`PluginInstance`] bundle that composes them.
//!
//! A loaded plugin (VST2 / VST3 / CLAP / AU) is a *subprocess-local* object:
//! it lives entirely inside `tutti-plugin-server`, reached through
//! `Plugin::instance_mut()`, and never crosses the IPC wire. The surface is
//! split along the real capability axis, one trait per concern, so a loader
//! implements only the capabilities it has and a consumer depends only on the
//! capability it uses.
//!
//! Note the audio methods here ([`PluginAudio`]) are the *subprocess* render
//! path — distinct from the host-side `AudioUnit` node (`PluginClient`) on the
//! other end of the wire. Both are "process a block", but they are different
//! objects in different processes, so this is not a duplicate of `AudioUnit`.

use crate::{
    AudioBufferMut, EditorSize, LoadedPlugin, Normalized, ParamAddress, ParameterInfo,
    PluginDescriptor, Preset, PresetId, ProcessContext, ProcessOutput, RenderMode, Result,
    WindowHandle,
};

/// Catalog identity + load-time engine-wiring snapshot.
///
/// Static identity (name, vendor, native class, has-editor) is on
/// [`descriptor`](Self::descriptor); per-bus widths / latency / f64 support on
/// [`loaded`](Self::loaded). Both are snapshots of what the plugin reported at
/// load time — pure `&self` queries with no live-plugin analogue on a fundsp
/// node (`AudioUnit::get_id()` is a shared *type* tag, not per-instance
/// identity).
pub trait PluginMeta {
    /// Static catalog identity as reported at load: name, vendor, native class,
    /// whether an editor exists.
    fn descriptor(&self) -> &PluginDescriptor;

    /// The load-time engine-wiring snapshot: per-bus channel widths, reported
    /// latency, f64 support.
    fn loaded(&self) -> &LoadedPlugin;
}

/// The subprocess audio render path.
///
/// The loader-side counterpart of the host-side `AudioUnit` node: same "render
/// one block" job, different object across the IPC boundary.
pub trait PluginAudio: Send {
    /// Process one audio block. The buffer carries the negotiated sample
    /// format (f32 or f64) as a tagged enum, so the trait stays
    /// dyn-compatible while implementations branch once and delegate into a
    /// single generic inner body.
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> Result<ProcessOutput>;

    /// Sets the render sample rate in Hz.
    ///
    /// A raw `f64` rather than `Hz`: this is the value handed straight to a C
    /// ABI (`effSetSampleRate`, `IComponent::setupProcessing`), which is where
    /// the unit types stop. Configure-time — most formats accept it only while
    /// the plugin is deactivated.
    fn set_sample_rate(&mut self, rate: f64);

    /// Tell the plugin whether it is rendering under realtime pressure.
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
///
/// A fundsp node has no parameter *catalog* ([`get_parameter_list`](Self::get_parameter_list)
/// returns id/name/range/default/unit/flags with no node analogue), so this
/// stays plugin-specific.
pub trait PluginParams {
    /// Parameter value, **normalized `0..=1`**, for every format.
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

    /// Write a parameter — see [`get_parameter`](Self::get_parameter) for why
    /// every format speaks the normalized convention here, and for how an
    /// address of the wrong model is treated.
    ///
    /// The domain is in the signature rather than only in this sentence:
    /// [`Normalized`] cannot be built from a plain value without passing its
    /// clamp, so the plain-for-normalized mistake described above is not
    /// expressible at this seam. The clamp is silent, so a plain value passed
    /// anyway arrives saturated rather than rejected.
    fn set_parameter(&mut self, id: ParamAddress, value: Normalized);

    /// The plugin's own display string for `value` — `"800 Hz"`, `"Bandpass"`,
    /// `"-inf dB"` — or `None` if it will not say.
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

    /// Parse `text` back to a value using the plugin's own interpretation — the
    /// inverse of [`parameter_text`](Self::parameter_text), and what lets a user
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

    /// Push the host [`AutomationMode`](crate::AutomationMode) to the plugin.
    /// Fire-and-forget; the default no-op covers formats without an
    /// automation-state concept. A format that supports it (VST3's
    /// `IAutomationState`) encodes the mode onto its own ABI at the FFI edge.
    fn set_automation_state(&mut self, _mode: crate::AutomationMode) {}

    /// Every parameter the plugin advertises, in the plugin's own order.
    ///
    /// The order is presentation, not addressing: index `n` in this vector is
    /// not parameter id `n` for VST3, CLAP or AU. Address through each entry's
    /// [`ParameterInfo::id`].
    fn get_parameter_list(&self) -> Vec<ParameterInfo>;
}

/// Opaque preset-chunk save/load. No fundsp node has serializable opaque state,
/// so this is genuinely irreducible.
pub trait PluginState: Send {
    /// The plugin's full state as an opaque chunk, for the host to persist.
    ///
    /// The bytes are the plugin's own format and carry no host-readable
    /// structure; only the same plugin can interpret them.
    fn get_state(&mut self) -> Result<Vec<u8>>;

    /// Restores state from a chunk [`get_state`](Self::get_state) produced.
    ///
    /// `&mut self` because every format applies this to the live instance.
    /// Handing a chunk from a different plugin is the caller's error to avoid —
    /// nothing here can validate the opaque bytes.
    fn set_state(&mut self, data: &[u8]) -> Result<()>;
}

/// Preset enumeration and loading, subprocess side.
///
/// The mirror of the host-side `HostPresets`, on the far end of the IPC. Every
/// method is defaulted to "this format cannot", so a loader implements only
/// what its format actually offers — which matters more here than for the other
/// capabilities, because **no format offers both halves unconditionally**:
///
/// - **CLAP** loads by path but cannot enumerate: discovery is a factory-level
///   extension this host does not bind.
/// - **VST3** enumerates but has no load call — a program is selected by writing
///   the parameter flagged `kIsProgramChange`, through the parameter path.
/// - **AU** and **VST2** offer both.
///
/// That split is what [`Features::PRESET_LIST`](crate::Features::PRESET_LIST)
/// and [`Features::PRESET_LOAD`](crate::Features::PRESET_LOAD) report, and why
/// they are two bits rather than one.
pub trait PluginPresets: Send {
    /// Every preset the plugin advertises, in the plugin's own order.
    ///
    /// Empty is the default and is the honest answer for a format that cannot
    /// enumerate. It is **not** the same as "this plugin has no presets" — a
    /// caller separates the two by reading `Features::PRESET_LIST`.
    fn get_presets(&mut self) -> Vec<Preset> {
        Vec::new()
    }

    /// Load one, by an id [`get_presets`](Self::get_presets) produced.
    ///
    /// Returns whether the plugin accepted. `false` is the default, and is the
    /// honest answer for VST3: its programs go through the parameter path, and
    /// routing them here as well would give one operation two write paths.
    ///
    /// An id whose shape this format does not use addresses nothing — a CLAP
    /// path names no AU preset — and must be refused rather than coerced into
    /// whatever number is nearest. See [`PresetId`].
    fn load_preset(&mut self, _id: &PresetId) -> bool {
        false
    }

    /// Which preset the plugin considers current, when it will say. `None`
    /// means the format has no query or the plugin declined — never "the first
    /// one".
    fn get_current_preset(&mut self) -> Option<PresetId> {
        None
    }
}

/// The subprocess-side editor hooks.
///
/// Distinct from the host-side `PluginEditor` (the second, editor-only dlopen
/// in the main process): this is the editor surface a loader exposes *from
/// inside* the plugin-server subprocess, where the editor runs on the platform
/// GUI toolkit's own run loop.
///
/// **Idle ticking is not here.** Every editor this codebase pumps — including
/// the in-process VST2 one — is pumped through the host-side surface
/// (`HostEditor::editor_idle`, driven per frame by
/// `bevy_tutti::plugin_host::editor`). This trait deliberately has no
/// `editor_idle`: a second pump path beside the working one would give one thing
/// two writers.
pub trait PluginEditorHost {
    /// Embeds the plugin's editor into `parent`, returning the size it asks for.
    ///
    /// Runs on the platform GUI toolkit's run loop inside the plugin-server
    /// subprocess, never on the audio thread.
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;

    /// Tears the editor down.
    ///
    /// Infallible and idempotent: a caller unwinding from a failed
    /// [`open_editor`](Self::open_editor) has nothing better to do than ask, so
    /// closing an editor that was never opened must be a no-op.
    fn close_editor(&mut self);
}

/// A loaded plugin instance: the full capability bundle a format loader
/// implements.
///
/// This is a marker supertrait over the fine-grained capabilities, with a
/// blanket impl — a loader implements the small traits and gets
/// `PluginInstance` for free, and a consumer that needs "the whole plugin"
/// (the session dispatch) depends on this one bound. Consumers that need only
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
