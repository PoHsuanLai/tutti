//! The values that cross the plugin boundary, as types: what `clap-sys` has as
//! a bare integer or a struct of C arrays.
//!
//! A flag word is a bit set, a closed choice is an enum (with the raw value
//! kept for the ones CLAP may add to), and the structs a plugin fills in
//! (`params.get_info`, `note-name.get`, ...) are owned values with `String`
//! names and no pointer.

use bitflags::bitflags;
use clap_sys::ext::ambisonic::clap_ambisonic_config;
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
    CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
    CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
};
use clap_sys::ext::audio_ports_config::clap_audio_ports_config;
#[cfg(feature = "clap-extras")]
use clap_sys::ext::draft::triggers::clap_trigger_info;
#[cfg(feature = "clap-extras")]
use clap_sys::ext::draft::undo::clap_undo_delta_properties;
use clap_sys::ext::log::{
    CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO,
    CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING,
};
use clap_sys::ext::note_name::clap_note_name;
use clap_sys::ext::note_ports::clap_note_port_info;
use clap_sys::ext::param_indication::{
    CLAP_PARAM_INDICATION_AUTOMATION_NONE, CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING,
    CLAP_PARAM_INDICATION_AUTOMATION_PLAYING, CLAP_PARAM_INDICATION_AUTOMATION_PRESENT,
    CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
};
use clap_sys::ext::params::{
    CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT,
    CLAP_PARAM_RESCAN_VALUES, clap_param_info,
};
#[cfg(feature = "clap-extras")]
use clap_sys::ext::remote_controls::clap_remote_controls_page;
use clap_sys::ext::state_context::{
    CLAP_STATE_CONTEXT_FOR_DUPLICATE, CLAP_STATE_CONTEXT_FOR_PRESET, CLAP_STATE_CONTEXT_FOR_PROJECT,
};
use clap_sys::ext::voice_info::clap_voice_info;
use clap_sys::process::{
    CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, CLAP_PROCESS_SLEEP,
    CLAP_PROCESS_TAIL,
};

use crate::library::chars_to_string;

/// How serious a `clap.log` line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSeverity {
    /// `CLAP_LOG_DEBUG`.
    Debug,
    /// `CLAP_LOG_INFO`.
    Info,
    /// `CLAP_LOG_WARNING`.
    Warning,
    /// `CLAP_LOG_ERROR`.
    Error,
    /// `CLAP_LOG_FATAL`.
    Fatal,
    /// `CLAP_LOG_HOST_MISBEHAVING`.
    HostMisbehaving,
    /// `CLAP_LOG_PLUGIN_MISBEHAVING`.
    PluginMisbehaving,
    /// A severity this version of CLAP does not name; the raw value.
    Unknown(i32),
}

impl LogSeverity {
    /// The severity a raw `clap_log_severity` names.
    #[must_use]
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            CLAP_LOG_DEBUG => Self::Debug,
            CLAP_LOG_INFO => Self::Info,
            CLAP_LOG_WARNING => Self::Warning,
            CLAP_LOG_ERROR => Self::Error,
            CLAP_LOG_FATAL => Self::Fatal,
            CLAP_LOG_HOST_MISBEHAVING => Self::HostMisbehaving,
            CLAP_LOG_PLUGIN_MISBEHAVING => Self::PluginMisbehaving,
            other => Self::Unknown(other),
        }
    }

    /// The raw `clap_log_severity`.
    #[must_use]
    pub fn raw(self) -> i32 {
        match self {
            Self::Debug => CLAP_LOG_DEBUG,
            Self::Info => CLAP_LOG_INFO,
            Self::Warning => CLAP_LOG_WARNING,
            Self::Error => CLAP_LOG_ERROR,
            Self::Fatal => CLAP_LOG_FATAL,
            Self::HostMisbehaving => CLAP_LOG_HOST_MISBEHAVING,
            Self::PluginMisbehaving => CLAP_LOG_PLUGIN_MISBEHAVING,
            Self::Unknown(raw) => raw,
        }
    }
}

/// What a plugin's `process` answered (other than `CLAP_PROCESS_ERROR`, which
/// is a [`ProcessError`](crate::ProcessError)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    /// `CLAP_PROCESS_CONTINUE`.
    Continue,
    /// `CLAP_PROCESS_CONTINUE_IF_NOT_QUIET`.
    ContinueIfNotQuiet,
    /// `CLAP_PROCESS_TAIL`.
    Tail,
    /// `CLAP_PROCESS_SLEEP`.
    Sleep,
    /// A status this version of CLAP does not name; the raw value.
    Unknown(i32),
}

