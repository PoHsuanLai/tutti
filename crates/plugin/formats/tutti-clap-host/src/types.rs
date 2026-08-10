//! Common types for CLAP plugin hosting.
//!
//! These are safe, idiomatic Rust counterparts to the C structs exposed by
//! `clap-sys`. They are used throughout the crate's public API so callers
//! never have to touch raw CLAP types directly. Cross-format shared types
//! (buffers, parameter automation, window/editor primitives, transport
//! snapshot, MIDI events) come from `tutti_plugin_types`.

use bitflags::bitflags;
use std::fmt;

pub use tutti_plugin_types::{
    AudioBuffer, AudioBuffer32, AudioBuffer64, ChannelLayout, EditorCapabilities, EditorSize,
    MidiEvent, NoteExpressionType, ParameterChanges, ParameterPoint, ParameterQueue, TransportInfo,
    WindowHandle,
};

/// Metadata describing a loaded plugin, returned from
/// [`ClapLoaded::probe`](crate::ClapLoaded::probe) and
/// [`ClapLoaded::info`](crate::ClapLoaded::info).
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// Reverse-DNS plugin identifier from `clap_plugin_descriptor.id`, stable
    /// across versions and the key a host stores to re-find this plugin.
    pub id: String,
    /// Display name for user-facing lists.
    pub name: String,
    /// Vendor name; empty when the descriptor omits it.
    pub vendor: String,
    /// Vendor-formatted version string, not parsed or ordered by this crate.
    pub version: String,
    /// Plugin homepage; empty when the descriptor omits it.
    pub url: String,
    /// One-line description; empty when the descriptor omits it.
    pub description: String,
    /// CLAP feature/category tags (`"audio-effect"`, `"instrument"`, …) used
    /// for browser categorisation.
    pub features: Vec<String>,
    /// Total input channels summed across every audio input port.
    pub audio_inputs: usize,
    /// Total output channels summed across every audio output port.
    pub audio_outputs: usize,
}

impl PluginInfo {
    /// Create a new [`PluginInfo`] with the given plugin ID and display name.
    /// Defaults to stereo in/out and empty metadata fields.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            vendor: String::new(),
            version: String::new(),
            url: String::new(),
            description: String::new(),
            features: Vec::new(),
            audio_inputs: 2,
            audio_outputs: 2,
        }
    }

    /// Set the plugin vendor (builder style).
    pub fn vendor(mut self, vendor: impl Into<String>) -> Self {
        self.vendor = vendor.into();
        self
    }

    /// Set the plugin version string (builder style).
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Set the plugin homepage URL (builder style).
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Set the plugin description (builder style).
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Set the CLAP feature/category tags (builder style).
    pub fn features(mut self, features: Vec<String>) -> Self {
        self.features = features;
        self
    }

    /// Set the audio input/output channel counts (builder style).
    pub fn audio_io(mut self, inputs: usize, outputs: usize) -> Self {
        self.audio_inputs = inputs;
        self.audio_outputs = outputs;
        self
    }
}

impl fmt::Display for PluginInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} v{} by {}", self.name, self.version, self.vendor)
    }
}

/// A single note expression applied at a sample offset within a block.
///
/// CLAP's voice addressing is richer than the cross-format
/// [`NoteExpressionValue`](tutti_plugin_types::NoteExpressionValue): besides
/// `note_id` it can scope to a `port_index` / `channel` / `key`. This local
/// wrapper carries those extra fields; [`expression_type`](Self::expression_type)
/// uses the shared [`NoteExpressionType`]. Consumers crossing the crate
/// boundary use the shared `NoteExpressionValue`; the loader converts to/from
/// this CLAP-native shape at its edge.
#[derive(Debug, Clone, Copy)]
pub struct ClapNoteExpression {
    /// Frame offset within the current block at which the expression fires.
    pub sample_offset: i32,
    /// The voice this targets, matching the `note_id` of the note-on that
    /// started it. `-1` addresses every voice matching the key/channel scope.
    pub note_id: i32,
    /// Note port the voice was started on.
    pub port_index: i16,
    /// MIDI channel to scope to; `-1` means any channel.
    pub channel: i16,
    /// MIDI key to scope to; `-1` means any key.
    pub key: i16,
    /// Which expression dimension this carries (tuning, brightness, …).
    pub expression_type: NoteExpressionType,
    /// The expression value. Raw `f64` because the valid range is
    /// per-`expression_type` (semitones for tuning, `0..1` for most others) —
    /// this is a C-ABI-shaped value and unit types stop here.
    pub value: f64,
}

/// Cap for a note-expression pool filled on the audio thread.
///
/// Note expressions are per-note-per-block; a plugin emitting more than this
/// in a single block is well outside normal use.
pub const RT_NOTE_EXPR_CAPACITY: usize = 16;

/// A per-block note-expression pool filled inside `process` and lent back out
/// as `&[ClapNoteExpression]`. Capped for the same reason as
/// [`RtMidiEvents`](tutti_plugin_types::RtMidiEvents): the plugin, not the
/// host, decides how many events arrive.
pub type RtNoteExpressions = tutti_types::RtVec<ClapNoteExpression, RT_NOTE_EXPR_CAPACITY>;

impl ClapNoteExpression {
    /// Create a new note expression. Defaults to port 0, any channel, any key;
    /// refine with the `port`/`on_channel`/`on_key`/`at` builders.
    pub fn new(expression_type: NoteExpressionType, note_id: i32, value: f64) -> Self {
        Self {
            sample_offset: 0,
            note_id,
            port_index: 0,
            channel: -1,
            key: -1,
            expression_type,
            value,
        }
    }

