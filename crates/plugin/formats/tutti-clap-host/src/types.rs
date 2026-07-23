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
    AudioBuffer, AudioBuffer32, AudioBuffer64, EditorCapabilities, EditorSize, MidiEvent,
    NoteExpressionType, ParameterChanges, ParameterPoint, ParameterQueue, TransportInfo,
    WindowHandle,
};

/// Metadata describing a loaded plugin, returned from
/// [`ClapInstance::probe`](crate::ClapInstance::probe) and
/// [`ClapInstance::info`](crate::ClapInstance::info).
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub url: String,
    pub description: String,
    pub features: Vec<String>,
    pub audio_inputs: usize,
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
/// uses the shared [`NoteExpressionType`].
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionValue {
    pub sample_offset: i32,
    pub note_id: i32,
    pub port_index: i16,
    pub channel: i16,
    pub key: i16,
    pub expression_type: NoteExpressionType,
    pub value: f64,
}

impl NoteExpressionValue {
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
}

bitflags! {
    /// Parameter behaviour flags from `clap_param_info`. See the CLAP spec
    /// for precise semantics of each bit.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ParameterFlags: u32 {
        const STEPPED                 = 1 << 0;
        const PERIODIC                = 1 << 1;
        const HIDDEN                  = 1 << 2;
        const READONLY                = 1 << 3;
        const BYPASS                  = 1 << 4;
        const AUTOMATABLE             = 1 << 5;
        const AUTOMATABLE_PER_NOTE_ID = 1 << 6;
        const AUTOMATABLE_PER_KEY     = 1 << 7;
        const AUTOMATABLE_PER_CHANNEL = 1 << 8;
        const AUTOMATABLE_PER_PORT    = 1 << 9;
        const MODULATABLE             = 1 << 10;
        const MODULATABLE_PER_NOTE_ID = 1 << 11;
        const MODULATABLE_PER_KEY     = 1 << 12;
        const MODULATABLE_PER_CHANNEL = 1 << 13;
        const MODULATABLE_PER_PORT    = 1 << 14;
        const REQUIRES_PROCESS        = 1 << 15;
    }
}

/// Description of a single plugin parameter.
#[derive(Debug, Clone)]
pub struct ParameterInfo {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    pub flags: ParameterFlags,
}

impl ParameterInfo {
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
            flags: ParameterFlags::default(),
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
    pub fn flags(mut self, flags: ParameterFlags) -> Self {
        self.flags = flags;
        self
    }
}

/// Description of an audio port exposed by the plugin.
#[derive(Debug, Clone)]
pub struct AudioPortInfo {
    pub id: u32,
    pub name: String,
    pub channel_count: u32,
    pub flags: AudioPortFlags,
    pub port_type: AudioPortType,
    pub in_place_pair_id: u32,
}

bitflags! {
    /// Audio port capability flags from `clap_audio_port_info`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct AudioPortFlags: u32 {
        const MAIN                      = 1 << 0;
        const SUPPORTS_64BIT            = 1 << 1;
        const PREFERS_64BIT             = 1 << 2;
        const REQUIRES_COMMON_SAMPLE_SIZE = 1 << 3;
    }
}

/// Standard or custom audio port channel layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPortType {
    Mono,
    Stereo,
    /// Non-standard layout identified by its CLAP string tag.
    Custom(String),
}

/// Description of a note (MIDI) port exposed by the plugin.
#[derive(Debug, Clone)]
pub struct NotePortInfo {
    pub id: u32,
    pub name: String,
    pub supported_dialects: NoteDialects,
    pub preferred_dialect: NoteDialect,
}

bitflags! {
    /// Bitset of note-event dialects a port can accept.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct NoteDialects: u32 {
        const CLAP     = 1 << 0;
        const MIDI     = 1 << 1;
        const MIDI_MPE = 1 << 2;
        const MIDI2    = 1 << 3;
    }
}

/// A single note-event dialect (the port's preferred encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteDialect {
    Clap,
    Midi,
    MidiMpe,
    Midi2,
}

/// Voice allocation information reported by instruments that implement
/// `CLAP_EXT_VOICE_INFO`.
#[derive(Debug, Clone, Copy)]
pub struct VoiceInfo {
    pub voice_count: u32,
    pub voice_capacity: u32,
    pub supports_overlapping_notes: bool,
}

/// A predefined audio-port configuration the plugin can switch to.
#[derive(Debug, Clone)]
pub struct AudioPortConfig {
    pub id: u32,
    pub name: String,
    pub input_port_count: u32,
    pub output_port_count: u32,
    pub has_main_input: bool,
    pub main_input_channel_count: u32,
    pub has_main_output: bool,
    pub main_output_channel_count: u32,
}

/// A custom name for a single (port, channel, key) triple — used by
/// drum kits and similar instruments.
#[derive(Debug, Clone)]
pub struct NoteName {
    pub name: String,
    pub port: i16,
    pub channel: i16,
    pub key: i16,
}

/// Which use-case a state save/load is for, from `CLAP_EXT_STATE_CONTEXT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateContext {
    ForPreset,
    ForProject,
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
    pub alpha: u8,
    pub red: u8,
    pub green: u8,
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

/// Track metadata the host exposes through `CLAP_EXT_TRACK_INFO`.
#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    pub name: Option<String>,
    pub color: Option<Color>,
    pub audio_channel_count: Option<i32>,
    pub audio_port_type: Option<String>,
    pub is_return_track: bool,
    pub is_bus: bool,
    pub is_master: bool,
}

