//! Audio/note port enumeration, configuration, and the render / voice /
//! surround / ambisonic extensions.

use super::ClapLoaded;
use crate::types::{
    AmbisonicConfig, AmbisonicNormalization, AmbisonicOrdering, AudioPortConfig, AudioPortFlags,
    AudioPortInfo, ChannelLayout, NoteDialect, NoteDialects, NoteName, NotePortInfo,
    SurroundChannel, VoiceInfo,
};
// Audio-port *reconfiguration* is speculative (gated); the type it consumes.
#[cfg(feature = "clap-extras")]
use crate::types::AudioPortConfigRequest;
use clap_sys::ext::ambisonic::{
    clap_ambisonic_config, CLAP_AMBISONIC_NORMALIZATION_MAXN, CLAP_AMBISONIC_NORMALIZATION_N2D,
    CLAP_AMBISONIC_NORMALIZATION_N3D, CLAP_AMBISONIC_NORMALIZATION_SN2D,
    CLAP_AMBISONIC_NORMALIZATION_SN3D, CLAP_AMBISONIC_ORDERING_ACN, CLAP_AMBISONIC_ORDERING_FUMA,
};
use clap_sys::ext::audio_ports::{clap_audio_port_info, CLAP_PORT_MONO, CLAP_PORT_STEREO};
use clap_sys::ext::audio_ports_config::clap_audio_ports_config;
#[cfg(feature = "clap-extras")]
use clap_sys::ext::configurable_audio_ports::clap_audio_port_configuration_request;
use clap_sys::ext::note_name::clap_note_name;
use clap_sys::ext::note_ports::{
    clap_note_port_info, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI2,
    CLAP_NOTE_DIALECT_MIDI_MPE,
};
use clap_sys::ext::render::{CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME};
use clap_sys::ext::voice_info::{clap_voice_info, CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES};
use std::ffi::CStr;
#[cfg(feature = "clap-extras")]
use std::ptr;

use crate::cstr_to_string;