    /// Sample offset (within the current block) at which the expression fires.
    pub fn at(mut self, sample_offset: i32) -> Self {
        self.sample_offset = sample_offset;
        self
    }

    /// Set the note port index.
    pub fn port(mut self, port_index: i16) -> Self {
        self.port_index = port_index;
        self
    }

    /// Scope to a specific MIDI channel.
    pub fn on_channel(mut self, channel: i16) -> Self {
        self.channel = channel;
        self
    }

    /// Scope to a specific MIDI key.
    pub fn on_key(mut self, key: i16) -> Self {
        self.key = key;
        self
    }

    /// Build a CLAP-native expression from the shared, format-agnostic
    /// [`NoteExpressionValue`](tutti_plugin_types::NoteExpressionValue). The
    /// shared form carries no voice addressing, so `port`/`channel`/`key`
    /// default to port 0 / any-channel / any-key.
    pub fn from_shared(expr: &tutti_plugin_types::NoteExpressionValue) -> Self {
        Self::new(expr.expression_type, expr.note_id, expr.value).at(expr.sample_offset)
    }
}

bitflags! {
    /// Parameter behaviour flags from `clap_param_info`. See the CLAP spec
    /// for precise semantics of each bit. CLAP-native; the loader projects the
    /// subset it needs onto the shared `tutti_plugin_types::ParameterFlags` at
    /// the crate boundary.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ClapParamFlags: u32 {
        /// Only integral values within the range are valid.
        const STEPPED                 = 1 << 0;
        /// The range wraps: max and min denote the same point.
        const PERIODIC                = 1 << 1;
        /// Not to be shown in a generic parameter list.
        const HIDDEN                  = 1 << 2;
        /// The host must not write this value; the plugin owns it.
        const READONLY                = 1 << 3;
        /// This parameter is the plugin's bypass switch.
        const BYPASS                  = 1 << 4;
        /// The host may automate this parameter.
        const AUTOMATABLE             = 1 << 5;
        /// Automatable with per-voice addressing by `note_id`.
        const AUTOMATABLE_PER_NOTE_ID = 1 << 6;
        /// Automatable with per-key addressing.
        const AUTOMATABLE_PER_KEY     = 1 << 7;
        /// Automatable with per-channel addressing.
        const AUTOMATABLE_PER_CHANNEL = 1 << 8;
        /// Automatable with per-port addressing.
        const AUTOMATABLE_PER_PORT    = 1 << 9;
        /// The host may send modulation offsets for this parameter.
        const MODULATABLE             = 1 << 10;
        /// Modulatable with per-voice addressing by `note_id`.
        const MODULATABLE_PER_NOTE_ID = 1 << 11;
        /// Modulatable with per-key addressing.
        const MODULATABLE_PER_KEY     = 1 << 12;
        /// Modulatable with per-channel addressing.
        const MODULATABLE_PER_CHANNEL = 1 << 13;
        /// Modulatable with per-port addressing.
        const MODULATABLE_PER_PORT    = 1 << 14;
        /// Changes must be delivered in event order through `process()`
        /// rather than out-of-band via `flush()`. Gates
        /// [`set_parameter`](crate::ClapLoaded::set_parameter) on an actively
        /// processing instance.
        const REQUIRES_PROCESS        = 1 << 15;
    }
}

/// Description of a single plugin parameter, CLAP-native. Richer than the
/// shared `tutti_plugin_types::ParameterInfo`: it carries CLAP's full
/// [`ClapParamFlags`] and a `module` grouping path. The loader projects it
/// down to the shared shape at the crate boundary.
#[derive(Debug, Clone)]
pub struct ClapParamInfo {
    /// Stable parameter id. Chosen by the plugin and persisted by the host —
    /// it is the key in automation and state, not the enumeration index.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Slash-separated grouping path (`"Filter/Cutoff"`); empty for ungrouped.
    pub module: String,
    /// Low end of the valid range, in the parameter's own plain units.
    pub min_value: f64,
    /// High end of the valid range, in the parameter's own plain units.
    pub max_value: f64,
    /// Value the plugin starts at, within `min_value..=max_value`.
    pub default_value: f64,
    /// Behaviour bits governing automation, modulation and visibility.
    pub flags: ClapParamFlags,
}

impl ClapParamInfo {
    /// Create a new parameter with the given ID and display name. Defaults
    /// to range `[0.0, 1.0]` with default `0.0` and no flags — use the
    /// builder methods to refine.
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            module: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.0,
            flags: ClapParamFlags::default(),
        }
    }

    /// Set the grouping path (slash-separated, e.g. `"Filter/Cutoff"`).
    pub fn module(mut self, module: impl Into<String>) -> Self {
        self.module = module.into();
        self
    }

    /// Set the value range and default all at once.
    pub fn range(mut self, min: f64, max: f64, default: f64) -> Self {
        self.min_value = min;
        self.max_value = max;
        self.default_value = default;
        self
    }

    /// Set the parameter flags.
    pub fn flags(mut self, flags: ClapParamFlags) -> Self {
        self.flags = flags;
        self
    }
}

