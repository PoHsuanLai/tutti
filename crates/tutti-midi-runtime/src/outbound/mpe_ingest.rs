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
//! (`tutti_plugin_types::PerNoteBendRanges`) follows.
//!
//! An MPE Configuration Message resets the zone it configures to MPE's
//! default ranges (RP-053 / M1-100-UM, "Pitch Bend Sensitivity": a receiver
//! of an MCM sets the master channel to ±2 semitones and every member
//! channel to ±48). The message is absorbed, so the reset goes on in its
//! place: RPN #00/00 = 2 on the master and RPN #00/07 = 48 on each member
//! ([`MpeIngest::zone_defaults`], which a host-set mode change sends too).
//!
//! An input event therefore yields none, one, or (a bend range, an MCM) one
//! per channel of a zone: [`translate`](MpeIngest::translate) calls its `out`
//! for each, in order, and holds nothing between calls.
//!
//! # Real-time safety
//!
//! Runs on the audio thread inside the pre-block phase: no allocation, no
//! locking. The channel→note maps are fixed-size and pre-built at
//! [`MpeIngest::new`], so reconfiguring is off-RT and rebuilds the whole
//! transform.

use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_midi_protocol::{MpeMode, MpeZone, MpeZoneConfig, MpeZones, PitchBendSensitivity};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, Rpn, Ump, Voice2, decode};

/// The Registered Controller `rpn` carrying `data` on each channel of `channels`, on `group`
/// at `offset`.
fn send_rpns(
    group: MidiGroup,
    offset: Offset,
    channels: impl Iterator<Item = MidiChannel>,
    rpn: Rpn,
    data: u32,
    out: &mut impl FnMut(UmpAt),
) {
    for channel in channels {
        out((
            offset,
            Message::Midi2(
                Address { group, channel },
                Voice2::Registered {
                    bank: rpn.bank(),
                    index: rpn.index(),
                    value: data,
                },
            )
            .into(),
        ));
    }
}

/// The pitch-bend ranges a configured zone starts at (MPE, RP-053 /
/// M1-100-UM, "Pitch Bend Sensitivity"): a receiver of an MPE Configuration
/// Message sets the master channel to ±2 semitones and every member channel
/// to ±48. As this transform's output carries them: RPN #00/00 = 2 on the
/// master, which passes through as a channel bend range, and RPN #00/07 =
/// 48 on each member, whose bends it makes per-note.
fn send_zone_defaults(
    config: &MpeZoneConfig,
    group: MidiGroup,
    offset: Offset,
    out: &mut impl FnMut(UmpAt),
) {
    send_rpns(
        group,
        offset,
        core::iter::once(config.master_channel()),
        Rpn::PITCH_BEND_SENSITIVITY,
        PitchBendSensitivity::of(2).to_rpn_bits(),
        out,
    );
    send_rpns(
        group,
        offset,
        config.member_channels(),
        Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
        PitchBendSensitivity::MPE_DEFAULT.to_rpn_bits(),
        out,
    );
}

/// Rewrites classic-MPE channel-spread into native MIDI-2 per-note messages.
///
/// Construct with the app-configured [`MpeMode`]; feed every inbound (already
/// MIDI-2-normalized) event through [`translate`](Self::translate).
#[derive(Debug)]
pub struct MpeIngest {
    mode: MpeMode,
    /// The note each member channel holds. The two zones share no channel
    /// ([`MpeZones`]), so one table serves both.
    held: held::HeldNotes,
}

/// What a channel is to the configured zones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The zone's master: zone-wide messages.
    Master,
    /// A member: one note's expression.
    Member,
}