impl ClapLoaded {
    /// Number of input or output audio ports exposed by the plugin.
    ///
    /// `u32` because that is what CLAP's own `count`/`get` pair speaks, and it
    /// is the type every port index in this file is denominated in. Four of
    /// these accessors used to widen the count to `usize` and narrow the index
    /// back to `u32` at the FFI call — a matched pair of conversions that
    /// bought nothing and left one concept with two spellings in one file.
    pub fn audio_port_count(&self, is_input: bool) -> u32 {
        if self.extensions.audio.ports.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.audio.ports };
        match ext.count {
            Some(f) => unsafe { f(self.plugin.as_ptr(), is_input) },
            None => 0,
        }
    }

    /// Metadata for the audio port at `index`, or `None` if the index is
    /// invalid or the plugin does not implement the extension.
    pub fn audio_port_info(&self, index: u32, is_input: bool) -> Option<AudioPortInfo> {
        if self.extensions.audio.ports.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.audio.ports };
        let get_fn = ext.get?;

        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), index, is_input, &mut info) } {
            return None;
        }

        Some(audio_port_info_from_clap(&info))
    }

    /// Total channel count summed across one side's audio ports.
    ///
    /// Stops at the first index `get` rejects. A sum is order-free, so a hole
    /// misattributes nothing here — but this total must describe the same
    /// truncated list [`port_channels`](super::load) presents, or it stops
    /// matching `PortLayout::{input,output}_channel_total`, which is what sizes
    /// the process scratch.
    fn channel_total(&self, is_input: bool) -> usize {
        let count = self.audio_port_count(is_input);
        let mut total = 0usize;
        for i in 0..count {
            let Some(port) = self.audio_port_info(i, is_input) else {
                break;
            };
            total += port.layout.count() as usize;
        }
        total
    }

    /// Total input channel count, summed across every input port.
    pub fn num_input_channels(&self) -> usize {
        self.channel_total(true)
    }

    /// Total output channel count, summed across every output port.
    pub fn num_output_channels(&self) -> usize {
        self.channel_total(false)
    }

    /// Number of input or output note (MIDI) ports.
    pub fn note_port_count(&self, is_input: bool) -> u32 {
        if self.extensions.notes.ports.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.notes.ports };
        match ext.count {
            Some(f) => unsafe { f(self.plugin.as_ptr(), is_input) },
            None => 0,
        }
    }

    /// Metadata for the note port at `index`, including supported dialects.
    pub fn note_port_info(&self, index: u32, is_input: bool) -> Option<NotePortInfo> {
        if self.extensions.notes.ports.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.notes.ports };
        let get_fn = ext.get?;

        let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), index, is_input, &mut info) } {
            return None;
        }

        // `ext/note-ports.h:35` types this field as "one value of
        // clap_note_dialect", so it is matched whole rather than by priority
        // over a bitfield. Anything else — 0 for a plugin that stated no
        // preference, or a dialect added after this match — is `None`, not a
        // dialect this host picked on the plugin's behalf.
        let preferred_dialect = match info.preferred_dialect {
            CLAP_NOTE_DIALECT_CLAP => Some(NoteDialect::Clap),
            CLAP_NOTE_DIALECT_MIDI => Some(NoteDialect::Midi),
            CLAP_NOTE_DIALECT_MIDI_MPE => Some(NoteDialect::MidiMpe),
            CLAP_NOTE_DIALECT_MIDI2 => Some(NoteDialect::Midi2),
            _ => None,
        };

        Some(NotePortInfo {
            id: info.id,
            name: unsafe { cstr_to_string(info.name.as_ptr()) },
            supported_dialects: NoteDialects::from_bits_truncate(info.supported_dialects),
            preferred_dialect,
        })
    }

    /// Number of predefined port configurations
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG`) the plugin offers.
    pub fn audio_port_config_count(&self) -> u32 {
        if self.extensions.audio.ports_config.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.audio.ports_config };
        match ext.count {
            Some(f) => unsafe { f(self.plugin.as_ptr()) },
            None => 0,
        }
    }

    /// Describe the audio port configuration at `index`.
    pub fn get_audio_port_config(&self, index: u32) -> Option<AudioPortConfig> {
        if self.extensions.audio.ports_config.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.audio.ports_config };
        let get_fn = ext.get?;

        let mut config: clap_audio_ports_config = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), index, &mut config) } {
            return None;
        }

        Some(AudioPortConfig {
            id: config.id,
            name: unsafe { cstr_to_string(config.name.as_ptr()) },
            input_port_count: config.input_port_count,
            output_port_count: config.output_port_count,
            has_main_input: config.has_main_input,
            main_input_channel_count: config.main_input_channel_count,
            has_main_output: config.has_main_output,
            main_output_channel_count: config.main_output_channel_count,
        })
    }

    /// Ask the plugin to switch to a previously reported port configuration.
    /// Returns whether the plugin accepted the request.
    pub fn select_audio_port_config(&mut self, config_id: u32) -> bool {
        if self.extensions.audio.ports_config.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.ports_config };
        match ext.select {
            Some(f) => unsafe { f(self.plugin.as_ptr(), config_id) },
            None => false,
        }
    }

    /// The `id` of the audio-ports configuration currently active
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG_INFO`). Returns `None` when the plugin
    /// does not implement the extension.
    ///
    /// This is the companion to [`audio_port_config_count`](Self::audio_port_config_count) /
    /// [`get_audio_port_config`](Self::get_audio_port_config): those enumerate
    /// the *available* configs; this reports which one is live and lets you
    /// read its full per-port info via
    /// [`audio_port_config_port_info`](Self::audio_port_config_port_info).
    pub fn current_audio_port_config(&self) -> Option<u32> {
        if self.extensions.audio.ports_config_info.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.audio.ports_config_info };
        let current_fn = ext.current_config?;
        Some(unsafe { current_fn(self.plugin.as_ptr()) })
    }

    /// Full [`AudioPortInfo`] for a single port *within a specific config*
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG_INFO`). Unlike
    /// [`get_audio_port_config`](Self::get_audio_port_config) (which returns
    /// only the config summary), this exposes each port's id / name / channel
    /// count / flags / type for the named `config_id`, without first having to
    /// switch to it. Returns `None` when unsupported or the plugin rejects the
    /// `(config_id, port_index, is_input)` triple.
    pub fn audio_port_config_port_info(
        &self,
        config_id: u32,
        port_index: u32,
        is_input: bool,
    ) -> Option<AudioPortInfo> {
        if self.extensions.audio.ports_config_info.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.audio.ports_config_info };
        let get_fn = ext.get?;

        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if !unsafe {
            get_fn(
                self.plugin.as_ptr(),
                config_id,
                port_index,
                is_input,
                &mut info,
            )
        } {
            return None;
        }
        Some(audio_port_info_from_clap(&info))
    }

    /// Plugin-reported processing latency in samples. 0 when unsupported.
    pub fn get_latency(&self) -> u32 {
        if self.extensions.system.latency.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.system.latency };
        match ext.get {
            Some(f) => unsafe { f(self.plugin.as_ptr()) },
            None => 0,
        }
    }

    /// Plugin-reported tail length in samples (audio continues after input
    /// stops — reverbs, delays). 0 when unsupported.
    pub fn get_tail(&self) -> u32 {
        if self.extensions.system.tail.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.system.tail };
        match ext.get {
            Some(f) => unsafe { f(self.plugin.as_ptr()) },
            None => 0,
        }
    }

    /// Switch between real-time (`false`) and offline (`true`) rendering
    /// modes per `CLAP_EXT_RENDER`. Returns whether the plugin accepted.
    pub fn set_render_mode(&mut self, offline: bool) -> bool {
        if self.extensions.system.render.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.system.render };
        match ext.set {
            Some(f) => {
                let mode = if offline {
                    CLAP_RENDER_OFFLINE
                } else {
                    CLAP_RENDER_REALTIME
                };
                unsafe { f(self.plugin.as_ptr(), mode) }
            }
            None => false,
        }
    }

    /// Whether the plugin requires a hard real-time environment (e.g. it
    /// talks to hardware). Hosts should avoid running such plugins offline.
    pub fn has_hard_realtime_requirement(&self) -> bool {
        if self.extensions.system.render.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.system.render };
        match ext.has_hard_realtime_requirement {
            Some(f) => unsafe { f(self.plugin.as_ptr()) },
            None => false,
        }
    }

    /// Voice-allocation information from `CLAP_EXT_VOICE_INFO`.
    pub fn get_voice_info(&self) -> Option<VoiceInfo> {
        if self.extensions.system.voice_info.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.system.voice_info };
        let get_fn = ext.get?;
        let mut info: clap_voice_info = unsafe { std::mem::zeroed() };
        if unsafe { get_fn(self.plugin.as_ptr(), &mut info) } {
            Some(VoiceInfo {
                voice_count: info.voice_count,
                voice_capacity: info.voice_capacity,
                supports_overlapping_notes: (info.flags
                    & CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES)
                    != 0,
            })
        } else {
            None
        }
    }

    /// Number of custom note names (e.g. drum-kit labels) the plugin
    /// exposes via `CLAP_EXT_NOTE_NAME`.
    pub fn note_name_count(&self) -> u32 {
        if self.extensions.notes.name.is_null() {
            return 0;
        }
        let ext = unsafe { &*self.extensions.notes.name };
        match ext.count {
            Some(f) => unsafe { f(self.plugin.as_ptr()) },
            None => 0,
        }
    }

    /// Retrieve a single custom note name.
    pub fn get_note_name(&self, index: u32) -> Option<NoteName> {
        if self.extensions.notes.name.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.notes.name };
        let get_fn = ext.get?;
        let mut info: clap_note_name = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), index, &mut info) } {
            return None;
        }
        Some(NoteName {
            name: unsafe { cstr_to_string(info.name.as_ptr()) },
            port: info.port,
            channel: info.channel,
            key: info.key,
        })
    }

    /// Ask the plugin whether it could apply a set of port-configuration
    /// requests without actually applying them. Speculative (audio-port
    /// reconfiguration) — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn can_apply_audio_port_configuration(&self, requests: &[AudioPortConfigRequest]) -> bool {
        if self.extensions.audio.configurable_ports.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.configurable_ports };
        let can_apply_fn = match ext.can_apply_configuration {
            Some(f) => f,
            None => return false,
        };
        let clap_requests = build_port_config_requests(requests);
        unsafe {
            can_apply_fn(
                self.plugin.as_ptr(),
                clap_requests.as_ptr(),
                clap_requests.len() as u32,
            )
        }
    }

    /// Apply a set of port-configuration requests via
    /// `CLAP_EXT_CONFIGURABLE_AUDIO_PORTS`. Returns success. Speculative — gated
    /// behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn apply_audio_port_configuration(&mut self, requests: &[AudioPortConfigRequest]) -> bool {
        if self.extensions.audio.configurable_ports.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.configurable_ports };
        let apply_fn = match ext.apply_configuration {
            Some(f) => f,
            None => return false,
        };
        let clap_requests = build_port_config_requests(requests);
        unsafe {
            apply_fn(
                self.plugin.as_ptr(),
                clap_requests.as_ptr(),
                clap_requests.len() as u32,
            )
        }
    }

    /// Whether the plugin supports activating/deactivating ports while
    /// processing is running. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn can_activate_audio_port_while_processing(&self) -> bool {
        if self.extensions.audio.ports_activation.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.ports_activation };
        match ext.can_activate_while_processing {
            Some(f) => unsafe { f(self.plugin.as_ptr()) },
            None => false,
        }
    }

    /// Activate or deactivate a single audio port.
    /// `sample_size` is the bit depth (32 or 64). Speculative — gated behind
    /// `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn set_audio_port_active(
        &mut self,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        sample_size: u32,
    ) -> bool {
        if self.extensions.audio.ports_activation.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.ports_activation };
        match ext.set_active {
            Some(f) => unsafe {
                f(
                    self.plugin.as_ptr(),
                    is_input,
                    port_index,
                    is_active,
                    sample_size,
                )
            },
            None => false,
        }
    }

    /// Ask the plugin to add a new port via the draft
    /// `CLAP_EXT_EXTENSIBLE_AUDIO_PORTS`. Returns whether the plugin added it.
    /// Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn add_audio_port(
        &mut self,
        is_input: bool,
        channel_count: u32,
        port_type: Option<&str>,
    ) -> bool {
        if self.extensions.audio.extensible_ports.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.extensible_ports };
        let add_fn = match ext.add_port {
            Some(f) => f,
            None => return false,
        };
        let type_cstr = port_type.and_then(|s| std::ffi::CString::new(s).ok());
        let type_ptr = type_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(ptr::null());
        unsafe {
            add_fn(
                self.plugin.as_ptr(),
                is_input,
                channel_count,
                type_ptr,
                ptr::null(),
            )
        }
    }

    /// Counterpart to [`Self::add_audio_port`]. Returns whether the plugin
    /// removed the port. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
    pub fn remove_audio_port(&mut self, is_input: bool, index: u32) -> bool {
        if self.extensions.audio.extensible_ports.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.extensible_ports };
        match ext.remove_port {
            Some(f) => unsafe { f(self.plugin.as_ptr(), is_input, index) },
            None => false,
        }
    }

    /// Ask whether the plugin can process the given ambisonic ordering +
    /// normalization. Returns false when `CLAP_EXT_AMBISONIC` is unsupported.
    pub fn is_ambisonic_config_supported(&self, config: &AmbisonicConfig) -> bool {
        if self.extensions.audio.ambisonic.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.ambisonic };
        let f = match ext.is_config_supported {
            Some(f) => f,
            None => return false,
        };
        let clap_config = clap_ambisonic_config {
            ordering: match config.ordering {
                AmbisonicOrdering::Fuma => CLAP_AMBISONIC_ORDERING_FUMA,
                AmbisonicOrdering::Acn => CLAP_AMBISONIC_ORDERING_ACN,
            },
            normalization: match config.normalization {
                AmbisonicNormalization::MaxN => CLAP_AMBISONIC_NORMALIZATION_MAXN,
                AmbisonicNormalization::Sn3d => CLAP_AMBISONIC_NORMALIZATION_SN3D,
                AmbisonicNormalization::N3d => CLAP_AMBISONIC_NORMALIZATION_N3D,
                AmbisonicNormalization::Sn2d => CLAP_AMBISONIC_NORMALIZATION_SN2D,
                AmbisonicNormalization::N2d => CLAP_AMBISONIC_NORMALIZATION_N2D,
            },
        };
        unsafe { f(self.plugin.as_ptr(), &clap_config) }
    }

    /// Retrieve the ambisonic config currently used on a given port.
    pub fn get_ambisonic_config(&self, is_input: bool, port_index: u32) -> Option<AmbisonicConfig> {
        if self.extensions.audio.ambisonic.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.audio.ambisonic };
        let get_fn = ext.get_config?;
        let mut config: clap_ambisonic_config = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(self.plugin.as_ptr(), is_input, port_index, &mut config) } {
            return None;
        }
        let ordering = match config.ordering {
            CLAP_AMBISONIC_ORDERING_FUMA => AmbisonicOrdering::Fuma,
            _ => AmbisonicOrdering::Acn,
        };
        let normalization = match config.normalization {
            CLAP_AMBISONIC_NORMALIZATION_MAXN => AmbisonicNormalization::MaxN,
            CLAP_AMBISONIC_NORMALIZATION_SN3D => AmbisonicNormalization::Sn3d,
            CLAP_AMBISONIC_NORMALIZATION_N3D => AmbisonicNormalization::N3d,
            CLAP_AMBISONIC_NORMALIZATION_SN2D => AmbisonicNormalization::Sn2d,
            _ => AmbisonicNormalization::N2d,
        };
        Some(AmbisonicConfig {
            ordering,
            normalization,
        })
    }

    /// Ask whether the plugin can process the given
    /// [`SurroundChannel`]-bit channel mask (`CLAP_EXT_SURROUND`).
    pub fn is_surround_channel_mask_supported(&self, channel_mask: u64) -> bool {
        if self.extensions.audio.surround.is_null() {
            return false;
        }
        let ext = unsafe { &*self.extensions.audio.surround };
        match ext.is_channel_mask_supported {
            Some(f) => unsafe { f(self.plugin.as_ptr(), channel_mask) },
            None => false,
        }
    }

    /// Retrieve the channel-to-speaker mapping the plugin uses on a port.
    ///
    /// The result is indexed **by channel**: element `i` is the speaker fed by
    /// channel `i`, so it always has `count` elements. A position this crate
    /// cannot name arrives as [`SurroundChannel::Unknown`] rather than being
    /// dropped — omitting it would renumber every channel after it.
    ///
    /// `None` means the plugin cannot answer — no `clap.surround` extension or
    /// no `get_channel_map`. An empty `Vec` means it answered with no channels:
    /// `get_channel_map` returns "the number of elements stored", so `0` is
    /// data, not failure, and the old `count == 0 || count > map.len()` folded
    /// the two together. Only the capacity overrun is genuinely broken — the
    /// plugin claims to have written past the 64 elements it was given, so
    /// nothing in the buffer can be trusted.
    pub fn get_surround_channel_map(
        &self,
        is_input: bool,
        port_index: u32,
    ) -> Option<Vec<SurroundChannel>> {
        if self.extensions.audio.surround.is_null() {
            return None;
        }
        let ext = unsafe { &*self.extensions.audio.surround };
        let get_fn = ext.get_channel_map?;
        let mut map = [0u8; 64];
        let count = unsafe {
            get_fn(
                self.plugin.as_ptr(),
                is_input,
                port_index,
                map.as_mut_ptr(),
                64,
            )
        } as usize;
        decode_surround_channel_map(&map, count)
    }
}