/// The scope of a plugin's `params.rescan` request, decoded from
/// `clap_param_rescan_flags`. Returned by
/// [`ClapLoaded::poll_params_rescan`](crate::ClapLoaded::poll_params_rescan)
/// so the host can honour CLAP's rule that a full rescan (`all`) may only be
/// applied while the plugin is deactivated, whereas a value-only rescan can be
/// picked up live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParamRescan {
    /// Any rescan at all was requested since the last poll.
    pub requested: bool,
    /// `CLAP_PARAM_RESCAN_ALL` — everything changed. The plugin MUST be
    /// deactivated before the host re-reads, per the CLAP spec.
    pub all: bool,
    /// `CLAP_PARAM_RESCAN_VALUES` — current values changed; safe to re-read
    /// live.
    pub values: bool,
    /// `CLAP_PARAM_RESCAN_INFO` — info (name/module/flags/range) changed.
    pub info: bool,
    /// `CLAP_PARAM_RESCAN_TEXT` — value→text display changed.
    pub text: bool,
}

impl ParamRescan {
    /// Decode the accumulated `clap_param_rescan_flags` bitset. `requested`
    /// reflects whether any rescan happened (the caller passes that separately
    /// since the flags alone can legally be 0).
    pub(crate) fn from_flags(requested: bool, flags: u32) -> Self {
        use clap_sys::ext::params::{
            CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT,
            CLAP_PARAM_RESCAN_VALUES,
        };
        Self {
            requested,
            all: flags & CLAP_PARAM_RESCAN_ALL != 0,
            values: flags & CLAP_PARAM_RESCAN_VALUES != 0,
            info: flags & CLAP_PARAM_RESCAN_INFO != 0,
            text: flags & CLAP_PARAM_RESCAN_TEXT != 0,
        }
    }

    /// Whether a full re-read requiring plugin deactivation is pending.
    pub fn needs_deactivate(&self) -> bool {
        self.all
    }
}

/// The scope of a plugin's `audio-ports.rescan` request, decoded from
/// `clap_audio_ports_rescan_flags`.
///
/// The sibling of [`ParamRescan`], and for the same reason: five of the six
/// flags are annotated `[!active]` in `ext/audio-ports.h`, meaning the host must
/// deactivate the plugin before re-enumerating. Only `NAMES` is safe to pick up
/// live. Collapsing all six into one bool left a consumer unable to tell a
/// cosmetic port rename from a channel-count change, so `PortLayout` could go
/// stale against the buffer widths the plugin actually expects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioPortsRescan {
    /// Any rescan at all was requested since the last poll.
    pub requested: bool,
    /// `CLAP_AUDIO_PORTS_RESCAN_NAMES` — port names changed. The only flag
    /// applicable while the plugin is active.
    pub names: bool,
    /// `CLAP_AUDIO_PORTS_RESCAN_FLAGS` — per-port flags changed. `[!active]`.
    pub flags: bool,
    /// `CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT` — a port's channel count
    /// changed. `[!active]`, and the one that invalidates buffer sizing.
    pub channel_count: bool,
    /// `CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE` — a port's type changed.
    /// `[!active]`.
    pub port_type: bool,
    /// `CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR` — in-place pairing changed.
    /// `[!active]`.
    pub in_place_pair: bool,
    /// `CLAP_AUDIO_PORTS_RESCAN_LIST` — the set of ports itself changed.
    /// `[!active]`.
    pub list: bool,
}

impl AudioPortsRescan {
    /// Decode the accumulated `clap_audio_ports_rescan_flags` bitset.
    ///
    /// `requested` is passed separately for the same reason as on
    /// [`ParamRescan::from_flags`]: a plugin may legally call `rescan` with no
    /// bits set, and "asked for nothing" must stay distinct from "never asked".
    pub(crate) fn from_flags(requested: bool, flags: u32) -> Self {
        use clap_sys::ext::audio_ports::{
            CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
            CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
            CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
        };
        Self {
            requested,
            names: flags & CLAP_AUDIO_PORTS_RESCAN_NAMES != 0,
            flags: flags & CLAP_AUDIO_PORTS_RESCAN_FLAGS != 0,
            channel_count: flags & CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT != 0,
            port_type: flags & CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE != 0,
            in_place_pair: flags & CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR != 0,
            list: flags & CLAP_AUDIO_PORTS_RESCAN_LIST != 0,
        }
    }

    /// Whether re-enumerating requires deactivating the plugin first.
    ///
    /// True for every flag except `NAMES`. Phrased as "anything but names"
    /// rather than as a list, so a flag added to a later CLAP revision is
    /// treated as unsafe-while-active until someone has read its annotation.
    pub fn needs_deactivate(&self) -> bool {
        self.flags || self.channel_count || self.port_type || self.in_place_pair || self.list
    }
}

/// Description of an audio port exposed by the plugin.
#[derive(Debug, Clone)]
pub struct AudioPortInfo {
    /// Stable port id, distinct from the port's enumeration index.
    pub id: u32,
    /// Display name for the port.
    pub name: String,
    /// The port's channel layout. Carries the count directly; a CLAP port tag
    /// this host does not recognize as mono/stereo becomes
    /// `Multi(channel_count)` — the tag string itself is dropped (nothing
    /// consumes it).
    pub layout: ChannelLayout,
    /// Capability bits for this port.
    pub flags: AudioPortFlags,
    /// Id of the opposite-direction port this one can process in place with,
    /// or CLAP's invalid-id sentinel when in-place is unsupported.
    pub in_place_pair_id: u32,
}

