//! MPE ingestion as an input-edge **transform**.
//!
//! Per M2-104 (Appendix C.3, §7.4.7), classic MPE — spreading notes across
//! member channels so a channel-wide pitch bend / pressure / CC74 acts *per
//! note* — is a **MIDI-1 ingestion concern**, not a synthesis concern. A
//! MIDI-2-native receiver only handles per-note messages; it "does not need to
//! know that a rotation scheme is used."
//!
//! So [`MpeIngest`] sits at the input edge (in [`MidiInputNode`](crate::MidiInputNode))
//! and rewrites the classic-MPE channel-spread into **native MIDI-2 per-note
//! messages**. A channel pitch bend on a member channel becomes a Per-Note Pitch
//! Bend addressed to the note that channel holds; channel pressure becomes a
//! per-note (Poly) Pressure; CC74 becomes an Assignable Per-Note Controller.
//! Master-channel messages (global) and already-native per-note messages pass
//! through unchanged. Downstream nodes then only ever see native per-note MIDI-2.
//!
//! # Where expression state lives
//!
//! Not here. This transform is stateless about *values* — it holds only the
//! channel→note map needed to address a per-note message, and forgets each note
//! at note-off. Expression state belongs on the synth voices, derived from the
//! native per-note messages this emits, which is where the spec puts it. A
//! transform that also wrote an expression table would be a second owner of a
//! value the voices already hold.
//!
//! # Bend ranges
//!
//! A pitch-bend sensitivity (RPN #00/00) on the master channel sets the
//! master channel's own bend range, and passes through. On a member channel
//! it sets the range of every member channel's bends (MPE, RP-053 §2.4),
//! which this transform has just made per-note bends: so it becomes a
//! per-note bend sensitivity (RPN #00/07, M2-104 §7.4.13) on each of the
//! zone's member channels, the message a per-note receiver
//! ([`PerNoteBendRanges`](tutti_midi_types::PerNoteBendRanges)) follows.
//!
//! An MPE Configuration Message resets the zone it configures to MPE's
//! default ranges (RP-053 / M1-100-UM, "Pitch Bend Sensitivity": a receiver
//! of an MCM sets the master channel to ±2 semitones and every member
//! channel to ±48). The message is absorbed, so the reset goes on in its
//! place: RPN #00/00 = 2 on the master and RPN #00/07 = 48 on each member
//! ([`MpeIngest::zone_defaults`], which a host-set mode change sends too).
//!
//! An input event therefore yields none, one, or (a bend range, an MCM) one
//! per channel of a zone: [`translate`](MpeIngest::translate) returns a
//! [`Translated`], an iterator that holds no allocation.
//!
//! # Real-time safety
//!
//! Runs on the audio thread inside the pre-block phase: no allocation, no
//! locking. The channel→note maps are fixed-size and pre-built at
//! [`MpeIngest::new`], so reconfiguring is off-RT and rebuilds the whole
//! transform.

use tutti_midi_types::midi2::channel_voice2::ChannelVoice2;
use tutti_midi_types::midi2::{Channeled, UmpMessage};
use tutti_midi_types::mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZoneConfig, NoteRotationAllocator, ZoneInfo,
};
use tutti_midi_types::ump::{
    MidiEvent, RPN_BANK_MPE, RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY,
    RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
};
use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup, PitchBendSensitivity};

/// What [`MpeIngest::translate`] makes of one event, in order: nothing, the
/// event or its per-note form, a per-note bend range for each member channel
/// of a zone, or the bend ranges a zone's configuration resets. A fixed-size
/// value: iterating allocates nothing.
#[derive(Clone, Debug)]
pub struct Translated(Out);

#[derive(Clone, Debug)]
enum Out {
    One(Option<MidiEvent>),
    /// Registered controllers in [`RPN_BANK_MPE`], each run in turn.
    Rpns {
        group: MidiGroup,
        frame_offset: u32,
        runs: [Option<RpnRun>; MAX_RPN_RUNS],
    },
}

/// The most RPN runs one translation yields: a dual-zone reset, a master
/// and a member run per zone.
const MAX_RPN_RUNS: usize = 4;

/// RPN #00/`index` carrying `data` on each channel of `channels`.
#[derive(Clone, Debug)]
struct RpnRun {
    channels: core::ops::RangeInclusive<u8>,
    index: u8,
    data: u32,
}

impl Translated {
    /// RPN runs at `event`'s group and offset.
    fn rpns(group: MidiGroup, frame_offset: u32, runs: [Option<RpnRun>; MAX_RPN_RUNS]) -> Self {
        Self(Out::Rpns {
            group,
            frame_offset,
            runs,
        })
    }
}