/// Decode the first `count` entries of a plugin-filled surround channel map.
///
/// Split out of [`ClapLoaded::get_surround_channel_map`] so the count
/// validation is testable without a live `ClapLoaded`, which no stub vtable can
/// produce.
fn decode_surround_channel_map(map: &[u8], count: usize) -> Option<Vec<SurroundChannel>> {
    // A count past the capacity we handed over is a plugin bug and makes every
    // element suspect — reject. `count == 0` is a real empty map.
    if count > map.len() {
        return None;
    }
    // `map`, not `filter_map`: the result is indexed by channel, so an
    // unnameable position must keep its slot. Filtering shortened the vector
    // and renumbered every channel after the dropped one.
    Some(
        map[..count]
            .iter()
            .map(|&pos| SurroundChannel::from_position(pos))
            .collect(),
    )
}

/// Convert a raw `clap_audio_port_info` into the safe [`AudioPortInfo`].
/// Shared by [`ClapLoaded::audio_port_info`] and
/// [`ClapLoaded::audio_port_config_port_info`].
fn audio_port_info_from_clap(info: &clap_audio_port_info) -> AudioPortInfo {
    AudioPortInfo {
        id: info.id,
        name: unsafe { cstr_to_string(info.name.as_ptr()) },
        layout: layout_from_clap_port(info.port_type, info.channel_count),
        flags: AudioPortFlags::from_bits_truncate(info.flags),
        in_place_pair_id: info.in_place_pair,
    }
}