bitflags! {
    /// Audio port capability flags from `clap_audio_port_info`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct AudioPortFlags: u32 {
        /// This is the plugin's main port for its direction. At most one
        /// input and one output port may carry it.
        const MAIN                      = 1 << 0;
        /// The port can process 64-bit buffers as well as 32-bit.
        const SUPPORTS_64BIT            = 1 << 1;
        /// The port would rather be given 64-bit buffers.
        const PREFERS_64BIT             = 1 << 2;
        /// Every port on the plugin must be given the same sample width in a
        /// given `process` call — 32 and 64 bit cannot be mixed.
        const REQUIRES_COMMON_SAMPLE_SIZE = 1 << 3;
    }
}

/// Description of a note (MIDI) port exposed by the plugin.
#[derive(Debug, Clone)]
pub struct NotePortInfo {
    /// Stable port id, distinct from the port's enumeration index.
    pub id: u32,
    /// Display name for the port.
    pub name: String,
    /// Every dialect the port can accept.
    pub supported_dialects: NoteDialects,
    /// The port's preferred encoding, or `None` when the plugin named no
    /// dialect this host recognises.
    ///
    /// `Option` rather than a default variant because there is no dialect a
    /// host can substitute here without making a claim the plugin never made:
    /// `preferred_dialect == 0` means the plugin stated no preference, and a
    /// dialect added to CLAP after this host was built is equally unreadable.
    /// Route through [`dialect_to_send`](Self::dialect_to_send) rather than
    /// reading this field, so the absent case cannot be mistaken for a choice.
    pub preferred_dialect: Option<NoteDialect>,
}

impl NotePortInfo {
    /// The dialect to encode note events in for this port.
    ///
    /// Prefers the plugin's stated choice when this host can speak it, and
    /// otherwise falls back to a dialect the port supports — CLAP first, then
    /// MIDI 1.0. Returns `None` when the port supports neither, which is the
    /// case a caller must not paper over: it can accept only MPE or MIDI 2.0,
    /// and `host_note_ports_supported_dialects` tells the plugin this host
    /// sends neither, so there is no encoding both sides agree on.
    pub fn dialect_to_send(&self) -> Option<NoteDialect> {
        match self.preferred_dialect {
            Some(d @ (NoteDialect::Clap | NoteDialect::Midi)) => Some(d),
            // A preference this host cannot send is no more usable than an
            // absent one, so both take the supported-dialect fallback.
            _ if self.supported_dialects.contains(NoteDialects::CLAP) => Some(NoteDialect::Clap),
            _ if self.supported_dialects.contains(NoteDialects::MIDI) => Some(NoteDialect::Midi),
            _ => None,
        }
    }
}

bitflags! {
    /// Bitset of note-event dialects a port can accept.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct NoteDialects: u32 {
        /// CLAP's own note events, carrying `note_id` voice addressing.
        const CLAP     = 1 << 0;
        /// MIDI 1.0 byte messages.
        const MIDI     = 1 << 1;
        /// MIDI 1.0 under the MPE conventions.
        const MIDI_MPE = 1 << 2;
        /// MIDI 2.0 UMP packets.
        const MIDI2    = 1 << 3;
    }
}

/// A single note-event dialect (the port's preferred encoding).
///
/// This host can encode only [`Clap`](Self::Clap) and [`Midi`](Self::Midi);
/// the other two are recognised when a plugin names them but never sent. See
/// [`NotePortInfo::dialect_to_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteDialect {
    /// CLAP native note events.
    Clap,
    /// MIDI 1.0.
    Midi,
    /// MIDI 1.0 with MPE conventions.
    MidiMpe,
    /// MIDI 2.0 UMP.
    Midi2,
}

/// Voice allocation information reported by instruments that implement
/// `CLAP_EXT_VOICE_INFO`.
#[derive(Debug, Clone, Copy)]
pub struct VoiceInfo {
    /// Voices the plugin currently expects to use, at most `voice_capacity`.
    pub voice_count: u32,
    /// Hard upper bound on simultaneous voices; the host must not allocate
    /// `note_id`s expecting more than this to sound at once.
    pub voice_capacity: u32,
    /// Whether two notes on the same key may overlap. When false, the host
    /// must end the sounding note before starting another on that key.
    pub supports_overlapping_notes: bool,
}

/// A predefined audio-port configuration the plugin can switch to.
#[derive(Debug, Clone)]
pub struct AudioPortConfig {
    /// Stable id, passed back to select this configuration.
    pub id: u32,
    /// Display name, e.g. `"Stereo"` or `"5.1"`.
    pub name: String,
    /// Number of input ports this configuration exposes.
    pub input_port_count: u32,
    /// Number of output ports this configuration exposes.
    pub output_port_count: u32,
    /// Whether a main input port exists in this configuration.
    pub has_main_input: bool,
    /// Channel count of the main input; meaningless unless `has_main_input`.
    pub main_input_channel_count: u32,
    /// Whether a main output port exists in this configuration.
    pub has_main_output: bool,
    /// Channel count of the main output; meaningless unless `has_main_output`.
    pub main_output_channel_count: u32,
}

/// A custom name for a single (port, channel, key) triple — used by
/// drum kits and similar instruments.
#[derive(Debug, Clone)]
pub struct NoteName {
    /// The name to display for this key, e.g. `"Kick"`.
    pub name: String,
    /// Note port the name applies to; `-1` means every port.
    pub port: i16,
    /// MIDI channel the name applies to; `-1` means every channel.
    pub channel: i16,
    /// MIDI key the name applies to; `-1` means every key.
    pub key: i16,
}