impl ProcessStatus {
    /// The status a raw `clap_process_status` names.
    #[must_use]
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            CLAP_PROCESS_CONTINUE => Self::Continue,
            CLAP_PROCESS_CONTINUE_IF_NOT_QUIET => Self::ContinueIfNotQuiet,
            CLAP_PROCESS_TAIL => Self::Tail,
            CLAP_PROCESS_SLEEP => Self::Sleep,
            other => Self::Unknown(other),
        }
    }

    /// The raw `clap_process_status`.
    #[must_use]
    pub fn raw(self) -> i32 {
        match self {
            Self::Continue => CLAP_PROCESS_CONTINUE,
            Self::ContinueIfNotQuiet => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
            Self::Tail => CLAP_PROCESS_TAIL,
            Self::Sleep => CLAP_PROCESS_SLEEP,
            Self::Unknown(raw) => raw,
        }
    }
}

/// Which use-case a state save or load is for (`clap.state-context`).
///
/// The plugin may serialize differently per context: a preset typically
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

impl StateContext {
    /// The raw `clap_plugin_state_context_type`.
    #[must_use]
    pub fn raw(self) -> u32 {
        match self {
            Self::ForPreset => CLAP_STATE_CONTEXT_FOR_PRESET,
            Self::ForProject => CLAP_STATE_CONTEXT_FOR_PROJECT,
            Self::ForDuplicate => CLAP_STATE_CONTEXT_FOR_DUPLICATE,
        }
    }
}

/// What the host tells a plugin about a parameter's automation
/// (`clap.param-indication`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationState {
    /// No automation.
    None,
    /// Automation exists but is not playing.
    Present,
    /// Automation is playing.
    Playing,
    /// Automation is being recorded.
    Recording,
    /// Automation exists and the user is overriding it.
    Overriding,
}

impl AutomationState {
    /// The raw `CLAP_PARAM_INDICATION_AUTOMATION_*` value.
    #[must_use]
    pub fn raw(self) -> u32 {
        match self {
            Self::None => CLAP_PARAM_INDICATION_AUTOMATION_NONE,
            Self::Present => CLAP_PARAM_INDICATION_AUTOMATION_PRESENT,
            Self::Playing => CLAP_PARAM_INDICATION_AUTOMATION_PLAYING,
            Self::Recording => CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
            Self::Overriding => CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING,
        }
    }
}

bitflags! {
    /// Which parts of its parameters a plugin asks the host to rescan
    /// (`clap_param_rescan_flags`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ParamsRescanFlags: u32 {
        /// Values changed.
        const VALUES = CLAP_PARAM_RESCAN_VALUES;
        /// Value texts changed.
        const TEXT = CLAP_PARAM_RESCAN_TEXT;
        /// Info (names, ranges, flags) changed.
        const INFO = CLAP_PARAM_RESCAN_INFO;
        /// The whole list changed.
        const ALL = CLAP_PARAM_RESCAN_ALL;
    }

    /// Which parts of its audio ports a plugin asks the host to rescan
    /// (`CLAP_AUDIO_PORTS_RESCAN_*`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct AudioPortsRescanFlags: u32 {
        /// Port names changed.
        const NAMES = CLAP_AUDIO_PORTS_RESCAN_NAMES;
        /// Port flags changed.
        const FLAGS = CLAP_AUDIO_PORTS_RESCAN_FLAGS;
        /// Channel counts changed.
        const CHANNEL_COUNT = CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT;
        /// Port types changed.
        const PORT_TYPE = CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE;
        /// In-place pairs changed.
        const IN_PLACE_PAIR = CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR;
        /// The list of ports changed.
        const LIST = CLAP_AUDIO_PORTS_RESCAN_LIST;
    }

    /// Audio port capability flags from `clap_audio_port_info`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct AudioPortFlags: u32 {
        /// This is the plugin's main port for its direction. At most one
        /// input and one output port may carry it.
        const MAIN = 1 << 0;
        /// The port can process 64-bit buffers as well as 32-bit.
        const SUPPORTS_64BIT = 1 << 1;
        /// The port would rather be given 64-bit buffers.
        const PREFERS_64BIT = 1 << 2;
        /// Every port on the plugin must be given the same sample width in a
        /// given `process` call: 32 and 64 bit cannot be mixed.
        const REQUIRES_COMMON_SAMPLE_SIZE = 1 << 3;
    }
}

/// `params.get_info`, owned.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    /// The parameter's id.
    pub id: u32,
    /// The raw `clap_param_info_flags`.
    pub flags: u32,
    /// Display name.
    pub name: String,
    /// `/`-separated display path.
    pub module: String,
    /// Smallest plain value.
    pub min_value: f64,
    /// Largest plain value.
    pub max_value: f64,
    /// Default plain value.
    pub default_value: f64,
}

impl ParamInfo {
    pub(crate) fn from_raw(info: &clap_param_info) -> Self {
        Self {
            id: info.id,
            flags: info.flags,
            name: chars_to_string(&info.name),
            module: chars_to_string(&info.module),
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
        }
    }
}

/// `voice-info.get`, owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceInfo {
    /// Voices the plugin plays at once.
    pub voice_count: u32,
    /// Voices it can play at most.
    pub voice_capacity: u32,
    /// The raw `CLAP_VOICE_INFO_*` flags.
    pub flags: u64,
}