impl From<Option<MidiEvent>> for Translated {
    fn from(event: Option<MidiEvent>) -> Self {
        Self(Out::One(event))
    }
}

impl Iterator for Translated {
    type Item = MidiEvent;

    fn next(&mut self) -> Option<MidiEvent> {
        match &mut self.0 {
            Out::One(event) => event.take(),
            Out::Rpns {
                group,
                frame_offset,
                runs,
            } => runs.iter_mut().flatten().find_map(|run| {
                let channel = run.channels.next()?;
                Some(
                    MidiEvent::registered_controller(
                        *group,
                        MidiChannel::new(channel),
                        RPN_BANK_MPE,
                        run.index,
                        run.data,
                    )
                    .with_frame_offset(*frame_offset),
                )
            }),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.0 {
            Out::One(event) => usize::from(event.is_some()),
            Out::Rpns { runs, .. } => runs
                .iter()
                .flatten()
                .map(|run| run.channels.clone().count())
                .sum(),
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for Translated {}
impl core::iter::FusedIterator for Translated {}

/// The pitch-bend ranges a configured zone starts at (MPE, RP-053 /
/// M1-100-UM, "Pitch Bend Sensitivity"): a receiver of an MPE Configuration
/// Message sets the master channel to ±2 semitones and every member channel
/// to ±48. As this transform's output carries them: RPN #00/00 = 2 on the
/// master, which passes through as a channel bend range, and RPN #00/07 =
/// 48 on each member, whose bends it makes per-note.
fn zone_default_runs(config: &MpeZoneConfig) -> [RpnRun; 2] {
    let master = config.master_channel;
    [
        RpnRun {
            channels: master..=master,
            index: RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY,
            data: PitchBendSensitivity::from_semitones(2).to_rpn_bits(),
        },
        RpnRun {
            channels: config.member_channel_range(),
            index: RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
            data: PitchBendSensitivity::MPE_DEFAULT.to_rpn_bits(),
        },
    ]
}

/// Rewrites classic-MPE channel-spread into native MIDI-2 per-note messages.
///
/// Construct with the app-configured [`MpeMode`]; feed every inbound (already
/// MIDI-2-normalized) event through [`translate`](Self::translate).
pub struct MpeIngest {
    mode: MpeMode,
    lower_zone_map: Option<MpeChannelVoiceMap>,
    upper_zone_map: Option<MpeChannelVoiceMap>,
    /// Present only in [`MpeMode::SingleChannelRotation`]. Rotation is a *sender*
    /// scheme (M2-104 C.3): a receiver need not rotate. The allocator exists
    /// only to mint a stable per-note identity for same-pitch notes on the one
    /// channel, so downstream voices stay independent. The emitted messages are
    /// already native per-note, so nothing downstream is rotation-aware.
    rotation: Option<NoteRotationAllocator>,
}

impl MpeIngest {
    /// An ingest configured for `mode`, holding no notes.
    ///
    /// Builds the per-zone channel→note maps the mode calls for: one for a
    /// single zone, two for [`MpeMode::DualZone`], none for
    /// [`MpeMode::Disabled`] or [`MpeMode::SingleChannelRotation`] (which
    /// carries a rotation allocator instead).
    pub fn new(mode: MpeMode) -> Self {
        let (lower_zone_map, upper_zone_map) = match &mode {
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => (None, None),
            MpeMode::LowerZone(config) => (Some(MpeChannelVoiceMap::new(*config)), None),
            MpeMode::UpperZone(config) => (None, Some(MpeChannelVoiceMap::new(*config))),
            MpeMode::DualZone { lower, upper } => (
                Some(MpeChannelVoiceMap::new(*lower)),
                Some(MpeChannelVoiceMap::new(*upper)),
            ),
        };
        let rotation =
            matches!(mode, MpeMode::SingleChannelRotation { .. }).then(NoteRotationAllocator::new);

        Self {
            mode,
            lower_zone_map,
            upper_zone_map,
            rotation,
        }
    }

    /// The configured zone layout. Which channels are master and which are
    /// members follows from this, and so does whether an inbound channel message
    /// is folded to per-note or passed through.
    pub fn mode(&self) -> &MpeMode {
        &self.mode
    }

    /// Reconfigure to a new zone layout, **dropping all held-note state**.
    ///
    /// The channel→note maps are keyed by a channel range the new mode may not
    /// share, so they cannot be carried over. Any note held across this call is
    /// forgotten: its note-off arrives on a channel with no mapping and is
    /// passed through rather than folded, so silence the sounding voices
    /// separately. Off-RT — call at wiring time, or via
    /// [`MidiInputControls::set_mpe_mode`](crate::MidiInputControls::set_mpe_mode),
    /// which defers adoption to the input node's next block.
    pub fn set_mode(&mut self, mode: MpeMode) {
        *self = Self::new(mode);
    }

    /// The bend ranges the configured zones start at, as RPNs on `group` at
    /// `frame_offset`: for each zone, RPN #00/00 = ±2 on its master channel
    /// and RPN #00/07 = ±48 on each member channel, what an MPE
    /// Configuration Message resets a receiver to. A host that sets the mode
    /// itself ([`set_mode`](Self::set_mode)) sends these downstream, so a
    /// receiver that followed the old zones' ranges starts the new ones at
    /// their defaults, as an MCM from the wire would have it. Nothing for
    /// [`MpeMode::Disabled`] or [`MpeMode::SingleChannelRotation`].
    pub fn zone_defaults(&self, group: MidiGroup, frame_offset: u32) -> Translated {
        let mut runs = [None, None, None, None];
        let zones = match &self.mode {
            MpeMode::LowerZone(c) | MpeMode::UpperZone(c) => [Some(c), None],
            MpeMode::DualZone { lower, upper } => [Some(lower), Some(upper)],
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => [None, None],
        };
        for (slot, zone) in runs.chunks_mut(2).zip(zones) {
            if let Some(config) = zone {
                let [a, b] = zone_default_runs(config);
                slot[0] = Some(a);
                slot[1] = Some(b);
            }
        }
        Translated::rpns(group, frame_offset, runs)
    }

    /// Rewrite one already-normalized (MIDI-2) event.
    ///
    /// Yields the events to forward downstream, in order:
    /// - a member-channel bend/pressure/CC74 becomes its native per-note form;
    /// - a member-channel bend sensitivity (RPN #00/00) becomes a per-note
    ///   bend sensitivity (RPN #00/07) on each of the zone's member channels;
    /// - an MPE Configuration Message is absorbed, and the zone it configures
    ///   is reset to its default ranges ([`zone_defaults`](Self::zone_defaults));
    /// - note-on/off and master-channel messages pass through (with the channel
    ///   map updated for note-on/off so later channel messages resolve);
    /// - already-native per-note messages pass through unchanged.
    ///
    /// Nothing means "drop this event" (e.g. a member-channel message with no
    /// note currently held on that channel).
    pub fn translate(&mut self, event: &MidiEvent) -> Translated {
        self.translate_one(event).unwrap_or_else(|| None.into())
    }

    /// [`translate`](Self::translate), with `None` for nothing.
    fn translate_one(&mut self, event: &MidiEvent) -> Option<Translated> {
        // An MPE Configuration Message (RPN 0x00/0x06, M2-104 §7.4.7) reconfigures
        // the zone from the wire — handled in *any* mode, including Disabled (a
        // controller declaring its zone should enable MPE). The message itself
        // is absorbed (configuration, not a musical event); what it resets
        // goes on: the configured zone's default bend ranges.
        //
        // `from_mcm` requires the master channel be Ch1 or Ch16 per RP-053, so
        // RPN 0x00/0x06 on any other channel is ordinary parameter traffic and
        // passes through rather than being swallowed as configuration.
        if let Some((master, members)) = MpeZoneConfig::from_mcm(event) {
            self.reconfigure_from_mcm(master, members);
            let configured = match (&self.mode, master) {
                (MpeMode::LowerZone(c) | MpeMode::DualZone { lower: c, .. }, 0)
                | (MpeMode::UpperZone(c) | MpeMode::DualZone { upper: c, .. }, 15) => c,
                // A zone disabled: no ranges to reset.
                _ => return None,
            };
            let [a, b] = zone_default_runs(configured);
            return Some(Translated::rpns(
                event.group(),
                event.frame_offset,
                [Some(a), Some(b), None, None],
            ));
        }

        if matches!(self.mode, MpeMode::Disabled) {
            return Some(Some(*event).into());
        }

        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(event.data_words()) else {
            // Not a channel-voice-2 message (System, SysEx, …) — pass through.
            return Some(Some(*event).into());
        };

        if let MpeMode::SingleChannelRotation { channel } = self.mode {
            return Some(self.translate_rotation(channel, event, cv2).into());
        }
        if let Some(members) = self.member_bend_range(event, &cv2) {
            return Some(members);
        }
        Some(self.translate_zoned(event, cv2).into())
    }

    /// A bend sensitivity (RPN #00/00) on a member channel, as a per-note
    /// bend sensitivity (RPN #00/07) on each of its zone's member channels;
    /// `None` for anything else (the master channel's passes through).
    fn member_bend_range(
        &self,
        event: &MidiEvent,
        cv2: &ChannelVoice2<&[u32]>,
    ) -> Option<Translated> {
        let ChannelVoice2::RegisteredController(m) = cv2 else {
            return None;
        };
        if u8::from(m.bank()) != RPN_BANK_MPE
            || u8::from(m.index()) != RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY
        {
            return None;
        }
        let zone = self.get_zone_info(u8::from(m.channel()))?;
        if !zone.is_member {
            return None;
        }
        let config = match (&self.mode, zone.is_lower_zone) {
            (MpeMode::LowerZone(c) | MpeMode::DualZone { lower: c, .. }, true)
            | (MpeMode::UpperZone(c) | MpeMode::DualZone { upper: c, .. }, false) => c,
            _ => return None,
        };
        let run = RpnRun {
            channels: config.member_channel_range(),
            index: RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
            data: m.controller_data(),
        };
        Some(Translated::rpns(
            event.group(),
            event.frame_offset,
            [Some(run), None, None, None],
        ))
    }

    /// Apply an MPE Configuration Message: a master channel of 0 selects the lower
    /// zone, 15 the upper zone (RP-053 / M2-104). `members == 0` disables that
    /// zone. Existing zones on the *other* side are preserved (dual-zone setups
    /// configure each zone with its own MCM).
    fn reconfigure_from_mcm(&mut self, master: u8, members: u8) {
        // Snapshot the current per-side configs so one MCM only touches its side.
        let (mut lower, mut upper) = match &self.mode {
            MpeMode::LowerZone(l) => (Some(*l), None),
            MpeMode::UpperZone(u) => (None, Some(*u)),
            MpeMode::DualZone { lower, upper } => (Some(*lower), Some(*upper)),
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => (None, None),
        };

        // Master channel 0 = lower zone, 15 = upper zone (the spec's fixed master
        // assignment). `members == 0` disables that side.
        if master == 0 {
            lower = (members > 0).then(|| MpeZoneConfig::lower(members));
        } else if master == 15 {
            upper = (members > 0).then(|| MpeZoneConfig::upper(members));
        }

        let mode = match (lower, upper) {
            (Some(l), Some(u)) => MpeMode::DualZone { lower: l, upper: u },
            (Some(l), None) => MpeMode::LowerZone(l),
            (None, Some(u)) => MpeMode::UpperZone(u),
            (None, None) => MpeMode::Disabled,
        };
        // Rebuild the voice maps for the new mode (also clears stale bindings).
        *self = Self::new(mode);
    }

    /// Zone (lower/upper/dual) mode: member-channel channel-messages fold onto the
    /// note that member channel holds; master-channel and native per-note pass.
    fn translate_zoned(
        &mut self,
        event: &MidiEvent,
        cv2: ChannelVoice2<&[u32]>,
    ) -> Option<MidiEvent> {
        match cv2 {
            ChannelVoice2::NoteOn(m) => {
                let (ch, note, vel) = (
                    u8::from(m.channel()),
                    u8::from(m.note_number()),
                    m.velocity(),
                );
                let zone = self.get_zone_info(ch)?;
                if vel > 0 {
                    if zone.is_member {
                        if let Some(map) = self.voice_map_mut(zone.is_lower_zone) {
                            map.bind_channel(ch, note);
                        }
                    }
                } else if zone.is_member {
                    if let Some(map) = self.voice_map_mut(zone.is_lower_zone) {
                        map.unbind_channel(ch, note);
                    }
                }
                // The note itself always flows through unchanged.
                Some(*event)
            }
            ChannelVoice2::NoteOff(m) => {
                let (ch, note) = (u8::from(m.channel()), u8::from(m.note_number()));
                if let Some(zone) = self.get_zone_info(ch) {
                    if zone.is_member {
                        if let Some(map) = self.voice_map_mut(zone.is_lower_zone) {
                            map.unbind_channel(ch, note);
                        }
                    }
                }
                Some(*event)
            }
            ChannelVoice2::ChannelPitchBend(m) => {
                let ch = u8::from(m.channel());
                let zone = self.get_zone_info(ch)?;
                if zone.is_master {
                    // Master-channel bend is global — spec keeps it channel-wide.
                    Some(*event)
                } else if zone.is_member {
                    let note = self.held_note(ch, zone.is_lower_zone)?;
                    Some(
                        MidiEvent::per_note_pitch_bend(
                            MidiGroup::FIRST,
                            MidiChannel::new(ch),
                            note,
                            m.pitch_bend_data(),
                        )
                        .with_frame_offset(event.frame_offset),
                    )
                } else {
                    Some(*event)
                }
            }
            ChannelVoice2::ChannelPressure(m) => {
                let ch = u8::from(m.channel());
                let zone = self.get_zone_info(ch)?;
                if zone.is_master {
                    Some(*event)
                } else if zone.is_member {
                    let note = self.held_note(ch, zone.is_lower_zone)?;
                    Some(
                        MidiEvent::poly_pressure(
                            MidiGroup::FIRST,
                            MidiChannel::new(ch),
                            note,
                            m.channel_pressure_data(),
                        )
                        .with_frame_offset(event.frame_offset),
                    )
                } else {
                    Some(*event)
                }
            }
            ChannelVoice2::ControlChange(m)
                if CCNumber::new(u8::from(m.control())) == tutti_midi_types::cc::BRIGHTNESS =>
            {
                let ch = u8::from(m.channel());
                let zone = self.get_zone_info(ch)?;
                if zone.is_member {
                    let note = self.held_note(ch, zone.is_lower_zone)?;
                    // CC74 → Assignable Per-Note Controller index 74 (the MPE slide).
                    Some(
                        MidiEvent::per_note_controller(
                            MidiGroup::FIRST,
                            MidiChannel::new(ch),
                            note,
                            74,
                            m.control_change_data(),
                            false,
                        )
                        .with_frame_offset(event.frame_offset),
                    )
                } else {
                    Some(*event)
                }
            }
            // Everything else (native per-note messages, other CCs, program
            // change, …) passes through unchanged.
            _ => Some(*event),
        }
    }

    /// Note Number Rotation mode: mint/resolve a stable per-note id per note
    /// number, so same-pitch notes on the one channel stay independent. Emits
    /// native per-note messages addressed by the rotated note number.
    fn translate_rotation(
        &mut self,
        channel: u8,
        event: &MidiEvent,
        cv2: ChannelVoice2<&[u32]>,
    ) -> Option<MidiEvent> {
        // Messages off the rotation channel are ignored (this mode owns one).
        if u8::from(cv2.channel()) != channel {
            return None;
        }
        let rotation = self.rotation.as_mut()?;
        match cv2 {
            ChannelVoice2::NoteOn(m) if m.velocity() > 0 => {
                rotation.note_on(u8::from(m.note_number()));
                Some(*event)
            }
            ChannelVoice2::NoteOn(m) => {
                rotation.note_off(u8::from(m.note_number()));
                Some(*event)
            }
            ChannelVoice2::NoteOff(m) => {
                rotation.note_off(u8::from(m.note_number()));
                Some(*event)
            }
            // Native per-note (and everything else) on the rotation channel passes
            // through — it already carries the note number the receiver keys on.
            _ => Some(*event),
        }
    }

    fn held_note(&self, channel: u8, is_lower_zone: bool) -> Option<u8> {
        self.voice_map(is_lower_zone)
            .as_ref()
            .and_then(|map| map.get_note_for_channel(channel))
    }

    fn get_zone_info(&self, channel: u8) -> Option<ZoneInfo> {
        match &self.mode {
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => None,
            MpeMode::LowerZone(config) => config.handles_channel(channel).then(|| ZoneInfo {
                is_master: config.is_master_channel(channel),
                is_member: config.is_member_channel(channel),
                is_lower_zone: true,
            }),
            MpeMode::UpperZone(config) => config.handles_channel(channel).then(|| ZoneInfo {
                is_master: config.is_master_channel(channel),
                is_member: config.is_member_channel(channel),
                is_lower_zone: false,
            }),
            MpeMode::DualZone { lower, upper } => {
                if lower.handles_channel(channel) {
                    Some(ZoneInfo {
                        is_master: lower.is_master_channel(channel),
                        is_member: lower.is_member_channel(channel),
                        is_lower_zone: true,
                    })
                } else if upper.handles_channel(channel) {
                    Some(ZoneInfo {
                        is_master: upper.is_master_channel(channel),
                        is_member: upper.is_member_channel(channel),
                        is_lower_zone: false,
                    })
                } else {
                    None
                }
            }
        }
    }

    fn voice_map(&self, is_lower_zone: bool) -> &Option<MpeChannelVoiceMap> {
        if is_lower_zone {
            &self.lower_zone_map
        } else {
            &self.upper_zone_map
        }
    }

    fn voice_map_mut(&mut self, is_lower_zone: bool) -> Option<&mut MpeChannelVoiceMap> {
        if is_lower_zone {
            self.lower_zone_map.as_mut()
        } else {
            self.upper_zone_map.as_mut()
        }
    }

    /// Forget every held note and every rotation assignment, leaving the zone
    /// configuration intact.
    ///
    /// The counterpart to [`set_mode`](Self::set_mode) for a panic / all-notes-off:
    /// the mapping tables are cleared, so a subsequent member-channel message
    /// with no matching note-on is dropped rather than addressed at a stale note.
    pub fn reset(&mut self) {
        if let Some(map) = &mut self.lower_zone_map {
            map.clear();
        }
        if let Some(map) = &mut self.upper_zone_map {
            map.clear();
        }
        if let Some(rotation) = &mut self.rotation {
            rotation.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };
    use tutti_midi_types::mpe::MpeZoneConfig;

    fn note_on(channel: u8, note: u8, vel_u7: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            note,
            midi1_velocity_to_midi2(vel_u7),
        )
    }
    fn pitch_bend14(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_pitch_bend_to_midi2(bend14),
        )
    }
    fn cc(channel: u8, cc_num: CCNumber, value_u7: u8) -> MidiEvent {
        MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            cc_num,
            midi1_cc_to_midi2(value_u7),
        )
    }

    /// True if the event is the given native CV2 kind — matched inline so the
    /// borrow of `data_words()` never escapes.
    macro_rules! assert_cv2 {
        ($ev:expr, $pat:pat $(if $guard:expr)?) => {
            match UmpMessage::try_from($ev.data_words()).unwrap() {
                UmpMessage::ChannelVoice2($pat) $(if $guard)? => {}
                other => panic!("unexpected message: {other:?}"),
            }
        };
    }

    #[test]
    fn member_channel_bend_becomes_per_note_pitch_bend() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        // Note held on member channel 2.
        assert!(ingest
            .translate(&note_on(2, 60, 100))
            .next()
            .unwrap()
            .is_note_on());
        // Channel bend on ch2 → per-note bend on note 60.
        let out = ingest
            .translate(&pitch_bend14(2, 16383))
            .next()
            .expect("emits");
        assert_cv2!(
            out,
            ChannelVoice2::PerNotePitchBend(m)
                if u8::from(m.channel()) == 2 && u8::from(m.note_number()) == 60
        );
    }

    #[test]
    fn master_channel_bend_passes_through_as_channel_bend() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        // Master channel of a lower zone is channel 0.
        let out = ingest
            .translate(&pitch_bend14(0, 12288))
            .next()
            .expect("passes through");
        assert_cv2!(out, ChannelVoice2::ChannelPitchBend(_));
    }