/// Which use-case a state save/load is for, from `CLAP_EXT_STATE_CONTEXT`.
///
/// The plugin may serialize differently per context — a preset typically
/// excludes the project-specific bindings a project save keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateContext {
    /// Saving or loading a user preset.
    ForPreset,
    /// Saving or loading as part of the enclosing project.
    ForProject,
    /// Duplicating an existing instance.
    ForDuplicate,
}

impl From<StateContext> for clap_sys::ext::state_context::clap_plugin_state_context_type {
    fn from(ctx: StateContext) -> Self {
        match ctx {
            StateContext::ForPreset => clap_sys::ext::state_context::CLAP_STATE_CONTEXT_FOR_PRESET,
            StateContext::ForProject => {
                clap_sys::ext::state_context::CLAP_STATE_CONTEXT_FOR_PROJECT
            }
            StateContext::ForDuplicate => {
                clap_sys::ext::state_context::CLAP_STATE_CONTEXT_FOR_DUPLICATE
            }
        }
    }
}

/// 32-bit ARGB color used by track info and parameter indication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Opacity; `255` is fully opaque.
    pub alpha: u8,
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Color {
    /// Opaque color (`alpha = 255`).
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            alpha: 255,
            red,
            green,
            blue,
        }
    }

    /// Color with explicit alpha.
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            alpha,
            red,
            green,
            blue,
        }
    }
}

/// The CLAP port-type tag a track's audio width is described by.
///
/// A closed set of the four tags CLAP defines, rather than the `String` this
/// replaced. The stringly-typed version was matched against `"mono"` /
/// `"stereo"` / `"surround"` / `"ambisonic"` on the way out, so any other
/// spelling — a typo, a different case — silently became a null tag that the
/// plugin reads as "no port type at all".
///
/// [`ChannelLayout`] alone cannot carry this: it is a count, deliberately
/// without placement, and `Multi(6)` does not say whether those six channels are
/// 5.1 or third-order-ambisonic-truncated. That distinction is exactly what the
/// tag adds, so the two travel together in [`TrackInfo::audio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackPortType {
    /// `CLAP_PORT_MONO` — one channel.
    Mono,
    /// `CLAP_PORT_STEREO` — two channels, L/R.
    Stereo,
    /// `CLAP_PORT_SURROUND` — channels carry speaker positions.
    Surround,
    /// `CLAP_PORT_AMBISONIC` — channels carry ambisonic components.
    Ambisonic,
}

/// A track's audio width, and the CLAP tag describing how to read it.
///
/// One value rather than the `Option<i32>` count + `Option<String>` tag pair
/// this replaced. Those were two sources of truth about one fact and could
/// disagree — `Some(6)` alongside `Some("stereo")` was representable, with
/// nothing to catch it. Here the count *is* the layout's, so a mismatch cannot
/// be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackAudio {
    /// The track's channel count.
    pub layout: ChannelLayout,
    /// The tag to advertise. `None` sends no port type, which is what a host
    /// that only knows the width should do — CLAP treats an absent tag as
    /// "unspecified" rather than as an error.
    pub port_type: Option<TrackPortType>,
}

impl TrackAudio {
    /// A track whose width is known but whose topology is not: the layout's
    /// natural tag for mono/stereo, and no tag for anything wider (where the
    /// count alone cannot distinguish surround from ambisonic).
    pub fn from_layout(layout: ChannelLayout) -> Self {
        let port_type = match layout.count() {
            1 => Some(TrackPortType::Mono),
            2 => Some(TrackPortType::Stereo),
            _ => None,
        };
        Self { layout, port_type }
    }
}

/// Track metadata the host exposes through `CLAP_EXT_TRACK_INFO`.
#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    /// Track name; `None` leaves the plugin without one.
    pub name: Option<String>,
    /// Track colour for the plugin's GUI; `None` leaves it unset.
    pub color: Option<Color>,
    /// Audio width + port tag. `None` sets neither the channel-count flag nor a
    /// port type, so the plugin learns nothing about the track's audio — which
    /// is the correct signal when the host does not know it.
    pub audio: Option<TrackAudio>,
    /// The track receives from sends rather than carrying its own material.
    pub is_return_track: bool,
    /// The track sums other tracks.
    pub is_bus: bool,
    /// The track is the project's master output.
    pub is_master: bool,
}

/// State of automation recording for a given parameter, used by
/// `CLAP_EXT_PARAM_INDICATION` to drive GUI feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamAutomationState {
    /// No automation exists for the parameter.
    None,
    /// Automation exists but the transport is not applying it.
    Present,
    /// Automation is being played back onto the parameter.
    Playing,
    /// Automation is being written from the parameter.
    Recording,
    /// Automation exists but a live edit is currently overriding it.
    Overriding,
}

/// A page of eight "remote controls" suggested by the plugin, for host
/// surfaces with physical knobs/faders.
#[derive(Debug, Clone)]
pub struct RemoteControlsPage {
    /// Name of the section this page belongs to; empty when ungrouped.
    pub section_name: String,
    /// Stable page id.
    pub page_id: u32,
    /// Display name for the page.
    pub page_name: String,
    /// The eight parameter ids to bind, in knob order, copied verbatim from
    /// the plugin. Unused slots carry CLAP's invalid-id sentinel rather than
    /// being absent, so the array's indices stay the knob positions.
    pub param_ids: [u32; 8],
    /// The page is meant for preset browsing rather than live control.
    pub is_for_preset: bool,
}