/// Lossy FFI-inbound conversion of a CLAP port's `port_type` tag + reported
/// `channel_count` into a [`ChannelLayout`].
///
/// Kept a named boundary fn rather than a `From` impl: it needs the
/// `channel_count` fallback for tags we don't recognize (the tag string itself
/// is dropped — nothing downstream reads it), and it borrows a raw FFI pointer.
/// `CLAP_PORT_MONO`/`CLAP_PORT_STEREO` map to the named variants; any other tag
/// (surround, ambisonic, vendor-specific) becomes `Multi(channel_count)`.
///
/// Shared with [`super::load::port_channels`], so the layout `PortLayout`
/// stores is the same one `audio_port_info` reports — one conversion, one
/// answer.
pub(super) fn layout_from_clap_port(
    port_type: *const std::os::raw::c_char,
    channel_count: u32,
) -> ChannelLayout {
    if !port_type.is_null() {
        let tag = unsafe { CStr::from_ptr(port_type) };
        if tag == CLAP_PORT_MONO {
            return ChannelLayout::MONO;
        }
        if tag == CLAP_PORT_STEREO {
            return ChannelLayout::STEREO;
        }
    }
    ChannelLayout::from(channel_count)
}

#[cfg(feature = "clap-extras")]
fn build_port_config_requests(
    requests: &[AudioPortConfigRequest],
) -> Vec<clap_audio_port_configuration_request> {
    requests
        .iter()
        .map(|r| clap_audio_port_configuration_request {
            is_input: r.is_input,
            port_index: r.port_index,
            channel_count: r.channel_count,
            port_type: ptr::null(),
            port_details: ptr::null(),
        })
        .collect()
}

