//! MPE ingestion as an input-edge **transform**.
//!
//! Per M2-104 (Appendix C.3, §7.4.7), classic MPE — spreading notes across
//! member channels so a channel-wide pitch bend / pressure / CC74 acts *per
//! note* — is a **MIDI-1 ingestion concern**, not a synthesis concern. A
//! MIDI-2-native receiver only handles per-note messages; it "does not need to
//! know that a rotation scheme is used."
//!
//! So [`MpeIngest`] sits at the input edge (in [`MidiIngestNode`](crate::MidiIngestNode))
//! and rewrites the classic-MPE channel-spread into **native MIDI-2 per-note
//! messages**. A channel pitch bend on a member channel becomes a Per-Note Pitch
//! Bend addressed to the note that channel holds; channel pressure becomes a
//! per-note (Poly) Pressure; CC74 becomes an Assignable Per-Note Controller.
//! Master-channel messages (global) and already-native per-note messages pass
//! through unchanged. Downstream nodes then only ever see native per-note MIDI-2.
//!
//! # Where expression state lives
//!
//! The voices hold the expression of a *sounding* note, derived from the native
//! per-note messages this emits. What a member channel was told *before* its
//! Note On is another matter: M1-100 §2.2.6 to 2.2.8 have a receiver "continue
//! to track" the channel's Pitch Bend, Channel Pressure and CC 74 "even when no
//! note is playing", and a note's number is not known until its Note On. So the
//! transform keeps the last value of each, per channel, and sends them as
//! per-note messages ahead of the Note On they belong to (which a per-note
//! receiver, `tutti_voicing::PerNoteState`, starts the note with). A manager's
//! pressure and CC 74 are combined into each note of its zone (the greater
//! of the two, `combined`); a manager's bend is the instrument's, summed with
//! each member's (`tutti_voicing::Voicing`). The transform forgets a note at
//! its note-off, and a channel's values when it enters or leaves MPE control.
//!
//! # The receive rules are a value
//!
//! Which message a channel takes (M1-100 Appendix E, Table 5) and what
//! switches a zone's MIDI Mode is `MpeSpec`'s, one match per rule set, so the
//! MIDI-CI MPE Profile (M2-120) is another variant and no branch elsewhere.
//! A message the table prohibits is ignored (§2.3.1: "it should ignore them").
//! A channel in no zone is conventional and passes whole (§2.2.1).
//!
//! # Changing the zones
//!
//! "When a receiver changes its Zone configurations, the receiver shall stop
//! all Sounding Notes and reset all controls to reasonable default values on
//! each Channel entering or leaving MPE control" (§2.2.3): an MCM from the wire
//! sends, for each such channel, All Sound Off and Reset All Controllers before
//! the zone's default ranges, and a host-set mode
//! ([`MpeIngest::set_mode`]) leaves them for
//! [`zone_defaults`](MpeIngest::zone_defaults), which the host sends. A channel
//! that did not change keeps its notes.
//!
//! # Bend ranges
//!
//! A pitch-bend sensitivity (RPN #00/00) on the master channel sets the
//! master channel's own bend range, and passes through. On a member channel
//! it sets the range of every member channel's bends (MPE, RP-053 §2.4),
//! which this transform has just made per-note bends: so it becomes a
//! per-note bend sensitivity (RPN #00/07, M2-104 §7.4.13) on each of the
//! zone's member channels, the message a per-note receiver
//! (`tutti_voicing::PerNoteState`) follows.
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
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, Rpn, Ump, Voice2};

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

/// The receive rules of the MPE specification in force, as a value: which
/// message a channel of a given role takes, and what switches a zone's MIDI
/// Mode. The one specification today is MPE 1.1 (M1-100, the MPE
/// Configuration Message); the MIDI-CI MPE Profile (M2-120) is another value
/// of this type, with its own table, and nothing outside the methods below
/// branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpeSpec {
    /// MPE 1.1, Appendix E (Table 5), and the sections it points to.
    Mpe11,
}