impl VoiceInfo {
    pub(crate) fn from_raw(info: &clap_voice_info) -> Self {
        Self {
            voice_count: info.voice_count,
            voice_capacity: info.voice_capacity,
            flags: info.flags,
        }
    }
}

/// `note-name.get`, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteName {
    /// The name.
    pub name: String,
    /// Note port it applies to; `-1` for every port.
    pub port: i16,
    /// Channel it applies to; `-1` for every channel.
    pub channel: i16,
    /// Key it applies to; `-1` for every key.
    pub key: i16,
}

impl NoteName {
    pub(crate) fn from_raw(info: &clap_note_name) -> Self {
        Self {
            name: chars_to_string(&info.name),
            port: info.port,
            channel: info.channel,
            key: info.key,
        }
    }
}

/// `note-ports.get`, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotePortDecl {
    /// The port's id.
    pub id: u32,
    /// The raw `CLAP_NOTE_DIALECT_*` bits it supports.
    pub supported_dialects: u32,
    /// The raw dialect it prefers (0 for none).
    pub preferred_dialect: u32,
    /// Display name.
    pub name: String,
}

impl NotePortDecl {
    pub(crate) fn from_raw(info: &clap_note_port_info) -> Self {
        Self {
            id: info.id,
            supported_dialects: info.supported_dialects,
            preferred_dialect: info.preferred_dialect,
            name: chars_to_string(&info.name),
        }
    }
}

/// `audio-ports-config.get`, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPortsConfig {
    /// The configuration's id.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Input ports it has.
    pub input_port_count: u32,
    /// Output ports it has.
    pub output_port_count: u32,
    /// Whether it has a main input.
    pub has_main_input: bool,
    /// Channels of the main input.
    pub main_input_channel_count: u32,
    /// Whether it has a main output.
    pub has_main_output: bool,
    /// Channels of the main output.
    pub main_output_channel_count: u32,
}

impl AudioPortsConfig {
    pub(crate) fn from_raw(config: &clap_audio_ports_config) -> Self {
        Self {
            id: config.id,
            name: chars_to_string(&config.name),
            input_port_count: config.input_port_count,
            output_port_count: config.output_port_count,
            has_main_input: config.has_main_input,
            main_input_channel_count: config.main_input_channel_count,
            has_main_output: config.has_main_output,
            main_output_channel_count: config.main_output_channel_count,
        }
    }
}

/// `ambisonic.get_config`, owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbisonicDecl {
    /// The raw `CLAP_AMBISONIC_ORDERING_*`.
    pub ordering: u32,
    /// The raw `CLAP_AMBISONIC_NORMALIZATION_*`.
    pub normalization: u32,
}

impl AmbisonicDecl {
    pub(crate) fn from_raw(config: &clap_ambisonic_config) -> Self {
        Self {
            ordering: config.ordering,
            normalization: config.normalization,
        }
    }
}

/// `remote-controls.get`, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(feature = "clap-extras")]
pub struct RemoteControlsPageDecl {
    /// The section the page belongs to.
    pub section_name: String,
    /// The page's id.
    pub page_id: u32,
    /// The page's name.
    pub page_name: String,
    /// The eight parameters it maps (`u32::MAX` for none).
    pub param_ids: [u32; 8],
    /// Whether the page is for the preset rather than the plugin.
    pub is_for_preset: bool,
}

#[cfg(feature = "clap-extras")]
impl RemoteControlsPageDecl {
    pub(crate) fn from_raw(page: &clap_remote_controls_page) -> Self {
        Self {
            section_name: chars_to_string(&page.section_name),
            page_id: page.page_id,
            page_name: chars_to_string(&page.page_name),
            param_ids: page.param_ids,
            is_for_preset: page.is_for_preset,
        }
    }
}

/// `triggers.get_info`, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(feature = "clap-extras")]
pub struct TriggerDecl {
    /// The trigger's id.
    pub id: u32,
    /// The raw `CLAP_TRIGGER_*` flags.
    pub flags: u32,
    /// Display name.
    pub name: String,
    /// `/`-separated display path.
    pub module: String,
}

#[cfg(feature = "clap-extras")]
impl TriggerDecl {
    pub(crate) fn from_raw(info: &clap_trigger_info) -> Self {
        Self {
            id: info.id,
            flags: info.flags,
            name: chars_to_string(&info.name),
            module: chars_to_string(&info.module),
        }
    }
}

/// `undo-delta.get_delta_properties`, owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "clap-extras")]
pub struct UndoDeltaDecl {
    /// Whether the plugin produces deltas.
    pub has_delta: bool,
    /// Whether they persist across sessions.
    pub are_deltas_persistent: bool,
    /// The format version it writes.
    pub format_version: u32,
}

#[cfg(feature = "clap-extras")]
impl UndoDeltaDecl {
    pub(crate) fn from_raw(props: &clap_undo_delta_properties) -> Self {
        Self {
            has_delta: props.has_delta,
            are_deltas_persistent: props.are_deltas_persistent,
            format_version: props.format_version,
        }
    }
}
