//! Audio/note port enumeration, configuration, and the render / voice /
//! surround / ambisonic extensions.

use super::ClapLoaded;
use super::load::layout_of_port;
use crate::types::{
    AmbisonicConfig, AmbisonicNormalization, AmbisonicOrdering, AudioPortConfig, AudioPortFlags,
    AudioPortInfo, NoteDialect, NoteDialects, NoteName, NotePortInfo, SurroundChannel, VoiceInfo,
};
use tutti_plugin_types::MainThread;
use tutti_types::ChannelTopology;
// Audio-port *reconfiguration* is speculative (gated); the type it consumes.
#[cfg(feature = "clap-extras")]
use crate::types::AudioPortConfigRequest;
use clap_sys::ext::ambisonic::{
    CLAP_AMBISONIC_NORMALIZATION_MAXN, CLAP_AMBISONIC_NORMALIZATION_N2D,
    CLAP_AMBISONIC_NORMALIZATION_N3D, CLAP_AMBISONIC_NORMALIZATION_SN2D,
    CLAP_AMBISONIC_NORMALIZATION_SN3D, CLAP_AMBISONIC_ORDERING_ACN, CLAP_AMBISONIC_ORDERING_FUMA,
    clap_ambisonic_config,
};
use clap_sys::ext::note_ports::{
    CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI_MPE,
    CLAP_NOTE_DIALECT_MIDI2,
};
use clap_sys::ext::voice_info::CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES;
#[cfg(feature = "clap-extras")]
use tutti_clap_ffi::PortConfigRequest;
use tutti_clap_ffi::chars_to_string;

impl ClapLoaded {
    /// Number of input or output audio ports exposed by the plugin.
    ///
    /// `u32` because that is what CLAP's own `count`/`get` pair speaks, and it
    /// is the type every port index in this file is denominated in. Widening
    /// the count to `usize` and narrowing the index back at the FFI call is a
    /// matched pair of conversions that buys nothing and leaves one concept
    /// with two spellings in one file.
    pub fn audio_port_count(&self, main: &MainThread, is_input: bool) -> u32 {
        self.plugin.audio_port_count(main, is_input)
    }

    /// Metadata for the audio port at `index`, or `None` if the index is
    /// invalid, the plugin does not implement the extension, or the port
    /// reports more channels than a layout holds.
    pub fn audio_port_info(
        &self,
        main: &MainThread,
        index: u32,
        is_input: bool,
    ) -> Option<AudioPortInfo> {
        self.plugin
            .audio_port_info(main, index, is_input)
            .and_then(|info| audio_port_info_from_ffi(&info))
    }

    /// Total channel count summed across one side's audio ports.
    ///
    /// Stops at the first index `get` rejects. A sum is order-free, so a hole
    /// misattributes nothing here — but this total must describe the same
    /// truncated list [`port_channels`](super::load) presents, or it stops
    /// matching `PortLayout::{input,output}_channel_total`, which is what sizes
    /// the process scratch.
    fn channel_total(&self, main: &MainThread, is_input: bool) -> usize {
        let count = self.audio_port_count(main, is_input);
        let mut total = 0usize;
        for i in 0..count {
            let Some(port) = self.audio_port_info(main, i, is_input) else {
                break;
            };
            total += port.layout.count() as usize;
        }
        total
    }

    /// Total input channel count, summed across every input port.
    pub fn num_input_channels(&self, main: &MainThread) -> usize {
        self.channel_total(main, true)
    }

    /// Total output channel count, summed across every output port.
    pub fn num_output_channels(&self, main: &MainThread) -> usize {
        self.channel_total(main, false)
    }

    /// Number of input or output note (MIDI) ports.
    pub fn note_port_count(&self, main: &MainThread, is_input: bool) -> u32 {
        self.plugin.note_port_count(main, is_input)
    }

    /// Whether notes to the first input note port go out as `clap_event_midi`:
    /// the port takes the MIDI dialect and not CLAP's own note events (or
    /// prefers MIDI, see [`NotePortInfo::dialect_to_send`]). Only input note
    /// port 0 decides. A plugin with no
    /// note port gets CLAP note events, as before.
    pub fn sends_midi_notes(&self, main: &MainThread) -> bool {
        self.note_port_info(main, 0, true)
            .and_then(|port| port.dialect_to_send())
            == Some(NoteDialect::Midi)
    }