/// A transport-state request a plugin has issued via `CLAP_EXT_TRANSPORT_CONTROL`.
///
/// Drain these with `ClapLoaded::drain_transport_requests` (itself gated
/// behind `clap-extras`) and translate them to your host's transport model.
/// A request is advisory:
/// the host decides whether to honour it.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportRequest {
    /// Begin playback from the current position.
    Start,
    /// Stop playback and return to the start position.
    Stop,
    /// Resume playback from where it was paused.
    Continue,
    /// Halt playback, keeping the current position.
    Pause,
    /// Start if stopped, stop if playing.
    TogglePlay,
    /// Relocate the playhead.
    Jump {
        /// Target position, in beats from the timeline origin.
        position_beats: f64,
    },
    /// Redefine the loop bounds.
    LoopRegion {
        /// Loop start, in beats from the timeline origin.
        start_beats: f64,
        /// Loop length in beats.
        duration_beats: f64,
    },
    /// Invert whether looping is enabled.
    ToggleLoop,
    /// Set looping on or off explicitly.
    EnableLoop(bool),
    /// Set recording on or off explicitly.
    Record(bool),
    /// Invert whether recording is armed.
    ToggleRecord,
}

/// What a context menu action applies to: the plugin as a whole, or a
/// specific parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuTarget {
    /// The plugin instance itself.
    Global,
    /// One parameter, by its stable id.
    Param(u32),
}

/// A single item in a plugin-supplied context menu.
///
/// Submenus are expressed as a flat sequence bracketed by
/// [`BeginSubmenu`](Self::BeginSubmenu) / [`EndSubmenu`](Self::EndSubmenu)
/// rather than by nesting, matching CLAP's builder callback order.
#[derive(Debug, Clone)]
pub enum ContextMenuItem {
    /// A clickable entry.
    Entry {
        /// Text to display.
        label: String,
        /// Whether the entry can be chosen.
        is_enabled: bool,
        /// Id to pass back when the entry is chosen.
        action_id: u32,
    },
    /// A clickable entry carrying a checkbox.
    CheckEntry {
        /// Text to display.
        label: String,
        /// Whether the entry can be chosen.
        is_enabled: bool,
        /// Current state of the checkbox.
        is_checked: bool,
        /// Id to pass back when the entry is chosen.
        action_id: u32,
    },
    /// A horizontal divider.
    Separator,
    /// A non-clickable heading.
    Title {
        /// Text to display.
        title: String,
        /// Whether the heading renders as active.
        is_enabled: bool,
    },
    /// Opens a submenu; items until the matching `EndSubmenu` belong to it.
    BeginSubmenu {
        /// Text to display for the submenu.
        label: String,
        /// Whether the submenu can be opened.
        is_enabled: bool,
    },
    /// Closes the most recently opened submenu.
    EndSubmenu,
}

/// A request to reconfigure a single audio port's channel count/type.
#[derive(Debug, Clone)]
pub struct AudioPortConfigRequest {
    /// Whether the port is an input; `false` selects an output port.
    pub is_input: bool,
    /// The port's enumeration index within its direction.
    pub port_index: u32,
    /// Requested channel count.
    pub channel_count: u32,
    /// Requested CLAP port-type tag; `None` requests no particular type.
    /// A `String` because this crosses to C as a raw tag.
    pub port_type: Option<String>,
}

/// Ambisonic channel ordering (Furse-Malham or Ambisonic Channel Number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbisonicOrdering {
    /// Furse-Malham ordering.
    Fuma,
    /// Ambisonic Channel Number ordering.
    Acn,
}

/// Ambisonic normalization scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbisonicNormalization {
    /// maxN.
    MaxN,
    /// SN3D, the AmbiX convention.
    Sn3d,
    /// N3D, full three-dimensional normalization.
    N3d,
    /// SN2D, the two-dimensional counterpart of SN3D.
    Sn2d,
    /// N2D, the two-dimensional counterpart of N3D.
    N2d,
}

/// Combined ambisonic ordering + normalization.
///
/// Both halves are needed to read a channel: the ordering says which component
/// a channel carries, the normalization says how it is scaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbisonicConfig {
    /// Which component each channel carries.
    pub ordering: AmbisonicOrdering,
    /// How component amplitudes are scaled.
    pub normalization: AmbisonicNormalization,
}