    #[test]
    fn member_channel_cc74_becomes_per_note_controller() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        ingest.translate(&note_on(3, 64, 100));
        let out = ingest
            .translate(&cc(3, CCNumber::BRIGHTNESS, 127))
            .next()
            .expect("emits");
        assert_cv2!(
            out,
            ChannelVoice2::AssignablePerNoteController(m)
                if u8::from(m.note_number()) == 64 && m.index() == 74
        );
    }

    #[test]
    fn member_channel_pressure_becomes_per_note_pressure() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        ingest.translate(&note_on(2, 60, 100));
        let out = ingest
            .translate(&MidiEvent::channel_pressure(
                MidiGroup::FIRST,
                MidiChannel::new(2),
                0xFFFF_FFFF,
            ))
            .next()
            .expect("emits");
        assert_cv2!(out, ChannelVoice2::KeyPressure(m) if u8::from(m.note_number()) == 60);
    }

    #[test]
    fn bend_with_no_held_note_is_dropped() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        // No note on ch2 yet → member-channel bend has nothing to address.
        assert!(ingest.translate(&pitch_bend14(2, 16383)).next().is_none());
    }

    #[test]
    fn native_per_note_passes_through() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let native =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF_FFFF);
        let out = ingest.translate(&native).next().expect("passes");
        assert_cv2!(out, ChannelVoice2::PerNotePitchBend(_));
    }

    #[test]
    fn disabled_mode_passes_everything_through() {
        let mut ingest = MpeIngest::new(MpeMode::Disabled);
        let out = ingest
            .translate(&pitch_bend14(2, 16383))
            .next()
            .expect("passes");
        assert_cv2!(out, ChannelVoice2::ChannelPitchBend(_));
    }

    #[test]
    fn rotation_ignores_off_channel() {
        let mut ingest = MpeIngest::new(MpeMode::SingleChannelRotation { channel: 5 });
        // On-channel note passes.
        assert!(ingest.translate(&note_on(5, 60, 100)).next().is_some());
        // Off-channel note is ignored (this mode owns channel 5).
        assert!(ingest.translate(&note_on(7, 64, 100)).next().is_none());
    }

    #[test]
    fn mcm_from_the_wire_configures_the_lower_zone() {
        // Start disabled; an MCM on the master channel (0) declaring 7 members
        // must enable the lower zone from the wire (M2-104 §7.4.7).
        let mut ingest = MpeIngest::new(MpeMode::Disabled);
        let mcm = MpeZoneConfig::lower(7).to_mcm();

        // The MCM itself is absorbed (configuration, not a musical event);
        // what goes on is the zone's reset ranges, all registered controllers
        // (`an_mcm_resets_its_zones_bend_ranges`).
        let reset: Vec<MidiEvent> = ingest.translate(&mcm).collect();
        assert_eq!(reset.len(), 8, "master and seven members");
        assert!(!reset.contains(&mcm));
        assert!(matches!(ingest.mode(), MpeMode::LowerZone(_)));

        // Now a member-channel bend folds to per-note, proving the zone is live.
        ingest.translate(&note_on(1, 60, 100));
        let out = ingest
            .translate(&pitch_bend14(1, 16383))
            .next()
            .expect("emits");
        assert_cv2!(out, ChannelVoice2::PerNotePitchBend(m) if u8::from(m.note_number()) == 60);
    }

    #[test]
    fn mcm_with_zero_members_disables_the_zone() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(7)));
        // A raw MCM on master channel 0 with member_count == 0 disables the lower
        // zone. (`MpeZoneConfig::lower` clamps to >= 1, so the disabling value can
        // only come off the wire — build it directly.)
        let disable = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::FIRST, // lower-zone master channel
            tutti_midi_types::ump::RPN_BANK_MPE,
            tutti_midi_types::ump::RPN_INDEX_MCM,
            0, // 0 members → disable
        );
        assert!(ingest.translate(&disable).next().is_none());
        assert!(matches!(ingest.mode(), MpeMode::Disabled));
    }

    /// What a receiver following RPN #00/00 (channel bend range) and
    /// #00/07 (per-note bend range) per channel ends with after `events`:
    /// `(channel, index) → semitones`, in order of arrival.
    fn ranges_after(events: impl IntoIterator<Item = MidiEvent>) -> Vec<(u8, u8, f32)> {
        events
            .into_iter()
            .map(|ev| {
                let Ok(UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m))) =
                    UmpMessage::try_from(ev.data_words())
                else {
                    panic!("{ev:?} is not a registered controller");
                };
                assert_eq!(u8::from(m.bank()), RPN_BANK_MPE);
                let range = PitchBendSensitivity::from_rpn_bits(m.controller_data());
                (
                    u8::from(m.channel()),
                    u8::from(m.index()),
                    range.as_semitones_f32(),
                )
            })
            .collect()
    }

    /// **An MPE Configuration Message resets its zone's bend ranges** (MPE,
    /// RP-053 / M1-100-UM, "Pitch Bend Sensitivity": the master channel to
    /// ±2, every member to ±48): an upper-zone MCM of three members yields
    /// RPN #00/00 = 2 on channel 15 and RPN #00/07 = 48 on 12, 13 and 14, at
    /// the message's offset, and a per-note receiver that had been told ±12
    /// on those members is back at ±48. The other zone of a dual setup keeps
    /// its ranges (nothing is sent for it), and a disabling MCM sends nothing.
    /// A host-set mode sends the same through `zone_defaults`, for each zone.
    ///
    /// Mutation (run): the MCM arm returns `None` again (absorbed, nothing
    /// sent) → the receiver keeps ±12 → fails.
    /// Mutation (run): `zone_default_runs` emits #00/00 on the members → the
    /// receiver keeps ±12 → fails.
    /// Mutation (run): the MCM resets every zone of the new mode
    /// (`zone_defaults` in place of the configured side's runs) → the lower
    /// zone's ranges are sent too → fails.
    #[test]
    fn an_mcm_resets_its_zones_bend_ranges() {
        use tutti_midi_types::PerNoteBendRanges;
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(3)));
        let mut receiver = PerNoteBendRanges::default();
        for ev in ingest.translate(&bend_sensitivity(1, 12)) {
            receiver.observe(&ev);
        }
        let mcm = MpeZoneConfig::upper(3).to_mcm().with_frame_offset(9);
        let reset: Vec<MidiEvent> = ingest.translate(&mcm).collect();
        assert!(matches!(ingest.mode(), MpeMode::DualZone { .. }));
        assert!(reset.iter().all(|ev| ev.frame_offset == 9));
        assert_eq!(
            ranges_after(reset.iter().copied()),
            vec![(15, 0, 2.0), (12, 7, 48.0), (13, 7, 48.0), (14, 7, 48.0)]
        );
        // An upper member set to ±12, then the upper MCM again: the upper
        // members are back at ±48, the lower zone's ±12 stands.
        let mcm = MpeZoneConfig::upper(3).to_mcm();
        for ev in ingest.translate(&bend_sensitivity(13, 12)) {
            receiver.observe(&ev);
        }
        assert_eq!(receiver.semitones(13), 12.0);
        for ev in ingest.translate(&mcm) {
            receiver.observe(&ev);
        }
        for channel in 12..=14 {
            assert_eq!(receiver.semitones(channel), 48.0, "channel {channel}");
        }
        assert_eq!(
            receiver.semitones(1),
            12.0,
            "the lower zone keeps its range"
        );

        let disable = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(15),
            RPN_BANK_MPE,
            tutti_midi_types::ump::RPN_INDEX_MCM,
            0,
        );
        assert_eq!(ingest.translate(&disable).len(), 0);

        // A host-set dual-zone mode: each zone's ranges, lower first.
        ingest.set_mode(MpeMode::DualZone {
            lower: MpeZoneConfig::lower(2),
            upper: MpeZoneConfig::upper(1),
        });
        let defaults = ingest.zone_defaults(MidiGroup::FIRST, 0);
        assert_eq!(defaults.len(), 5);
        assert_eq!(
            ranges_after(defaults),
            vec![
                (0, 0, 2.0),
                (1, 7, 48.0),
                (2, 7, 48.0),
                (15, 0, 2.0),
                (14, 7, 48.0)
            ]
        );
        ingest.set_mode(MpeMode::Disabled);
        assert_eq!(ingest.zone_defaults(MidiGroup::FIRST, 0).len(), 0);
    }

    fn bend_sensitivity(channel: u8, semitones: u8) -> MidiEvent {
        MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            tutti_midi_types::ump::RPN_BANK_MPE,
            RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY,
            tutti_midi_types::PitchBendSensitivity::from_semitones(semitones).to_rpn_bits(),
        )
        .with_frame_offset(17)
    }

    /// **A member channel's bend range is every member's per-note range; the
    /// master's is its own.** Dual zones of three members each (lower 1..=3,
    /// upper 12..=14): RPN #00/00 on member 2 becomes RPN #00/07 on 1, 2 and
    /// 3, on member 13 on 12, 13 and 14, at the event's offset; on master 0 it
    /// passes through as it came. A per-note receiver reading the output then
    /// bends those channels' notes by the new range and the rest by ±48.
    ///
    /// Mutation (run): the fan-out drops its `is_member` check → the master's
    /// sensitivity fans out too → fails.
    /// Mutation (run): the fan-out emits index #00/00 → the receiver keeps
    /// ±48 → fails.
    /// Mutation (run): the fan-out takes the lower zone's config for both
    /// sides → member 13's fans out over 1..=3 → fails.
    /// Mutation (run): one channel (`ch..=ch`) instead of the zone's range →
    /// fails.
    #[test]
    fn a_member_channels_bend_range_becomes_every_members_per_note_range() {
        use tutti_midi_types::PerNoteBendRanges;
        let mut ingest = MpeIngest::new(MpeMode::DualZone {
            lower: MpeZoneConfig::lower(3),
            upper: MpeZoneConfig::upper(3),
        });
        let mut receiver = PerNoteBendRanges::default();

        let master = bend_sensitivity(0, 2);
        let out: Vec<MidiEvent> = ingest.translate(&master).collect();
        assert_eq!(out, vec![master], "the master's own range passes through");
        assert!(!receiver.observe(&out[0]));

        for (member, semitones, channels) in [(2, 12, 1..=3), (13, 24, 12..=14)] {
            let fanned = ingest.translate(&bend_sensitivity(member, semitones));
            assert_eq!(fanned.len(), 3);
            let mut seen = Vec::new();
            for ev in fanned {
                assert_eq!(ev.frame_offset, 17);
                assert!(receiver.observe(&ev), "{ev:?} is RPN #00/07");
                seen.push(ev.channel().map(MidiChannel::get));
            }
            assert_eq!(seen, channels.map(Some).collect::<Vec<_>>());
        }
        for channel in 0..16u8 {
            let want = match channel {
                1..=3 => 12.0,
                12..=14 => 24.0,
                _ => 48.0,
            };
            assert_eq!(receiver.semitones(channel), want, "channel {channel}");
        }
    }
}