    /// Metadata for the note port at `index`, including supported dialects.
    pub fn note_port_info(
        &self,
        main: &MainThread,
        index: u32,
        is_input: bool,
    ) -> Option<NotePortInfo> {
        let info = self.plugin.note_port_info(main, index, is_input)?;

        let preferred_dialect = match info.preferred_dialect {
            CLAP_NOTE_DIALECT_CLAP => Some(NoteDialect::Clap),
            CLAP_NOTE_DIALECT_MIDI => Some(NoteDialect::Midi),
            CLAP_NOTE_DIALECT_MIDI_MPE => Some(NoteDialect::MidiMpe),
            CLAP_NOTE_DIALECT_MIDI2 => Some(NoteDialect::Midi2),
            _ => None,
        };

        Some(NotePortInfo {
            id: info.id,
            name: chars_to_string(&info.name),
            supported_dialects: NoteDialects::from_bits_truncate(info.supported_dialects),
            preferred_dialect,
        })
    }

    /// Number of predefined port configurations
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG`) the plugin offers.
    pub fn audio_port_config_count(&self, main: &MainThread) -> u32 {
        self.plugin.audio_ports_config_count(main)
    }

    /// Describes the audio port configuration at `index`.
    pub fn audio_port_config(&self, main: &MainThread, index: u32) -> Option<AudioPortConfig> {
        let config = self.plugin.audio_ports_config(main, index)?;

        Some(AudioPortConfig {
            id: config.id,
            name: chars_to_string(&config.name),
            input_port_count: config.input_port_count,
            output_port_count: config.output_port_count,
            has_main_input: config.has_main_input,
            main_input_channel_count: config.main_input_channel_count,
            has_main_output: config.has_main_output,
            main_output_channel_count: config.main_output_channel_count,
        })
    }

    /// Asks the plugin to switch to a previously reported port configuration.
    /// Returns whether the plugin accepted the request.
    pub fn select_audio_port_config(&mut self, main: &MainThread, config_id: u32) -> bool {
        self.plugin
            .select_audio_ports_config(main, config_id)
            .unwrap_or(false)
    }

    /// The `id` of the audio-ports configuration currently active
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG_INFO`). Returns `None` when the plugin
    /// does not implement the extension.
    ///
    /// This is the companion to [`audio_port_config_count`](Self::audio_port_config_count) /
    /// [`audio_port_config`](Self::audio_port_config): those enumerate
    /// the *available* configs; this reports which one is live and lets you
    /// read its full per-port info via
    /// [`audio_port_config_port_info`](Self::audio_port_config_port_info).
    pub fn current_audio_port_config(&self, main: &MainThread) -> Option<u32> {
        self.plugin.current_audio_ports_config(main)
    }

    /// Full [`AudioPortInfo`] for a single port *within a specific config*
    /// (`CLAP_EXT_AUDIO_PORTS_CONFIG_INFO`). Unlike
    /// [`audio_port_config`](Self::audio_port_config) (which returns
    /// only the config summary), this exposes each port's id / name / channel
    /// count / flags / type for the named `config_id`, without first having to
    /// switch to it. Returns `None` when unsupported or the plugin rejects the
    /// `(config_id, port_index, is_input)` triple.
    pub fn audio_port_config_port_info(
        &self,
        main: &MainThread,
        config_id: u32,
        port_index: u32,
        is_input: bool,
    ) -> Option<AudioPortInfo> {
        self.plugin
            .audio_ports_config_port_info(main, config_id, port_index, is_input)
            .and_then(|info| audio_port_info_from_ffi(&info))
    }

    /// Plugin-reported processing latency in samples. 0 when unsupported.
    pub fn latency_samples(&self, main: &MainThread) -> u32 {
        self.plugin.latency(main)
    }

    /// Plugin-reported tail length in samples (audio continues after input
    /// stops — reverbs, delays), or `None` when the plugin does not implement
    /// `clap.tail`: no extension means the tail is unknown, which is not the
    /// same answer as `Some(0)`, which means no tail. Raw: any value at or
    /// above `INT32_MAX` means an infinite tail (`clap/ext/tail.h`), which
    /// `Tail::from_clap_samples` decodes.
    pub fn tail_samples(&self, main: &MainThread) -> Option<u32> {
        self.plugin.tail(main)
    }

    /// Switches between real-time (`false`) and offline (`true`) rendering
    /// modes per `CLAP_EXT_RENDER`. Returns whether the plugin accepted.
    pub fn set_render_mode(&mut self, main: &MainThread, offline: bool) -> bool {
        self.plugin.set_render_offline(main, offline)
    }

    /// Whether the plugin requires a hard real-time environment (e.g. it
    /// talks to hardware). Hosts should avoid running such plugins offline.
    #[must_use]
    pub fn has_hard_realtime_requirement(&self, main: &MainThread) -> bool {
        self.plugin.has_hard_realtime_requirement(main)
    }

    /// Voice-allocation information from `CLAP_EXT_VOICE_INFO`.
    pub fn voice_info(&self, main: &MainThread) -> Option<VoiceInfo> {
        let info = self.plugin.voice_info(main)?;
        Some(VoiceInfo {
            voice_count: info.voice_count,
            voice_capacity: info.voice_capacity,
            supports_overlapping_notes: (info.flags & CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES)
                != 0,
        })
    }

    /// Number of custom note names (e.g. drum-kit labels) the plugin
    /// exposes via `CLAP_EXT_NOTE_NAME`.
    pub fn note_name_count(&self, main: &MainThread) -> u32 {
        self.plugin.note_name_count(main)
    }

    /// Retrieves a single custom note name.
    pub fn note_name(&self, main: &MainThread, index: u32) -> Option<NoteName> {
        let info = self.plugin.note_name(main, index)?;
        Some(NoteName {
            name: chars_to_string(&info.name),
            port: info.port,
            channel: info.channel,
            key: info.key,
        })
    }

    /// Asks the plugin whether it could apply a set of port-configuration
    /// requests without actually applying them. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn can_apply_audio_port_configuration(
        &self,
        main: &MainThread,
        requests: &[AudioPortConfigRequest],
    ) -> bool {
        self.plugin
            .can_apply_audio_port_configuration(main, &port_config_requests(requests))
            .unwrap_or(false)
    }

    /// Applies a set of port-configuration requests via
    /// `CLAP_EXT_CONFIGURABLE_AUDIO_PORTS`. Returns success. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn apply_audio_port_configuration(
        &mut self,
        main: &MainThread,
        requests: &[AudioPortConfigRequest],
    ) -> bool {
        self.plugin
            .apply_audio_port_configuration(main, &port_config_requests(requests))
            .unwrap_or(false)
    }

    /// Whether the plugin supports activating/deactivating ports while
    /// processing is running. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn can_activate_audio_port_while_processing(&self, main: &MainThread) -> bool {
        self.plugin.can_activate_audio_port_while_processing(main)
    }

    /// Activates or deactivates a single audio port.
    /// `sample_size` is the bit depth (32 or 64). Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn set_audio_port_active(
        &mut self,
        main: &MainThread,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        sample_size: u32,
    ) -> bool {
        self.plugin
            .set_audio_port_active(main, is_input, port_index, is_active, sample_size)
            .unwrap_or(false)
    }

    /// Asks the plugin to add a new port via the draft
    /// `CLAP_EXT_EXTENSIBLE_AUDIO_PORTS`. Returns whether the plugin added it.
    /// Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn add_audio_port(
        &mut self,
        main: &MainThread,
        is_input: bool,
        channel_count: u32,
        port_type: Option<&str>,
    ) -> bool {
        self.plugin
            .add_audio_port(main, is_input, channel_count, port_type)
            .unwrap_or(false)
    }

    /// Counterpart to [`Self::add_audio_port`]. Returns whether the plugin
    /// removed the port. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn remove_audio_port(&mut self, main: &MainThread, is_input: bool, index: u32) -> bool {
        self.plugin
            .remove_audio_port(main, is_input, index)
            .unwrap_or(false)
    }

    /// Asks whether the plugin can process the given ambisonic ordering +
    /// normalization. Returns false when `CLAP_EXT_AMBISONIC` is unsupported.
    #[must_use]
    pub fn is_ambisonic_config_supported(
        &self,
        main: &MainThread,
        config: &AmbisonicConfig,
    ) -> bool {
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
        self.plugin
            .is_ambisonic_config_supported(main, &clap_config)
    }

    /// Retrieves the ambisonic config currently used on a given port.
    pub fn ambisonic_config(
        &self,
        main: &MainThread,
        is_input: bool,
        port_index: u32,
    ) -> Option<AmbisonicConfig> {
        let config = self.plugin.ambisonic_config(main, is_input, port_index)?;
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

    /// Asks whether the plugin can process the given
    /// [`SurroundChannel`]-bit channel mask (`CLAP_EXT_SURROUND`).
    #[must_use]
    pub fn is_surround_channel_mask_supported(&self, main: &MainThread, channel_mask: u64) -> bool {
        self.plugin
            .is_surround_channel_mask_supported(main, channel_mask)
    }

    /// Retrieves the channel-to-speaker mapping the plugin uses on a port.
    ///
    /// The result is indexed **by channel**: element `i` is the speaker fed by
    /// channel `i`, so it always has `count` elements. A position this crate
    /// cannot name arrives as [`SurroundChannel::Unknown`] rather than being
    /// dropped — omitting it would renumber every channel after it.
    ///
    /// `None` means the plugin cannot answer — no `clap.surround` extension or
    /// no `get_channel_map`. An empty `Vec` means it answered with no channels:
    /// `get_channel_map` returns "the number of elements stored", so `0` is
    /// data, not failure. Only the capacity overrun is genuinely broken — the
    /// plugin claims to have written past the 64 elements it was given, so
    /// nothing in the buffer can be trusted.
    pub fn surround_channel_map(
        &self,
        main: &MainThread,
        is_input: bool,
        port_index: u32,
    ) -> Option<Vec<SurroundChannel>> {
        let (map, count) = self
            .plugin
            .surround_channel_map(main, is_input, port_index)?;
        decode_surround_channel_map(&map, count)
    }

    /// The channel-to-speaker mapping for a port as a [`ChannelTopology`].
    ///
    /// The same query as
    /// [`surround_channel_map`](Self::surround_channel_map), in the
    /// shared vocabulary the other format hosts also speak — so a caller can
    /// ask "which speaker is channel 3" without knowing it is holding a CLAP
    /// plugin.
    ///
    /// `None` for the same reason: the plugin has no `clap.surround` extension
    /// or no `get_channel_map`, which is distinct from answering with no
    /// channels. Nothing is lost in the conversion — a position this crate
    /// cannot name arrives as [`Speaker::Unknown`](tutti_types::Speaker::Unknown)
    /// and keeps its channel's slot.
    pub fn surround_topology(
        &self,
        main: &MainThread,
        is_input: bool,
        port_index: u32,
    ) -> Option<ChannelTopology> {
        self.surround_channel_map(main, is_input, port_index)
            .map(|map| crate::topology::topology_of(&map))
    }
}

/// Decodes the first `count` entries of a plugin-filled surround channel map.
///
/// Split out of [`ClapLoaded::surround_channel_map`] so the count
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

/// `None` for a port that reports more channels than a layout holds, as for a
/// port the plugin rejects.
fn audio_port_info_from_ffi(info: &tutti_clap_ffi::AudioPortInfo) -> Option<AudioPortInfo> {
    Some(AudioPortInfo {
        id: info.id,
        name: info.name.clone(),
        layout: layout_of_port(info).ok()?,
        flags: AudioPortFlags::from_bits_truncate(info.flags),
        in_place_pair_id: info.in_place_pair,
    })
}

#[cfg(feature = "clap-extras")]
fn port_config_requests(requests: &[AudioPortConfigRequest]) -> Vec<PortConfigRequest> {
    requests
        .iter()
        .map(|r| PortConfigRequest {
            is_input: r.is_input,
            port_index: r.port_index,
            channel_count: r.channel_count,
        })
        .collect()
}

#[cfg(test)]
mod surround_map_tests {
    use super::*;

    /// A plugin reporting **zero** channels is answering, not failing — a
    /// `count == 0 || count > map.len()` guard would fold that in with the error.
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