/// Surround speaker positions (matches CLAP's `CLAP_SURROUND_*` constants).
///
/// # Why an `Unknown` arm, and why no `#[repr(u8)]`
///
/// The channel map is **positional**: `map[i]` is the speaker fed by channel
/// `i`. So a position this crate cannot name must still occupy its slot —
/// dropping it renumbers every channel after it, turning one unnameable
/// speaker into a silently wrong routing for the whole tail of the bus. That is
/// what [`Unknown`](Self::Unknown) is for: it carries the raw value verbatim so
/// the map keeps its length and its indices.
///
/// The realistic source of one is a CLAP revision adding a position past
/// `CLAP_SURROUND_TSR` — the same open-catalog reason `AuLayoutTag` carries an
/// `Unknown` arm. A closed enum would turn that release into either a decode
/// failure or a shifted map.
///
/// The `#[repr(u8)]` went with it: a payload variant cannot carry explicit
/// discriminants. The wire values live in [`from_position`](Self::from_position)
/// and [`position`](Self::position), which is where a reader checks them
/// against the header anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurroundChannel {
    /// `CLAP_SURROUND_FL` (0).
    FrontLeft,
    /// `CLAP_SURROUND_FR` (1).
    FrontRight,
    /// `CLAP_SURROUND_FC` (2).
    FrontCenter,
    /// `CLAP_SURROUND_LFE` (3).
    LowFrequency,
    /// `CLAP_SURROUND_BL` (4).
    BackLeft,
    /// `CLAP_SURROUND_BR` (5).
    BackRight,
    /// `CLAP_SURROUND_FLC` (6).
    FrontLeftCenter,
    /// `CLAP_SURROUND_FRC` (7).
    FrontRightCenter,
    /// `CLAP_SURROUND_BC` (8).
    BackCenter,
    /// `CLAP_SURROUND_SL` (9).
    SideLeft,
    /// `CLAP_SURROUND_SR` (10).
    SideRight,
    /// `CLAP_SURROUND_TC` (11).
    TopCenter,
    /// `CLAP_SURROUND_TFL` (12).
    TopFrontLeft,
    /// `CLAP_SURROUND_TFC` (13).
    TopFrontCenter,
    /// `CLAP_SURROUND_TFR` (14).
    TopFrontRight,
    /// `CLAP_SURROUND_TBL` (15).
    TopBackLeft,
    /// `CLAP_SURROUND_TBC` (16).
    TopBackCenter,
    /// `CLAP_SURROUND_TBR` (17).
    TopBackRight,
    /// `CLAP_SURROUND_TSL` (18).
    TopSideLeft,
    /// `CLAP_SURROUND_TSR` (19) — the last position CLAP defines.
    TopSideRight,
    /// A position this crate does not name, carried verbatim.
    ///
    /// Keeps the channel's slot in a positional map (see the type docs) and
    /// lets the raw value be echoed back or logged.
    Unknown(u8),
}

impl SurroundChannel {
    /// Map a raw CLAP surround channel ID to a [`SurroundChannel`].
    ///
    /// Total: an unrecognised id becomes [`Unknown`](Self::Unknown) rather than
    /// `None`, because the caller decodes a *positional* map and has no way to
    /// represent "channel 4's speaker is unreadable" other than by keeping the
    /// slot.
    pub fn from_position(pos: u8) -> Self {
        match pos {
            0 => Self::FrontLeft,
            1 => Self::FrontRight,
            2 => Self::FrontCenter,
            3 => Self::LowFrequency,
            4 => Self::BackLeft,
            5 => Self::BackRight,
            6 => Self::FrontLeftCenter,
            7 => Self::FrontRightCenter,
            8 => Self::BackCenter,
            9 => Self::SideLeft,
            10 => Self::SideRight,
            11 => Self::TopCenter,
            12 => Self::TopFrontLeft,
            13 => Self::TopFrontCenter,
            14 => Self::TopFrontRight,
            15 => Self::TopBackLeft,
            16 => Self::TopBackCenter,
            17 => Self::TopBackRight,
            18 => Self::TopSideLeft,
            19 => Self::TopSideRight,
            other => Self::Unknown(other),
        }
    }

    /// The raw CLAP position id, the inverse of
    /// [`from_position`](Self::from_position).
    ///
    /// Exists because the enum no longer carries `#[repr(u8)]` (see the type
    /// docs), so `as u8` is not available — and because a round trip is what
    /// pins the two tables against each other.
    pub fn position(self) -> u8 {
        match self {
            Self::FrontLeft => 0,
            Self::FrontRight => 1,
            Self::FrontCenter => 2,
            Self::LowFrequency => 3,
            Self::BackLeft => 4,
            Self::BackRight => 5,
            Self::FrontLeftCenter => 6,
            Self::FrontRightCenter => 7,
            Self::BackCenter => 8,
            Self::SideLeft => 9,
            Self::SideRight => 10,
            Self::TopCenter => 11,
            Self::TopFrontLeft => 12,
            Self::TopFrontCenter => 13,
            Self::TopFrontRight => 14,
            Self::TopBackLeft => 15,
            Self::TopBackCenter => 16,
            Self::TopBackRight => 17,
            Self::TopSideLeft => 18,
            Self::TopSideRight => 19,
            Self::Unknown(raw) => raw,
        }
    }
}

/// Event flags of a POSIX file descriptor registered by the plugin
/// (`CLAP_EXT_POSIX_FD_SUPPORT`). Only available on Unix targets.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixFdFlags {
    /// The descriptor is readable without blocking.
    pub read: bool,
    /// The descriptor is writable without blocking.
    pub write: bool,
    /// The descriptor is in an error state.
    pub error: bool,
}

/// Description of a trigger parameter from `CLAP_EXT_TRIGGERS` (a stateless
/// momentary action, like "reset oscillators").
#[derive(Debug, Clone)]
pub struct TriggerInfo {
    /// Stable trigger id, passed back to fire it.
    pub id: u32,
    /// Raw `clap_trigger_info` flag bits, carried undecoded — this host has no
    /// trigger flags it acts on.
    pub flags: u32,
    /// Display name.
    pub name: String,
    /// Slash-separated grouping path; empty for ungrouped.
    pub module: String,
}