#[cfg(test)]
mod surround_map_tests {
    use super::*;

    /// A plugin reporting **zero** channels is answering, not failing — the old
    /// `count == 0 || count > map.len()` guard folded that in with the error.
    #[test]
    fn zero_count_is_an_empty_map_not_a_failure() {
        let map = [0u8; 64];
        assert_eq!(
            decode_surround_channel_map(&map, 0),
            Some(Vec::new()),
            "a zero-length map is data the plugin returned, not a failure"
        );
    }

    /// A count past the capacity the host handed over is a genuine plugin bug:
    /// the plugin claims to have written more than it was given room for, so no
    /// element can be trusted.
    #[test]
    fn count_past_capacity_is_rejected() {
        let map = [0u8; 64];
        assert_eq!(decode_surround_channel_map(&map, 65), None);
    }

    /// The ordinary path still decodes, and stops at `count` rather than
    /// running to the end of the buffer.
    #[test]
    fn decodes_exactly_count_entries() {
        let mut map = [0u8; 64];
        map[0] = 0; // FrontLeft
        map[1] = 1; // FrontRight
        map[2] = 2; // FrontCenter — past `count`, must not appear
        let decoded = decode_surround_channel_map(&map, 2).expect("valid count");
        assert_eq!(
            decoded,
            vec![SurroundChannel::FrontLeft, SurroundChannel::FrontRight]
        );
    }