/// What a receiver does with a message its table has no use for there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Receive {
    /// The message is for the instrument.
    Accept,
    /// The message is dropped: the instrument never sees it.
    Ignore,
}

impl MpeSpec {
    /// What a channel of `role` does with `voice`, in a zone that is in MIDI
    /// Mode 4 when `mono`.
    ///
    /// Table 5 marks a message "P: Prohibited" for a sender, and for a
    /// receiver on the same row. Section 2.3.1 says what a receiver does with
    /// a message that is not for the channel it came on: "it should ignore
    /// them". A prohibited message is therefore ignored, not acted on: acting
    /// on a message no conforming sender makes would only give a broken one
    /// something to break the zone with (an All Sound Off on the Manager
    /// Channel silencing every member, say).
    fn receive(self, role: Role, voice: &Voice2, mono: bool) -> Receive {
        match self {
            Self::Mpe11 => mpe11_receive(role, voice, mono),
        }
    }

    /// The MIDI Mode a Control Change names, if it is a Mode message that
    /// switches between the two the specification has: Mono Mode On (126) is
    /// Mode 4, Poly Mode On (127) is Mode 3 (section 2.2.4.3).
    fn mode_switch(self, controller: CCNumber) -> Option<bool> {
        match self {
            Self::Mpe11 => match controller.get() {
                CC_MONO_ON => Some(true),
                CC_POLY_ON => Some(false),
                _ => None,
            },
        }
    }
}

/// Control Change numbers of the Mode messages Table 5 names.
const CC_OMNI_ON: u8 = 125;
const CC_MONO_ON: u8 = 126;
const CC_POLY_ON: u8 = 127;

/// MPE 1.1, Appendix E (Table 5), receive columns, for the rows tutti
/// reads at this edge.
fn mpe11_receive(role: Role, voice: &Voice2, mono: bool) -> Receive {
    let ignored = match (role, voice) {
        // "Control Change #120 [All Sounds Off]": prohibited on the Manager
        // Channel, optional on a Member Channel.
        (Role::Master, Voice2::ControlChange { controller, .. }) => matches!(
            controller.get(),
            c if c == CCNumber::ALL_SOUND_OFF.get() || c == CC_MONO_ON || c == CC_POLY_ON
        ),
        // "RPN #6 [MPE Configuration Message]": prohibited on Member
        // Channels (the Manager Channels' is handled before this).
        (Role::Member, Voice2::Registered { bank, index, .. }) => {
            Rpn::new(*bank, *index) == Rpn::MPE_CONFIGURATION
        }
        // "Program Change" and "Bank Select": section 2.3.3, "A receiver
        // operating in MIDI Mode 3 shall ignore Program Change messages
        // received on Member Channels", and the table, Bank Select
        // "Prohibited in MIDI Mode 3" on them. (A Bank Select travels in the
        // Program Change the translator made of it.)
        (Role::Member, Voice2::ProgramChange { .. }) => !mono,
        _ => false,
    };
    // "Control Change #125 [Omni On]": prohibited on every channel.
    let omni =
        matches!(voice, Voice2::ControlChange { controller, .. } if controller.get() == CC_OMNI_ON);
    if ignored || omni {
        Receive::Ignore
    } else {
        Receive::Accept
    }
}

/// What the zone's channels were last told of the three MPE controls, kept
/// whether or not a note sounds (M1-100 sections 2.2.6 to 2.2.8).
#[derive(Debug, Clone, Copy, Default)]
struct Told {
    /// Pitch Bend, the 32-bit word.
    bend: Option<u32>,
    /// Channel Pressure.
    pressure: Option<u32>,
    /// Control Change #74.
    brightness: Option<u32>,
}