/// Description of a dynamic tuning table the plugin can use via
/// `CLAP_EXT_TUNING`.
#[derive(Debug, Clone)]
pub struct TuningInfo {
    /// Stable id the host uses to refer to this tuning.
    pub tuning_id: u32,
    /// Display name of the tuning.
    pub name: String,
    /// Whether the table may change while in use, so the plugin must re-read
    /// it rather than caching.
    pub is_dynamic: bool,
}

/// Undo delta-format capabilities reported by the plugin.
#[derive(Debug, Clone, Copy)]
pub struct UndoDeltaProperties {
    /// The plugin can produce undo deltas at all.
    pub has_delta: bool,
    /// Deltas stay valid across save/load, so the host may persist them. When
    /// false they are only usable within the current session.
    pub are_deltas_persistent: bool,
    /// Version of the plugin's delta encoding. A delta must only be replayed
    /// into a plugin reporting the same version.
    pub format_version: u32,
}

/// An undo step recorded by the plugin: its display name plus an opaque
/// delta blob whose meaning is private to the plugin.
#[derive(Debug, Clone)]
pub struct UndoChange {
    /// Display name for the step, e.g. `"Set Cutoff"`.
    pub name: String,
    /// The plugin's own encoding of the change. Opaque to the host, which may
    /// only store it and hand it back.
    pub delta: Vec<u8>,
    /// Whether this delta can be applied in the undo direction. A plugin may
    /// record a step it can only redo.
    pub delta_can_undo: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::ext::audio_ports::{
        CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
        CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
        CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
    };
    use clap_sys::ext::params::{
        CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT,
        CLAP_PARAM_RESCAN_VALUES,
    };

    #[test]
    fn param_rescan_decodes_all_flag() {
        let r = ParamRescan::from_flags(true, CLAP_PARAM_RESCAN_ALL);
        assert!(r.requested);
        assert!(r.all);
        assert!(r.needs_deactivate(), "RESCAN_ALL requires deactivate");
        assert!(!r.values);
    }

    #[test]
    fn param_rescan_decodes_values_only() {
        // A value-only rescan must NOT require deactivate — it can be picked up
        // live, which is the whole point of distinguishing it from RESCAN_ALL.
        let r = ParamRescan::from_flags(true, CLAP_PARAM_RESCAN_VALUES);
        assert!(r.requested);
        assert!(r.values);
        assert!(!r.all);
        assert!(!r.needs_deactivate());
    }

    #[test]
    fn param_rescan_decodes_combined_flags() {
        let flags = CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT;
        let r = ParamRescan::from_flags(true, flags);
        assert!(r.values && r.info && r.text);
        assert!(!r.all);
    }

    #[test]
    fn param_rescan_empty_when_not_requested() {
        let r = ParamRescan::from_flags(false, 0);
        assert_eq!(r, ParamRescan::default());
        assert!(!r.requested);
    }

    /// A name change is the one rescan applicable while the plugin is active.
    ///
    /// This is the distinction the old `changed: bool` could not carry. A host
    /// that deactivates on every port rename stalls audio for a cosmetic
    /// update; one that treats every rescan as cosmetic re-reads a channel
    /// count while active, which the spec forbids.
    #[test]
    fn audio_ports_rescan_names_is_safe_while_active() {
        let r = AudioPortsRescan::from_flags(true, CLAP_AUDIO_PORTS_RESCAN_NAMES);
        assert!(r.requested);
        assert!(r.names);
        assert!(
            !r.needs_deactivate(),
            "NAMES is the only flag not annotated [!active]"
        );
    }

    /// Each of the five `[!active]` flags requires deactivation on its own.
    ///
    /// Individually rather than combined: a decoder that only checked, say,
    /// `list` would pass a combined-flags test while silently letting a
    /// channel-count change through live.
    #[test]
    fn every_non_name_rescan_flag_requires_deactivation() {
        for (flag, label) in [
            (CLAP_AUDIO_PORTS_RESCAN_FLAGS, "FLAGS"),
            (CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, "CHANNEL_COUNT"),
            (CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE, "PORT_TYPE"),
            (CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, "IN_PLACE_PAIR"),
            (CLAP_AUDIO_PORTS_RESCAN_LIST, "LIST"),
        ] {
            let r = AudioPortsRescan::from_flags(true, flag);
            assert!(
                r.needs_deactivate(),
                "{label} is annotated [!active] and must require deactivation"
            );
        }
    }

    /// A channel-count change alongside a name change still needs deactivation.
    ///
    /// The mixed case is the one a host gets wrong by checking the wrong bit
    /// first: the safe flag being present must not license applying the batch
    /// live.
    #[test]
    fn a_name_change_does_not_excuse_a_channel_count_change() {
        let flags = CLAP_AUDIO_PORTS_RESCAN_NAMES | CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT;
        let r = AudioPortsRescan::from_flags(true, flags);
        assert!(r.names && r.channel_count);
        assert!(r.needs_deactivate());
    }

    /// A rescan carrying no bits is still a rescan, and no bits is not one.
    ///
    /// `requested` is tracked separately because a plugin may legally call
    /// `rescan(0)`; folding it into the flags would make "asked for nothing"
    /// indistinguishable from "never asked".
    #[test]
    fn audio_ports_rescan_tracks_requested_separately_from_flags() {
        let asked_nothing = AudioPortsRescan::from_flags(true, 0);
        assert!(asked_nothing.requested);
        assert!(!asked_nothing.needs_deactivate());

        let never_asked = AudioPortsRescan::from_flags(false, 0);
        assert_eq!(never_asked, AudioPortsRescan::default());
        assert!(!never_asked.requested);
    }
}