    /// Every position CLAP defines decodes, including the top-side pair.
    ///
    /// `CLAP_SURROUND_TSL`/`TSR` (18/19, `surround.h:55-56`) are the last two
    /// constants and the ones a 7.1.4 / Atmos bed actually uses. They were
    /// missing, which the next test shows is worse than a missing name.
    #[test]
    fn every_clap_position_decodes() {
        let map: Vec<u8> = (0..=19).collect();
        let decoded = decode_surround_channel_map(&map, map.len()).expect("valid count");
        assert_eq!(
            decoded.len(),
            20,
            "CLAP defines positions 0..=19; a short result means one was dropped"
        );
        assert_eq!(decoded[18], SurroundChannel::TopSideLeft);
        assert_eq!(decoded[19], SurroundChannel::TopSideRight);
    }

    /// An unknown position keeps its channel's slot instead of collapsing it.
    ///
    /// The map is **positional** — `map[i]` is the speaker fed by channel `i` —
    /// so dropping an entry silently renumbers every channel after it. That is
    /// the failure this guards: a 12-channel bed whose channel 4 is unreadable
    /// must not report channel 5's speaker as channel 4's.
    ///
    /// A future CLAP revision adding position 20 is the realistic source of an
    /// unknown value, which is why this is `Unknown(u8)` rather than an error:
    /// one unnameable speaker should not discard the eleven that decoded.
    #[test]
    fn an_unknown_position_holds_its_slot_rather_than_shifting_the_rest() {
        // Positions 0..=3 then an unallocated one, then 5.
        let map = [0u8, 1, 2, 3, 200, 5];
        let decoded = decode_surround_channel_map(&map, 6).expect("valid count");

        assert_eq!(
            decoded.len(),
            6,
            "one unreadable position must not shorten a positional map"
        );
        assert_eq!(decoded[4], SurroundChannel::Unknown(200));
        assert_eq!(
            decoded[5],
            SurroundChannel::BackRight,
            "channel 5 must still report its own speaker, not channel 4's"
        );
    }
}