/// The value a note takes from its Member Channel and its Manager Channel
/// (M1-100 Appendix D lists Add and Max, and leaves the choice to the
/// manufacturer): the greater of the two, so a zone-wide value is a floor
/// under every note and an unset one adds nothing.
fn combined(member: Option<u32>, manager: Option<u32>) -> Option<u32> {
    match (member, manager) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Rewrites classic-MPE channel-spread into native MIDI-2 per-note messages.
///
/// Construct with the app-configured [`MpeMode`]; feed every inbound (already
/// MIDI-2-normalized) event through [`translate`](Self::translate).
#[derive(Debug)]
pub struct MpeIngest {
    /// The specification whose receive rules apply.
    spec: MpeSpec,
    /// The mode the host set: every Group starts in it, and a Group's own
    /// Configuration Messages change that Group's alone.
    mode: MpeMode,
    /// One state for each Group: a Group is a MIDI 1.0 channel space of its own
    /// (M2-104 section 2.1.2, Appendix B.1), so its zones, its member channels and
    /// the notes they hold are not another Group's.
    groups: [GroupState; MidiGroup::COUNT as usize],
}

/// What one Group's sixteen channels are: its zone layout, the note each
/// member channel holds and what each channel was last told.
#[derive(Debug)]
struct GroupState {
    mode: MpeMode,
    /// The note each member channel holds. The two zones share no channel
    /// ([`MpeZones`]), so one table serves both.
    held: held::HeldNotes,
    /// What each channel was last told: a member's own, which its next note
    /// starts with, and a master's, which every note of its zone combines.
    told: [Told; MidiChannel::COUNT as usize],
    /// Whether each zone (lower, upper) is in MIDI Mode 4 (section 2.2.4.3);
    /// Mode 3 is the default.
    mono: [bool; 2],
    /// The channels the last host-set zone change took into or out of MPE
    /// control, one bit each, whose notes the next
    /// [`zone_defaults`](MpeIngest::zone_defaults) stops.
    entered: u16,
}

impl GroupState {
    fn new(mode: MpeMode) -> Self {
        Self {
            mode,
            held: held::HeldNotes::default(),
            told: [Told::default(); MidiChannel::COUNT as usize],
            mono: [false; 2],
            entered: 0,
        }
    }

    /// Takes `mode` as this Group's layout, keeping what the channels it does
    /// not change were told and hold. Returns the channels that entered or
    /// left MPE control, or changed what they are in it (one bit each): their
    /// notes and controls are what section 2.2.3 has a receiver stop and
    /// reset, and their record here goes.
    fn reconfigure(&mut self, mode: MpeMode) -> u16 {
        let mut changed = 0u16;
        for c in 0..MidiChannel::COUNT {
            let channel = MidiChannel::of(c);
            let (before, after) = (membership(&self.mode, channel), membership(&mode, channel));
            if before != after {
                changed |= 1 << c;
                self.held.release(channel);
                self.told[usize::from(c)] = Told::default();
                for zone in [before, after].into_iter().flatten() {
                    self.mono[zone_index(zone.0)] = false;
                }
            }
        }
        self.mode = mode;
        changed
    }
}

/// What `channel` is in `mode`'s zones: its zone, and whether it is that
/// zone's master; `None` for a channel in no zone.
fn membership(mode: &MpeMode, channel: MidiChannel) -> Option<(MpeZone, bool)> {
    let MpeMode::Zones(zones) = mode else {
        return None;
    };
    let config = zones.zone_of(channel)?;
    Some((config.zone(), config.is_master_channel(channel)))
}

fn zone_index(zone: MpeZone) -> usize {
    match zone {
        MpeZone::Lower => 0,
        MpeZone::Upper => 1,
    }
}

/// A Control Change at `value` zero on `channel`, `controller`.
fn control(group: MidiGroup, channel: MidiChannel, controller: CCNumber, offset: Offset) -> UmpAt {
    (
        offset,
        Message::Midi2(
            Address { group, channel },
            Voice2::ControlChange {
                controller,
                value: 0,
            },
        )
        .into(),
    )
}

/// M1-100 section 2.2.3: "the receiver shall stop all Sounding Notes and
/// reset all controls to reasonable default values on each Channel entering
/// or leaving MPE control". For each channel of `channels` (one bit each),
/// All Sound Off and Reset All Controllers, at `offset`.
fn send_stops(group: MidiGroup, offset: Offset, channels: u16, out: &mut impl FnMut(UmpAt)) {
    for c in 0..MidiChannel::COUNT {
        if channels & (1 << c) != 0 {
            let channel = MidiChannel::of(c);
            out(control(group, channel, CCNumber::ALL_SOUND_OFF, offset));
            out(control(group, channel, CCNumber::RESET_ALL, offset));
        }
    }
}

/// What a channel is to the configured zones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The zone's master: zone-wide messages.
    Master,
    /// A member: one note's expression.
    Member,
}

