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
#[derive(Debug)]
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
                    if zone.is_member
                        && let Some(map) = self.voice_map_mut(zone.is_lower_zone)
                    {
                        map.bind_channel(ch, note);
                    }
                } else if zone.is_member
                    && let Some(map) = self.voice_map_mut(zone.is_lower_zone)
                {
                    map.unbind_channel(ch, note);
                }
                // The note itself always flows through unchanged.
                Some(*event)
            }
            ChannelVoice2::NoteOff(m) => {
                let (ch, note) = (u8::from(m.channel()), u8::from(m.note_number()));
                if let Some(zone) = self.get_zone_info(ch)
                    && zone.is_member
                    && let Some(map) = self.voice_map_mut(zone.is_lower_zone)
                {
                    map.unbind_channel(ch, note);
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
            .and_then(|map| map.note_for_channel(channel))
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
mod tests;