/// State of automation recording for a given parameter, used by
/// `CLAP_EXT_PARAM_INDICATION` to drive GUI feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamAutomationState {
    None,
    Present,
    Playing,
    Recording,
    Overriding,
}

/// A page of eight "remote controls" suggested by the plugin, for host
/// surfaces with physical knobs/faders.
#[derive(Debug, Clone)]
pub struct RemoteControlsPage {
    pub section_name: String,
    pub page_id: u32,
    pub page_name: String,
    pub param_ids: [u32; 8],
    pub is_for_preset: bool,
}

/// A transport-state request a plugin has issued via `CLAP_EXT_TRANSPORT_CONTROL`.
///
/// Drain these with [`ClapInstance::drain_transport_requests`](crate::ClapInstance::drain_transport_requests)
/// and translate them to your host's transport model.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportRequest {
    Start,
    Stop,
    Continue,
    Pause,
    TogglePlay,
    Jump {
        position_beats: f64,
    },
    LoopRegion {
        start_beats: f64,
        duration_beats: f64,
    },
    ToggleLoop,
    EnableLoop(bool),
    Record(bool),
    ToggleRecord,
}

/// What a context menu action applies to: the plugin as a whole, or a
/// specific parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuTarget {
    Global,
    Param(u32),
}

/// A single item in a plugin-supplied context menu.
#[derive(Debug, Clone)]
pub enum ContextMenuItem {
    Entry {
        label: String,
        is_enabled: bool,
        action_id: u32,
    },
    CheckEntry {
        label: String,
        is_enabled: bool,
        is_checked: bool,
        action_id: u32,
    },
    Separator,
    Title {
        title: String,
        is_enabled: bool,
    },
    BeginSubmenu {
        label: String,
        is_enabled: bool,
    },
    EndSubmenu,
}

/// A request to reconfigure a single audio port's channel count/type.
#[derive(Debug, Clone)]
pub struct AudioPortConfigRequest {
    pub is_input: bool,
    pub port_index: u32,
    pub channel_count: u32,
    pub port_type: Option<String>,
}

/// Ambisonic channel ordering (Furse-Malham or Ambisonic Channel Number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbisonicOrdering {
    Fuma,
    Acn,
}

/// Ambisonic normalization scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbisonicNormalization {
    MaxN,
    Sn3d,
    N3d,
    Sn2d,
    N2d,
}

/// Combined ambisonic ordering + normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbisonicConfig {
    pub ordering: AmbisonicOrdering,
    pub normalization: AmbisonicNormalization,
}

/// Surround speaker positions (matches CLAP's `CLAP_SURROUND_*` constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SurroundChannel {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    LowFrequency = 3,
    BackLeft = 4,
    BackRight = 5,
    FrontLeftCenter = 6,
    FrontRightCenter = 7,
    BackCenter = 8,
    SideLeft = 9,
    SideRight = 10,
    TopCenter = 11,
    TopFrontLeft = 12,
    TopFrontCenter = 13,
    TopFrontRight = 14,
    TopBackLeft = 15,
    TopBackCenter = 16,
    TopBackRight = 17,
}

impl SurroundChannel {
    /// Map a raw CLAP surround channel ID to a [`SurroundChannel`]. Returns
    /// `None` for IDs outside the known range.
    pub fn from_position(pos: u8) -> Option<Self> {
        match pos {
            0 => Some(Self::FrontLeft),
            1 => Some(Self::FrontRight),
            2 => Some(Self::FrontCenter),
            3 => Some(Self::LowFrequency),
            4 => Some(Self::BackLeft),
            5 => Some(Self::BackRight),
            6 => Some(Self::FrontLeftCenter),
            7 => Some(Self::FrontRightCenter),
            8 => Some(Self::BackCenter),
            9 => Some(Self::SideLeft),
            10 => Some(Self::SideRight),
            11 => Some(Self::TopCenter),
            12 => Some(Self::TopFrontLeft),
            13 => Some(Self::TopFrontCenter),
            14 => Some(Self::TopFrontRight),
            15 => Some(Self::TopBackLeft),
            16 => Some(Self::TopBackCenter),
            17 => Some(Self::TopBackRight),
            _ => None,
        }
    }
}

/// Event flags of a POSIX file descriptor registered by the plugin
/// (`CLAP_EXT_POSIX_FD_SUPPORT`). Only available on Unix targets.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixFdFlags {
    pub read: bool,
    pub write: bool,
    pub error: bool,
}

/// Description of a trigger parameter from `CLAP_EXT_TRIGGERS` (a stateless
/// momentary action, like "reset oscillators").
#[derive(Debug, Clone)]
pub struct TriggerInfo {
    pub id: u32,
    pub flags: u32,
    pub name: String,
    pub module: String,
}

/// Description of a dynamic tuning table the plugin can use via
/// `CLAP_EXT_TUNING`.
#[derive(Debug, Clone)]
pub struct TuningInfo {
    pub tuning_id: u32,
    pub name: String,
    pub is_dynamic: bool,
}

/// Undo delta-format capabilities reported by the plugin.
#[derive(Debug, Clone, Copy)]
pub struct UndoDeltaProperties {
    pub has_delta: bool,
    pub are_deltas_persistent: bool,
    pub format_version: u32,
}

/// An undo step recorded by the plugin: its display name plus an opaque
/// delta blob whose meaning is private to the plugin.
#[derive(Debug, Clone)]
pub struct UndoChange {
    pub name: String,
    pub delta: Vec<u8>,
    pub delta_can_undo: bool,
}