/// A per-note message on `address`, at `offset`.
fn per_note(address: Address, offset: Offset, voice: Voice2) -> UmpAt {
    (offset, Ump::from(Message::Midi2(address, voice)))
}

impl MpeIngest {
    /// An ingest configured for `mode` on every Group, holding no notes.
    pub fn new(mode: MpeMode) -> Self {
        Self {
            spec: MpeSpec::Mpe11,
            mode,
            groups: std::array::from_fn(|_| GroupState::new(mode)),
        }
    }

    /// The zone layout the host configured, which every Group starts in. A
    /// Group's channel messages are folded to per-note or passed through by
    /// [`mode_of`](Self::mode_of).
    pub fn mode(&self) -> &MpeMode {
        &self.mode
    }

    /// The zone layout in force on `group`: the configured one until an MPE
    /// Configuration Message on that Group changed it.
    pub fn mode_of(&self, group: MidiGroup) -> &MpeMode {
        &self.groups[usize::from(group.get())].mode
    }

    /// Reconfigure every Group to a new zone layout. Off-RT: call at wiring
    /// time, or via
    /// [`MidiIngestControls::set_mpe_mode`](crate::MidiIngestControls::set_mpe_mode),
    /// which defers adoption to the ingest node's next block.
    ///
    /// What a channel held or was told is kept where the channel is what it
    /// was, and forgotten where it entered or left MPE control. The sounding
    /// notes of those channels are the instrument's, and M1-100 section 2.2.3
    /// has the receiver stop them: [`zone_defaults`](Self::zone_defaults),
    /// which the host sends downstream after this, carries the All Sound Off
    /// and Reset All Controllers that do.
    pub fn set_mode(&mut self, mode: MpeMode) {
        self.mode = mode;
        for group in &mut self.groups {
            group.entered = group.reconfigure(mode);
        }
    }

    /// What a host-set mode change ([`set_mode`](Self::set_mode)) sends
    /// downstream for `group`, as events at `offset`: for each channel that
    /// entered or left MPE control, All Sound Off and Reset All Controllers
    /// (M1-100 section 2.2.3, "stop all Sounding Notes and reset all controls
    /// to reasonable default values"); then, for each zone, RPN #00/00 = ±2
    /// on its master channel and RPN #00/07 = ±48 on each member channel,
    /// what an MPE Configuration Message resets a receiver to, so a receiver
    /// that followed the old zones' ranges starts the new ones at their
    /// defaults, as an MCM from the wire would have it. No ranges for
    /// [`MpeMode::Disabled`] or [`MpeMode::SingleChannelRotation`].
    pub fn zone_defaults(&self, group: MidiGroup, offset: Offset, mut out: impl FnMut(UmpAt)) {
        send_stops(
            group,
            offset,
            self.groups[usize::from(group.get())].entered,
            &mut out,
        );
        if let MpeMode::Zones(zones) = self.mode_of(group) {
            for config in zones.zones() {
                send_zone_defaults(config, group, offset, &mut out);
            }
        }
    }