impl MpeIngest {
    /// An ingest configured for `mode`, holding no notes.
    pub fn new(mode: MpeMode) -> Self {
        Self {
            mode,
            held: held::HeldNotes::default(),
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
    /// `offset`: for each zone, RPN #00/00 = ±2 on its master channel
    /// and RPN #00/07 = ±48 on each member channel, what an MPE
    /// Configuration Message resets a receiver to. A host that sets the mode
    /// itself ([`set_mode`](Self::set_mode)) sends these downstream, so a
    /// receiver that followed the old zones' ranges starts the new ones at
    /// their defaults, as an MCM from the wire would have it. Nothing for
    /// [`MpeMode::Disabled`] or [`MpeMode::SingleChannelRotation`].
    pub fn zone_defaults(&self, group: MidiGroup, offset: Offset, mut out: impl FnMut(UmpAt)) {
        if let MpeMode::Zones(zones) = &self.mode {
            for config in zones.zones() {
                send_zone_defaults(config, group, offset, &mut out);
            }
        }
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
    /// No call means "drop this event" (e.g. a member-channel message with no
    /// note currently held on that channel).
    pub fn translate(&mut self, event: &UmpAt, mut out: impl FnMut(UmpAt)) {
        if let Some(one) = self.translate_one(event, &mut out) {
            out(one);
        }
    }

    /// [`translate`](Self::translate): the one event it becomes, or `None`
    /// when it sent what it becomes through `out` itself (a fan-out) or
    /// dropped it.
    fn translate_one(&mut self, event: &UmpAt, out: &mut impl FnMut(UmpAt)) -> Option<UmpAt> {
        // An MPE Configuration Message (RPN 0x00/0x06, M2-104 §7.4.7) reconfigures
        // the zone from the wire — handled in *any* mode, including Disabled (a
        // controller declaring its zone should enable MPE). The message itself
        // is absorbed (configuration, not a musical event); what it resets
        // goes on: the configured zone's default bend ranges.
        //
        // `from_mcm` requires the master channel be Ch1 or Ch16 per RP-053, so
        // RPN 0x00/0x06 on any other channel is ordinary parameter traffic and
        // passes through rather than being swallowed as configuration.
        let decoded = decode(event.1).ok();
        if let (Some((zone, members)), Some(Message::Midi2(address, _))) =
            (MpeZoneConfig::from_mcm(&event.1), decoded)
        {
            if !self.reconfigure_from_mcm(zone, members) {
                return None;
            }
            // A zone disabled has no ranges to reset.
            if let MpeMode::Zones(zones) = &self.mode
                && let Some(configured) = match zone {
                    MpeZone::Lower => zones.lower(),
                    MpeZone::Upper => zones.upper(),
                }
            {
                send_zone_defaults(configured, address.group, event.0, out);
            }
            return None;
        }

        if matches!(self.mode, MpeMode::Disabled) {
            return Some(*event);
        }

        let Some(Message::Midi2(address, voice)) = decoded else {
            // Not a channel-voice-2 message (System, SysEx, …) — pass through.
            return Some(*event);
        };

        if let MpeMode::SingleChannelRotation { channel } = self.mode {
            // Rotation is a sender's scheme and a receiver need not know it
            // (M2-104 Appendix C.3): this mode owns one channel, so the
            // messages of the others are ignored, and the one's pass as they
            // are, native per-note messages included.
            return (address.channel == channel).then_some(*event);
        }
        if self.member_bend_range(event, address, voice, out) {
            return None;
        }
        self.translate_zoned(event, address, voice)
    }

    /// A bend sensitivity (RPN #00/00) on a member channel, as a per-note
    /// bend sensitivity (RPN #00/07) on each of its zone's member channels,
    /// sent through `out`; `false`, sending nothing, for anything else (the
    /// master channel's passes through).
    fn member_bend_range(
        &self,
        event: &UmpAt,
        address: Address,
        voice: Voice2,
        out: &mut impl FnMut(UmpAt),
    ) -> bool {
        let Voice2::Registered { bank, index, value } = voice else {
            return false;
        };
        if Rpn::new(bank, index) != Rpn::PITCH_BEND_SENSITIVITY {
            return false;
        }
        let MpeMode::Zones(zones) = &self.mode else {
            return false;
        };
        let Some(config) = zones.zone_of(address.channel) else {
            return false;
        };
        if !config.is_member_channel(address.channel) {
            return false;
        }
        send_rpns(
            address.group,
            event.0,
            config.member_channels(),
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            value,
            out,
        );
        true
    }

    /// Apply an MPE Configuration Message: it configures the zone whose master
    /// channel it arrived on (RP-053 / M2-104), `members == 0` disables that
    /// zone. The zone on the *other* side is preserved (a dual-zone setup
    /// configures each zone with its own MCM).
    ///
    /// A message that would leave the two zones sharing a member channel is
    /// ignored, like any other MCM RP-053 does not allow: the zones keep what
    /// they had, and this returns `false`.
    fn reconfigure_from_mcm(&mut self, zone: MpeZone, members: u8) -> bool {
        // Snapshot the current per-side configs so one MCM only touches its side.
        let (mut lower, mut upper) = match &self.mode {
            MpeMode::Zones(zones) => (zones.lower().copied(), zones.upper().copied()),
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => (None, None),
        };
        let configured = MpeZoneConfig::new(zone, members);
        match zone {
            MpeZone::Lower => lower = configured,
            MpeZone::Upper => upper = configured,
        }
        let mode = match MpeZones::new(lower, upper) {
            Some(zones) => MpeMode::Zones(zones),
            // Both zones off: MPE is off.
            None if lower.is_none() && upper.is_none() => MpeMode::Disabled,
            // The two zones would overlap.
            None => return false,
        };
        // Rebuild the held notes for the new mode (also clears stale bindings).
        *self = Self::new(mode);
        true
    }

    /// Zone mode: member-channel channel-messages fold onto the note that
    /// member channel holds; master-channel and native per-note pass.
    fn translate_zoned(&mut self, event: &UmpAt, address: Address, voice: Voice2) -> Option<UmpAt> {
        let ch = address.channel;
        match voice {
            // M2-104 7.4.2: a MIDI 2.0 Note On at velocity 0 is a Note On, and
            // binds its channel like any other (the translator before this
            // turns a MIDI 1.0 one into a Note Off).
            Voice2::NoteOn { note, .. } => {
                if self.role(ch)? == Role::Member {
                    self.held.bind(ch, note);
                }
                // The note itself always flows through unchanged.
                Some(*event)
            }
            Voice2::NoteOff { .. } => {
                if self.role(ch) == Some(Role::Member) {
                    self.held.release(ch);
                }
                Some(*event)
            }
            Voice2::PitchBend(value) => match self.role(ch)? {
                // Master-channel bend is global: the spec keeps it channel-wide.
                Role::Master => Some(*event),
                Role::Member => {
                    let note = self.held.note_on(ch)?;
                    Some((
                        event.0,
                        Ump::from(Message::Midi2(
                            Address::first(ch),
                            Voice2::PerNotePitchBend { note, value },
                        )),
                    ))
                }
            },
            Voice2::ChannelPressure(value) => match self.role(ch)? {
                Role::Master => Some(*event),
                Role::Member => {
                    let note = self.held.note_on(ch)?;
                    Some((
                        event.0,
                        Ump::from(Message::Midi2(
                            Address::first(ch),
                            Voice2::PolyPressure { note, value },
                        )),
                    ))
                }
            },
            Voice2::ControlChange { controller, value } if controller == CCNumber::BRIGHTNESS => {
                if self.role(ch)? == Role::Member {
                    let note = self.held.note_on(ch)?;
                    // CC74 -> Assignable Per-Note Controller index 74 (the MPE slide).
                    Some((
                        event.0,
                        Ump::from(Message::Midi2(
                            Address::first(ch),
                            Voice2::AssignablePerNote {
                                note,
                                controller: CCNumber::BRIGHTNESS.get(),
                                value,
                            },
                        )),
                    ))
                } else {
                    Some(*event)
                }
            }
            // Everything else (native per-note messages, other CCs, program
            // change, ...) passes through unchanged.
            _ => Some(*event),
        }
    }

    /// What `channel` is to the configured zones, or `None` when no zone owns it.
    fn role(&self, channel: MidiChannel) -> Option<Role> {
        let MpeMode::Zones(zones) = &self.mode else {
            return None;
        };
        let config = zones.zone_of(channel)?;
        Some(if config.is_master_channel(channel) {
            Role::Master
        } else {
            Role::Member
        })
    }

    /// Forget every held note, leaving the zone configuration intact.
    ///
    /// The counterpart to [`set_mode`](Self::set_mode) for a panic / all-notes-off:
    /// the mapping table is cleared, so a subsequent member-channel message
    /// with no matching note-on is dropped rather than addressed at a stale note.
    pub fn reset(&mut self) {
        self.held.clear();
    }
}

mod held {
    //! The note each member channel holds.

    use tutti_ump::{MidiChannel, NoteNumber};

    /// Which note each of the sixteen channels is playing, so a per-channel
    /// expression message (pressure arriving on a member channel) becomes a
    /// per-note one for the note that channel holds.
    ///
    /// The controller allocates: it sprays each note onto a channel of its own
    /// before sending (classic MPE), and the host is told the channel and only
    /// records the pairing. Choosing a channel for a note is the sender's job
    /// and nothing here does it.
    #[derive(Debug, Default)]
    pub(super) struct HeldNotes([Option<NoteNumber>; 16]);

    impl HeldNotes {
        /// Records that `channel` is playing `note`.
        pub(super) fn bind(&mut self, channel: MidiChannel, note: NoteNumber) {
            self.0[usize::from(channel.get())] = Some(note);
        }

        /// Frees `channel`.
        pub(super) fn release(&mut self, channel: MidiChannel) {
            self.0[usize::from(channel.get())] = None;
        }

        /// The note `channel` is playing, if any.
        pub(super) fn note_on(&self, channel: MidiChannel) -> Option<NoteNumber> {
            self.0[usize::from(channel.get())]
        }

        /// Frees every channel.
        pub(super) fn clear(&mut self) {
            self.0 = [None; 16];
        }
    }
}

#[cfg(test)]
mod tests;