    /// Rewrite one already-normalized (MIDI-2) event.
    ///
    /// Yields the events to forward downstream, in order:
    /// - a member-channel bend/pressure/CC74 becomes its native per-note form,
    ///   for the note that channel holds; with no note held it is remembered,
    ///   and the channel's next Note On is preceded by the per-note forms of
    ///   what it was last told (M1-100 sections 2.2.6 to 2.2.8);
    /// - a manager-channel pressure/CC74 passes, and also reaches each note
    ///   its zone holds, combined with the note's own (`combined`);
    /// - a member-channel bend sensitivity (RPN #00/00) becomes a per-note
    ///   bend sensitivity (RPN #00/07) on each of the zone's member channels;
    /// - an MPE Configuration Message is absorbed, the channels it takes into
    ///   or out of MPE control are stopped and reset (section 2.2.3), and the
    ///   zone it configures is reset to its default ranges
    ///   ([`zone_defaults`](Self::zone_defaults));
    /// - a message the specification's receive table prohibits there is
    ///   dropped (`MpeSpec::receive`);
    /// - note-on/off and master-channel messages pass (with the channel
    ///   map updated for note-on/off so later channel messages resolve), and
    ///   so does every message of a channel in no zone;
    /// - already-native per-note messages pass through unchanged.
    ///
    /// No call means "drop this event".
    pub fn translate(&mut self, event: &UmpAt, mut out: impl FnMut(UmpAt)) {
        if let Some(one) = self.translate_one(event, &mut out) {
            out(one);
        }
    }

    /// [`translate`](Self::translate): the one event it becomes, which goes
    /// after whatever it sent through `out` itself (a fan-out, the setup of a
    /// Note On), or `None` when it sent what it becomes or dropped it.
    fn translate_one(&mut self, event: &UmpAt, out: &mut impl FnMut(UmpAt)) -> Option<UmpAt> {
        // An MPE Configuration Message (RPN 0x00/0x06, M2-104 §7.4.7) reconfigures
        // the zone from the wire — handled in *any* mode, including Disabled (a
        // controller declaring its zone should enable MPE). The message itself
        // is absorbed (configuration, not a musical event); what it resets
        // goes on: the channels it moved are stopped, and the configured
        // zone's default bend ranges.
        //
        // `from_mcm` requires the master channel be Ch1 or Ch16 per RP-053, so
        // RPN 0x00/0x06 on any other channel is ordinary parameter traffic and
        // passes through rather than being swallowed as configuration.
        let decoded = event.1.decode().ok();
        if let (Some((zone, members)), Some(Message::Midi2(address, _))) =
            (MpeZoneConfig::from_mcm(&event.1), decoded)
        {
            if !self.reconfigure_from_mcm(address.group, zone, members, event.0, out) {
                return None;
            }
            // A zone disabled has no ranges to reset.
            if let MpeMode::Zones(zones) = self.mode_of(address.group)
                && let Some(configured) = match zone {
                    MpeZone::Lower => zones.lower(),
                    MpeZone::Upper => zones.upper(),
                }
            {
                send_zone_defaults(configured, address.group, event.0, out);
            }
            return None;
        }

        let Some(Message::Midi2(address, voice)) = decoded else {
            // Not a channel-voice-2 message (System, SysEx, …) — pass through.
            return Some(*event);
        };
        let mode = *self.mode_of(address.group);
        if matches!(mode, MpeMode::Disabled) {
            return Some(*event);
        }

        if let MpeMode::SingleChannelRotation { channel } = mode {
            // Rotation is a sender's scheme and a receiver need not know it
            // (M2-104 Appendix C.3): this mode owns one channel, so the
            // messages of the others are ignored, and the one's pass as they
            // are, native per-note messages included.
            return (address.channel == channel).then_some(*event);
        }
        if self.member_bend_range(event, address, voice, out) {
            return None;
        }
        self.translate_zoned(event, address, voice, out)
    }

    /// A bend sensitivity (RPN #00/00) on a member channel, as a per-note
    /// bend sensitivity (RPN #00/07) on each of its zone's member channels,
    /// sent through `out`; `false`, sending nothing, for anything else (the
    /// master channel's passes through).
    ///
    /// M1-100 section 2.2.5: "A receiver shall apply the last Pitch Bend
    /// Sensitivity message received on any Member Channel to all Member
    /// Channels in the Zone."
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
        let MpeMode::Zones(zones) = self.mode_of(address.group) else {
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
    /// channel it arrived on, in the Group it arrived in (RP-053 / M2-104; a
    /// Group's messages apply to no other, Appendix B.1), `members == 0` disables
    /// that zone. The zone on the *other* side is preserved (a dual-zone setup
    /// configures each zone with its own MCM).
    ///
    /// The channels the new layout takes into or out of MPE control are stopped
    /// and reset through `out` at `offset` (section 2.2.3); a message that
    /// changes nothing stops nothing.
    ///
    /// A message that would leave the two zones sharing a member channel is
    /// ignored, like any other MCM RP-053 does not allow: the zones keep what
    /// they had, and this returns `false`.
    fn reconfigure_from_mcm(
        &mut self,
        group: MidiGroup,
        zone: MpeZone,
        members: u8,
        offset: Offset,
        out: &mut impl FnMut(UmpAt),
    ) -> bool {
        // Snapshot the current per-side configs so one MCM only touches its side.
        let (mut lower, mut upper) = match self.mode_of(group) {
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
        // The Group's channels that changed lose their notes and their record;
        // the other Groups, and the channels that did not change, keep theirs.
        let state = &mut self.groups[usize::from(group.get())];
        let changed = state.reconfigure(mode);
        state.entered = 0;
        send_stops(group, offset, changed, out);
        true
    }

    /// Zone mode: member-channel channel-messages fold onto the note that
    /// member channel holds; master-channel and native per-note pass.
    fn translate_zoned(
        &mut self,
        event: &UmpAt,
        address: Address,
        voice: Voice2,
        out: &mut impl FnMut(UmpAt),
    ) -> Option<UmpAt> {
        let Address { group, channel: ch } = address;
        // M1-100 section 2.2.1: "Any MIDI Channels not assigned to any Zone
        // remain available for conventional use."
        let Some(role) = self.role(address) else {
            return Some(*event);
        };
        let state = &mut self.groups[usize::from(group.get())];
        let config = match &state.mode {
            MpeMode::Zones(zones) => *zones.zone_of(ch)?,
            _ => return Some(*event),
        };
        let zone = zone_index(config.zone());
        if self.spec.receive(role, &voice, state.mono[zone]) == Receive::Ignore {
            return None;
        }
        let master = config.master_channel();
        let slot = usize::from(ch.get());
        let manager = state.told[usize::from(master.get())];
        let at = event.0;
        match voice {
            // M2-104 7.4.2: a MIDI 2.0 Note On at velocity 0 is a Note On, and
            // binds its channel like any other (the translator before this
            // turns a MIDI 1.0 one into a Note Off).
            Voice2::NoteOn { note, .. } => {
                if role == Role::Member {
                    state.held.bind(ch, note);
                    // M1-100 section 2.2.6: "The pitch of a new note is
                    // affected by the Pitch Bend message most recently
                    // received on both the Manager Channel and that note's
                    // Member Channel before Note On", and sections 2.2.7 and
                    // 2.2.8 the same of Channel Pressure and Control Change
                    // #74. The note's Number is known only now, so what the
                    // channel was told goes ahead of the Note On, for the
                    // note it starts (the manager's bend is the instrument's
                    // own, summed with the member's).
                    let told = state.told[slot];
                    if let Some(value) = told.bend {
                        out(per_note(
                            address,
                            at,
                            Voice2::PerNotePitchBend { note, value },
                        ));
                    }
                    if let Some(value) = combined(told.pressure, manager.pressure) {
                        out(per_note(address, at, Voice2::PolyPressure { note, value }));
                    }
                    if let Some(value) = combined(told.brightness, manager.brightness) {
                        out(per_note(address, at, assignable(note, value)));
                    }
                }
                // The note itself always flows through unchanged.
                Some(*event)
            }
            Voice2::NoteOff { note, .. } => {
                // "A Released Note shall cease to be affected by Pitch Bend
                // messages from the Member Channels after the Note Off
                // message occurs": the channel holds no note, and what it is
                // told from here on waits for the next one.
                if role == Role::Member && state.held.note_on(ch) == Some(note) {
                    state.held.release(ch);
                }
                Some(*event)
            }
            Voice2::PitchBend(value) => match role {
                // Master-channel bend is global: the instrument sums it with
                // each member's (Appendix C.5), and tracks it with no note
                // sounding.
                Role::Master => Some(*event),
                Role::Member => {
                    state.told[slot].bend = Some(value);
                    let note = state.held.note_on(ch)?;
                    Some(per_note(
                        address,
                        at,
                        Voice2::PerNotePitchBend { note, value },
                    ))
                }
            },
            Voice2::ChannelPressure(value) => {
                state.told[slot].pressure = Some(value);
                match role {
                    Role::Master => {
                        for member in config.member_channels() {
                            if let Some(note) = state.held.note_on(member) {
                                let value = combined(
                                    state.told[usize::from(member.get())].pressure,
                                    Some(value),
                                )
                                .unwrap_or(value);
                                let to = Address {
                                    group,
                                    channel: member,
                                };
                                out(per_note(to, at, Voice2::PolyPressure { note, value }));
                            }
                        }
                        Some(*event)
                    }
                    Role::Member => {
                        let note = state.held.note_on(ch)?;
                        let value = combined(Some(value), manager.pressure)?;
                        Some(per_note(address, at, Voice2::PolyPressure { note, value }))
                    }
                }
            }
            Voice2::ControlChange { controller, value } if controller == CCNumber::BRIGHTNESS => {
                state.told[slot].brightness = Some(value);
                match role {
                    Role::Master => {
                        for member in config.member_channels() {
                            if let Some(note) = state.held.note_on(member) {
                                let value = combined(
                                    state.told[usize::from(member.get())].brightness,
                                    Some(value),
                                )
                                .unwrap_or(value);
                                let to = Address {
                                    group,
                                    channel: member,
                                };
                                out(per_note(to, at, assignable(note, value)));
                            }
                        }
                        Some(*event)
                    }
                    Role::Member => {
                        let note = state.held.note_on(ch)?;
                        let value = combined(Some(value), manager.brightness)?;
                        // CC74 -> Assignable Per-Note Controller index 74 (the MPE slide).
                        Some(per_note(address, at, assignable(note, value)))
                    }
                }
            }
            Voice2::ControlChange { controller, .. } => {
                // Section 2.2.4.3: a Mode message "to the lowest numbered
                // Member Channel of a zone (not to the Manager Channel)"
                // switches the zone between Mode 3 and Mode 4.
                if let Some(mono) = self.spec.mode_switch(controller)
                    && config.member_channels().map(|c| c.get()).min() == Some(ch.get())
                {
                    state.mono[zone] = mono;
                }
                Some(*event)
            }
            // Everything else (native per-note messages, program change, ...)
            // passes through unchanged.
            _ => Some(*event),
        }
    }

    /// What `address`'s channel is to its Group's zones, or `None` when no
    /// zone owns it.
    fn role(&self, address: Address) -> Option<Role> {
        let (_, master) = membership(self.mode_of(address.group), address.channel)?;
        Some(if master { Role::Master } else { Role::Member })
    }

    /// Forget every held note and what each channel was told, leaving the
    /// zone configuration intact.
    ///
    /// The counterpart to [`set_mode`](Self::set_mode) for a panic / all-notes-off:
    /// the mapping table is cleared, so a subsequent member-channel message
    /// with no matching note-on is remembered for the next note rather than
    /// addressed at a stale one.
    pub fn reset(&mut self) {
        for group in &mut self.groups {
            group.held.clear();
            group.told = [Told::default(); MidiChannel::COUNT as usize];
        }
    }
}

/// CC74 as an Assignable Per-Note Controller of `note`.
fn assignable(note: NoteNumber, value: u32) -> Voice2 {
    Voice2::AssignablePerNote {
        note,
        controller: CCNumber::BRIGHTNESS.get(),
        value,
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
