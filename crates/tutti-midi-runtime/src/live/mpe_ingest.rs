//! MPE ingestion as an input-edge **transform**.
//!
//! Per M2-104 (Appendix C.3, §7.4.7), classic MPE — spreading notes across
//! member channels so a channel-wide pitch bend / pressure / CC74 acts *per
//! note* — is a **MIDI-1 ingestion concern**, not a synthesis concern. A
//! MIDI-2-native receiver only handles per-note messages; it needs no
//! knowledge of the rotation scheme.
//!
//! So [`MpeIngest`] sits at the input edge (in [`MidiIngestNode`](crate::MidiIngestNode))
//! and rewrites the classic-MPE channel-spread into **native MIDI-2 per-note
//! messages**. A channel pitch bend on a member channel becomes a Per-Note Pitch
//! Bend addressed to the note that channel holds; channel pressure becomes a
//! per-note (Poly) Pressure; CC74 becomes an Assignable Per-Note Controller.
//! A Manager Channel's messages pass, and the zone-wide ones (pedals, the
//! panics, Reset All Controllers) and its Pitch Bend and bend range are also
//! sent on each member channel of its zone, so a receiver that reads one channel
//! at a time needs no zone. Already-native per-note messages pass through
//! unchanged. Downstream nodes then only ever see native per-note MIDI-2, and
//! no zone: this transform is the one place the zones are held, and the one
//! receiver of the MPE Configuration Message.
//!
//! # Where expression state lives
//!
//! The voices hold the expression of a *sounding* note, derived from the native
//! per-note messages this emits. What a member channel was told *before* its
//! Note On is another matter: M1-100 §2.2.6 to 2.2.8 have a receiver keep
//! tracking the channel's Pitch Bend, Channel Pressure and CC 74 while no note
//! sounds, and a note's number is not known until its Note On. So the
//! transform keeps the last value of each, per channel, and sends them as
//! per-note messages ahead of the Note On they belong to (which a per-note
//! receiver, `tutti_voicing::PerNoteState`, starts the note with). A manager's
//! pressure and CC 74 are combined into each note of its zone (the greater
//! of the two, `combined`). The transform forgets a note at
//! its note-off, and a channel's values when it enters or leaves MPE control.
//!
//! # The receive rules are a value
//!
//! Which message a channel takes and what switches a zone's MIDI Mode is
//! `MpeSpec`'s, one match per rule set: MPE 1.1 (M1-100 Appendix E, Table 5)
//! or the MIDI-CI MPE Profile (M2-120 Appendix E, Table 15). A zone carries its
//! specification ([`MpeZoneConfig::spec`]), fixed by the constructor that built
//! it: the zones of [`MpeMode`] are usually MPE 1.1's and the zones
//! [`set_profile`](MpeIngest::set_profile) enables the Profile's, so no
//! branch elsewhere asks which is in force. A message the table prohibits is
//! ignored (M1-100 §2.3.1). A channel in no zone is conventional and passes
//! whole (M1-100 §2.2.1).
//!
//! The two differ in what a zone is and in what it takes: the Profile's
//! Manager Channel is any channel and its zone the channels above it (M2-120
//! §2.2), the Manager and every Member share one bend range set on the Manager
//! (§3.5), All Sounds Off is taken on the Manager, Reset All Controllers and
//! All Notes Off are the Manager's alone, and Omni Off, Mono Mode On and Poly
//! Mode On are prohibited (Table 15). A Group with Profile zones has no MPE
//! Configuration Message: the Profile replaces it (§2.2). Disabling the zones
//! goes back to the MPE 1.1 layout the Group had.
//!
//! A Mono Mode On or Poly Mode On sent to a zone's lowest-numbered member
//! channel, not to its manager, switches the zone between MIDI Mode 4 and
//! Mode 3 (§2.2.4.3). The ingest records only that switch; what Mono means for
//! a note is the instrument's own `tutti_voicing::Rules`. A Poly Key
//! Pressure on a member channel is not filtered: it cannot be told from a
//! native per-note pressure message.
//!
//! # What a manager's messages become
//!
//! A manager channel's Damper and Sostenuto pedals, All Sound Off (where the
//! rules take it there), All Notes Off and Reset All Controllers apply to the
//! whole zone (M1-100 section 2.3.1) and a member's to its own channel. The
//! transform sends each of them on the manager and on every member channel of
//! its zone; the instrument (`tutti_voicing::Voicing`) reads one channel's
//! message as that channel's own and holds no zone.
//!
//! A manager's Pitch Bend moves every note of its zone (RP-053): it goes on and
//! is copied onto each member channel as that channel's own Pitch Bend, over a
//! sensitivity that is the manager's (its Pitch Bend Sensitivity, RPN #00/00, is
//! copied onto each member likewise, and re-sent when it changes). A member's
//! own bend is a per-note bend, which a receiver sums with its channel's
//! (M2-104 section 7.4.13), so a note sounds both, each over its own range. A
//! member's Reset All Controllers centres the copy with its own wheel, so the
//! manager's bend is sent to it again after the reset; the sensitivity stays.
//!
//! # Changing the zones
//!
//! A receiver that changes its zone layout stops the sounding notes and resets
//! the controllers of every channel that enters or leaves MPE control
//! (§2.2.3). An MCM from the wire sends, for each such channel, All Sound Off
//! and Reset All Controllers before the zone's default ranges, and a host-set
//! mode ([`MpeIngest::set_mode`]) leaves them for
//! [`zone_defaults`](MpeIngest::zone_defaults), which the host sends. A channel
//! that did not change keeps its notes and what it was told.
//!
//! # Bend ranges
//!
//! A pitch-bend sensitivity (RPN #00/00) on the master channel sets the
//! master channel's own bend range, and passes through (and is copied onto each
//! member channel, which carries the master's bend over it). On a member channel
//! it sets the range of every member channel's bends (MPE, RP-053 §2.4),
//! which this transform has just made per-note bends: so it becomes a
//! per-note bend sensitivity (RPN #00/07, M2-104 §7.4.13) on each of the
//! zone's member channels, the message a per-note receiver
//! (`tutti_voicing::PerNoteState`) follows.
//!
//! An MPE Configuration Message resets the zone it configures to MPE's
//! default ranges (RP-053 / M1-100-UM, Pitch Bend Sensitivity: a receiver
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
use tutti_midi_protocol::{
    ChannelResponse, MpeMode, MpeSpec, MpeZone, MpeZoneConfig, MpeZones, PitchBendSensitivity,
};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, Rpn, Ump, Voice2};
use tutti_ump::{BendValue, ControlValue};

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
                    value: ControlValue::new(data),
                },
            )
            .into(),
        ));
    }
}

/// The pitch-bend ranges a zone starts at: a receiver of an MPE Configuration
/// Message (M1-100 section 2.2.5) or of an enabled MPE Profile (M2-120 section
/// 3.5) sets them. As this transform's output carries them: RPN #00/00 on the
/// manager, which passes through as a channel bend range, RPN #00/00 on each
/// member with the manager's, since the manager's bend is copied onto it (the
/// member's own bend is per-note, over RPN #00/07).
fn send_zone_defaults(
    zone: &ZoneView,
    group: MidiGroup,
    offset: Offset,
    out: &mut impl FnMut(UmpAt),
) {
    send_rpns(
        group,
        offset,
        core::iter::once(zone.manager),
        Rpn::PITCH_BEND_SENSITIVITY,
        zone.manager_range.to_rpn_bits(),
        out,
    );
    send_rpns(
        group,
        offset,
        zone.member_channels(),
        Rpn::PITCH_BEND_SENSITIVITY,
        zone.manager_range.to_rpn_bits(),
        out,
    );
    send_rpns(
        group,
        offset,
        zone.member_channels(),
        Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
        zone.member_range.to_rpn_bits(),
        out,
    );
}

/// Whether `voice` is one of the messages that affect every sounding note
/// across a zone's Manager Channel and its Member Channels, and are sent only
/// on the Manager Channel (M1-100 section 2.3.1): the Damper and Sostenuto
/// pedals, All Sound Off (which MPE 1.1 prohibits on the Manager Channel and
/// the Profile takes there: `receive` has dropped it before this asks), Reset
/// All Controllers and All Notes Off.
fn zone_wide(voice: &Voice2) -> bool {
    matches!(voice, Voice2::ControlChange { controller, .. } if [
        CCNumber::SUSTAIN,
        CCNumber::SOSTENUTO,
        CCNumber::ALL_SOUND_OFF,
        CCNumber::RESET_ALL,
        CCNumber::ALL_NOTES_OFF,
    ]
    .contains(controller))
}

/// What a receiver does with a message its table has no use for there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Receive {
    /// The message is for the instrument.
    Accept,
    /// The message is dropped: the instrument never sees it.
    Ignore,
}

/// What a channel of `role` does with `voice` under `spec`, in a zone that is
/// monophonic when `mono` (MIDI Mode 4 under MPE 1.1, a Monophonic Channel
/// Response under the Profile).
///
/// Each table marks a message prohibited (P) for a sender, and for a receiver
/// on the same row. A receiver ignores a message that is not for the channel it
/// came on (M1-100 section 2.3.1). A prohibited message is therefore ignored,
/// not acted on: acting on a message no conforming sender makes would only
/// give a broken one something to break the zone with (an All Sound Off on
/// the Manager Channel silencing every member, say).
fn receive(spec: MpeSpec, role: Role, voice: &Voice2, mono: bool) -> Receive {
    match spec {
        MpeSpec::Mpe11 => mpe11_receive(role, voice, mono),
        MpeSpec::Profile => profile_receive(role, voice, mono),
    }
}

/// The MIDI Mode a Control Change names, if it is a Mode message that
/// switches between the two the specification has: under MPE 1.1, Mono Mode
/// On (126) is Mode 4 and Poly Mode On (127) is Mode 3 (M1-100 section
/// 2.2.4.3). The Profile has no such switch: both are prohibited, and the
/// receiver's Channel Response is its own (M2-120 section 3.6).
fn mode_switch(spec: MpeSpec, controller: CCNumber) -> Option<bool> {
    match spec {
        MpeSpec::Mpe11 => match controller.get() {
            CC_MONO_ON => Some(true),
            CC_POLY_ON => Some(false),
            _ => None,
        },
        MpeSpec::Profile => None,
    }
}

/// Control Change numbers of the Channel Mode messages the tables name.
const CC_RESET_ALL: u8 = 121;
const CC_ALL_NOTES_OFF: u8 = 123;
const CC_OMNI_OFF: u8 = 124;
const CC_OMNI_ON: u8 = 125;
const CC_MONO_ON: u8 = 126;
const CC_POLY_ON: u8 = 127;

/// MPE 1.1, Appendix E (Table 5), receive columns, for the rows tutti
/// reads at this edge.
fn mpe11_receive(role: Role, voice: &Voice2, mono: bool) -> Receive {
    let ignored = match (role, voice) {
        // Control Change #120 (All Sounds Off): prohibited on the Manager
        // Channel, optional on a Member Channel. Mono Mode On and Poly Mode
        // On are prohibited there too.
        (Role::Master, Voice2::ControlChange { controller, .. }) => matches!(
            controller.get(),
            c if c == CCNumber::ALL_SOUND_OFF.get() || c == CC_MONO_ON || c == CC_POLY_ON
        ),
        // RPN #6 (MPE Configuration Message): prohibited on Member
        // Channels (the Manager Channels' is handled before this).
        (Role::Member, Voice2::Registered { bank, index, .. }) => {
            Rpn::new(*bank, *index) == Rpn::MPE_CONFIGURATION
        }
        // Program Change and Bank Select: section 2.3.3 has a receiver in MIDI
        // Mode 3 ignore Program Change on Member Channels, and the table
        // prohibits Bank Select there in Mode 3. (A Bank Select travels in the
        // Program Change the translator made of it.)
        (Role::Member, Voice2::ProgramChange { .. }) => !mono,
        _ => false,
    };
    // Control Change #125 (Omni On): prohibited on every channel.
    let omni =
        matches!(voice, Voice2::ControlChange { controller, .. } if controller.get() == CC_OMNI_ON);
    if ignored || omni {
        Receive::Ignore
    } else {
        Receive::Accept
    }
}

/// The MIDI-CI MPE Profile, Appendix E (Table 15), receive columns, for the
/// rows tutti reads at this edge. Where it differs from [`mpe11_receive`]:
/// All Sounds Off is taken on the Manager Channel too; Reset All Controllers
/// and All Notes Off are the Manager's alone; Omni Off, Mono Mode On and
/// Poly Mode On are prohibited everywhere (the Channel Response is not a Mode
/// message); and a Member Channel's Pitch Bend Sensitivity is prohibited,
/// since it is set on the Manager (section 3.5). Notes on the Manager Channel
/// are undefined, which is no rule to drop them by.
fn profile_receive(role: Role, voice: &Voice2, mono: bool) -> Receive {
    let ignored = match (role, voice) {
        // Control Changes #124 to #127: prohibited on every channel.
        (_, Voice2::ControlChange { controller, .. }) => {
            (CC_OMNI_OFF..=CC_POLY_ON).contains(&controller.get())
                || (role == Role::Member
                    && [CC_RESET_ALL, CC_ALL_NOTES_OFF].contains(&controller.get()))
        }
        // RPN #0 (Pitch Bend Sensitivity): prohibited on Member Channels.
        (Role::Member, Voice2::Registered { bank, index, .. }) => {
            Rpn::new(*bank, *index) == Rpn::PITCH_BEND_SENSITIVITY
        }
        // Program Change and Bank Select: not sent on Member Channels in
        // Polyphonic Channel Response, and responded to there only in
        // Monophonic (section 4.6).
        (Role::Member, Voice2::ProgramChange { .. }) => !mono,
        _ => false,
    };
    if ignored {
        Receive::Ignore
    } else {
        Receive::Accept
    }
}

/// What the zone's channels were last told of the three MPE controls, kept
/// whether or not a note sounds (M1-100 sections 2.2.6 to 2.2.8).
#[derive(Debug, Clone, Copy, Default)]
struct Told {
    /// Pitch Bend, the 32-bit word. A member's own, which its next note starts
    /// with; a manager's, which every member of its zone carries as its
    /// channel bend.
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
    /// The MPE 1.1 layout: the host's, and what this Group's Configuration
    /// Messages changed it to. In force while no Profile zone is enabled.
    mode: MpeMode,
    /// The zones the MIDI-CI MPE Profile (M2-120) enabled on this Group,
    /// which replace `mode` for as long as there are any.
    profile: Option<MpeZones>,
    /// The note each member channel holds. The two zones share no channel
    /// ([`MpeZones`]), so one table serves both.
    held: held::HeldNotes,
    /// What each channel was last told: a member's own, which its next note
    /// starts with, and a master's, which every note of its zone combines.
    told: [Told; MidiChannel::COUNT as usize],
    /// Whether the MPE 1.1 zone whose manager is the channel is in MIDI Mode 4
    /// (M1-100 section 2.2.4.3); Mode 3 is the default. A Profile zone's
    /// response is its own.
    mono: [bool; MidiChannel::COUNT as usize],
    /// The channels the last host-set zone change took into or out of MPE
    /// control, one bit each, whose notes the next
    /// [`zone_defaults`](MpeIngest::zone_defaults) stops.
    entered: u16,
}

impl GroupState {
    fn new(mode: MpeMode) -> Self {
        let mut state = Self {
            mode,
            profile: None,
            held: held::HeldNotes::default(),
            told: [Told::default(); MidiChannel::COUNT as usize],
            mono: [false; MidiChannel::COUNT as usize],
            entered: 0,
        };
        // Every zone starts in the mode its response names.
        for channel in MidiChannel::all() {
            if let Some(zone) = state.zone_of(channel) {
                state.mono[zone.manager.index()] = zone.response == ChannelResponse::Monophonic;
            }
        }
        state
    }

    /// The zone `channel` is in under the layout in force: the Profile's
    /// zones while it has any, else the MPE 1.1 mode's.
    fn zone_of(&self, channel: MidiChannel) -> Option<ZoneView> {
        zone_view(&self.mode, self.profile.as_ref(), &self.mono, channel)
    }

    /// Takes `mode` and `profile` as this Group's layout, keeping what the
    /// channels it does not change were told and hold. Returns the channels
    /// that entered or left MPE control, or changed what they are in it (one
    /// bit each): their notes and controls are what M1-100 section 2.2.3 and
    /// M2-120 section 3.3 have a receiver stop and reset, and their record
    /// here goes.
    fn reconfigure(&mut self, mode: MpeMode, profile: Option<MpeZones>) -> u16 {
        let mut changed = 0u16;
        let mut entering = [None; MidiChannel::COUNT as usize];
        for c in 0..MidiChannel::COUNT {
            let channel = MidiChannel::of(c);
            let before = self.zone_of(channel);
            let after = zone_view(&mode, profile.as_ref(), &self.mono, channel);
            if before.map(|z| z.key(channel)) != after.map(|z| z.key(channel)) {
                changed |= 1 << c;
                self.held.release(channel);
                self.told[usize::from(c)] = Told::default();
                if let Some(zone) = before {
                    self.mono[zone.manager.index()] = false;
                }
                entering[usize::from(c)] = after;
            }
        }
        // A zone that took a channel starts in the mode its response names.
        for zone in entering.into_iter().flatten() {
            self.mono[zone.manager.index()] = zone.response == ChannelResponse::Monophonic;
        }
        self.mode = mode;
        self.profile = profile;
        changed
    }
}

/// One zone of the layout in force, as the receive rules read it, whichever
/// specification made it.
#[derive(Clone, Copy)]
struct ZoneView {
    spec: MpeSpec,
    manager: MidiChannel,
    /// The channels above or below the manager the zone's members occupy,
    /// ascending: `first..first + count`.
    first_member: u8,
    member_count: u8,
    manager_range: PitchBendSensitivity,
    member_range: PitchBendSensitivity,
    /// How the zone's members were configured to take notes.
    response: ChannelResponse,
    /// Whether the members take one note each: MIDI Mode 4, or a Monophonic
    /// Channel Response.
    monophonic: bool,
}

impl ZoneView {
    /// What identifies `channel`'s place in its zone, to tell when a change of
    /// layout moved it: the specification, the manager and its role.
    fn key(&self, channel: MidiChannel) -> (MpeSpec, u8, Role) {
        (self.spec, self.manager.get(), self.role_of(channel))
    }

    fn member_channels(&self) -> impl Iterator<Item = MidiChannel> + Clone {
        (self.first_member..self.first_member + self.member_count).filter_map(MidiChannel::new)
    }

    fn role_of(&self, channel: MidiChannel) -> Role {
        if channel == self.manager {
            Role::Master
        } else {
            Role::Member
        }
    }
}

/// How `config` reads under `mono`, the MIDI Mode 4 switches of the MPE 1.1
/// zones.
fn view_of(config: &MpeZoneConfig, mono: &[bool; MidiChannel::COUNT as usize]) -> Option<ZoneView> {
    let first_member = config.member_channels().next()?;
    Some(ZoneView {
        spec: config.spec(),
        manager: config.master_channel(),
        first_member: first_member.get(),
        member_count: config.member_count(),
        manager_range: config.pitch_bend_range_for(config.master_channel()),
        member_range: config.pitch_bend_range_for(first_member),
        response: config.channel_response(),
        // The Profile's response is the zone's; MPE 1.1's mode is switched by
        // the wire, from the response the zone started in.
        monophonic: match config.spec() {
            MpeSpec::Mpe11 => mono[config.master_channel().index()],
            MpeSpec::Profile => config.channel_response() == ChannelResponse::Monophonic,
        },
    })
}

/// The zone of `channel` under `profile` if it has zones, else under `mode`.
fn zone_view(
    mode: &MpeMode,
    profile: Option<&MpeZones>,
    mono: &[bool; MidiChannel::COUNT as usize],
    channel: MidiChannel,
) -> Option<ZoneView> {
    let zones = match (profile, mode) {
        (Some(zones), _) | (None, MpeMode::Zones(zones)) => zones,
        (None, MpeMode::Disabled | MpeMode::SingleChannelRotation { .. }) => return None,
    };
    view_of(zones.zone_of(channel)?, mono)
}

/// A Control Change at `value` zero on `channel`, `controller`.
fn control(group: MidiGroup, channel: MidiChannel, controller: CCNumber, offset: Offset) -> UmpAt {
    (
        offset,
        Message::Midi2(
            Address { group, channel },
            Voice2::ControlChange {
                controller,
                value: ControlValue::new(0),
            },
        )
        .into(),
    )
}

/// M1-100 section 2.2.3: a channel entering or leaving MPE control has its
/// notes stopped and its controllers reset. For each channel of `channels`
/// (one bit each), All Sound Off and Reset All Controllers, at `offset`.
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

/// A MIDI 2.0 message `voice` on `address`, at `offset`.
fn message(address: Address, offset: Offset, voice: Voice2) -> UmpAt {
    (offset, Ump::from(Message::Midi2(address, voice)))
}

/// `voice` on each of `members` of `group`, at `offset`: a Manager Channel's
/// message copied onto its zone.
fn to_members(
    group: MidiGroup,
    members: impl Iterator<Item = MidiChannel>,
    offset: Offset,
    voice: Voice2,
    out: &mut impl FnMut(UmpAt),
) {
    for channel in members {
        out(message(Address { group, channel }, offset, voice));
    }
}

impl MpeIngest {
    /// An ingest configured for `mode` on every Group, holding no notes.
    pub fn new(mode: MpeMode) -> Self {
        Self {
            mode,
            groups: std::array::from_fn(|_| GroupState::new(mode)),
        }
    }

    /// The zone layout the host configured, which every Group starts in. A
    /// Group's channel messages are folded to per-note or passed through by
    /// `mode_of`.
    pub fn mode(&self) -> &MpeMode {
        &self.mode
    }

    /// The zone layout in force on `group`: the configured one until an MPE
    /// Configuration Message on that Group changed it.
    pub(crate) fn mode_of(&self, group: MidiGroup) -> &MpeMode {
        &self.groups[group.index()].mode
    }

    /// Reconfigure every Group to a new zone layout. Off-RT: call at wiring
    /// time, or via
    /// [`MidiIngestControls::set_mpe_mode`](crate::MidiIngestControls::set_mpe_mode),
    /// which defers adoption to the ingest node's next block.
    ///
    /// What a channel held or was told is kept where the channel is what it
    /// was, and forgotten where it entered or left MPE control. The sounding
    /// notes of those channels are the instrument's, and M1-100 section 2.2.3
    /// has the receiver stop them: `zone_defaults`,
    /// which the host sends downstream after this, carries the All Sound Off
    /// and Reset All Controllers that do.
    pub fn set_mode(&mut self, mode: MpeMode) {
        self.mode = mode;
        for group in &mut self.groups {
            group.entered = group.reconfigure(mode, group.profile);
        }
    }

    /// The MIDI-CI MPE Profile zones (M2-120) enabled on `group`, if any: the
    /// layout in force there until they are all disabled.
    pub fn profile(&self, group: MidiGroup) -> Option<&MpeZones> {
        self.groups[group.index()].profile.as_ref()
    }

    /// Takes `zones` as the MPE Profile zones of `group`: `Some` is the
    /// Profile's rules for those zones in place of MPE 1.1's, `None` is back
    /// to the layout the host and the Group's Configuration Messages left.
    /// Off-RT with the rest of the layout; the node calls it at a block's
    /// start.
    ///
    /// A receiver stops all sounding notes and resets the controls of each
    /// channel that enters or leaves MPE control (M2-120 section 3.3), so
    /// each channel the change moved gets All Sound Off and Reset All
    /// Controllers through `out` at `offset`; then the zones in force start at
    /// their bend ranges (section 3.5, 48 semitones for a zone built with
    /// the Profile default). A change that moves nothing sends nothing; the same zones
    /// again are no change.
    pub fn set_profile(
        &mut self,
        group: MidiGroup,
        zones: Option<MpeZones>,
        offset: Offset,
        mut out: impl FnMut(UmpAt),
    ) {
        let state = &mut self.groups[group.index()];
        if state.profile == zones {
            return;
        }
        let changed = state.reconfigure(state.mode, zones);
        state.entered = 0;
        send_stops(group, offset, changed, &mut out);
        for c in MidiChannel::all() {
            let Some(zone) = state.zone_of(c) else {
                continue;
            };
            if zone.role_of(c) == Role::Master {
                send_zone_defaults(&zone, group, offset, &mut out);
            }
        }
    }

    /// What a host-set mode change ([`set_mode`](Self::set_mode)) sends
    /// downstream for `group`, as events at `offset`: for each channel that
    /// entered or left MPE control, All Sound Off and Reset All Controllers
    /// (M1-100 section 2.2.3); then, for each zone, RPN #00/00 with the
    /// zone's master range on its master channel and RPN #00/07 with its
    /// member range on each member channel (±2 and ±48 for the defaults an MPE
    /// Configuration Message resets a receiver to), so a receiver that
    /// followed the old zones' ranges starts the new ones at theirs, as an MCM
    /// from the wire would have it. No ranges for
    /// [`MpeMode::Disabled`] or [`MpeMode::SingleChannelRotation`].
    pub(crate) fn zone_defaults(
        &self,
        group: MidiGroup,
        offset: Offset,
        mut out: impl FnMut(UmpAt),
    ) {
        send_stops(group, offset, self.groups[group.index()].entered, &mut out);
        let state = &self.groups[group.index()];
        if let MpeMode::Zones(zones) = state.mode {
            for config in zones.zones() {
                if let Some(zone) =
                    zone_view(&state.mode, None, &state.mono, config.master_channel())
                {
                    send_zone_defaults(&zone, group, offset, &mut out);
                }
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
    /// - a manager-channel pitch bend, bend sensitivity and zone-wide message
    ///   (`zone_wide`) pass, and are also sent on each member channel;
    /// - a member-channel bend sensitivity (RPN #00/00) becomes a per-note
    ///   bend sensitivity (RPN #00/07) on each of the zone's member channels;
    /// - an MPE Configuration Message is absorbed, the channels it takes into
    ///   or out of MPE control are stopped and reset (section 2.2.3), and the
    ///   zone it configures is reset to its default ranges
    ///   (`zone_defaults`);
    /// - a message the specification's receive table prohibits there is
    ///   dropped (`receive`);
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
        //
        // A Group with Profile zones has no Configuration Message: the Profile
        // replaces it (M2-120 section 2.2), so RPN 0x00/0x06 there is ordinary
        // parameter traffic too.
        let decoded = event.1.decode().ok();
        if let (Some((zone, members)), Some(Message::Midi2(address, _))) =
            (MpeZoneConfig::from_mcm(&event.1), decoded)
            && self.groups[address.group.index()].profile.is_none()
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
                let state = &self.groups[address.group.index()];
                if let Some(view) =
                    zone_view(&state.mode, None, &state.mono, configured.master_channel())
                {
                    send_zone_defaults(&view, address.group, event.0, out);
                }
            }
            return None;
        }

        let Some(Message::Midi2(address, voice)) = decoded else {
            // Not a channel-voice-2 message (System, SysEx, …) — pass through.
            return Some(*event);
        };
        let mode = *self.mode_of(address.group);
        let profile = self.groups[address.group.index()].profile.is_some();
        if matches!(mode, MpeMode::Disabled) && !profile {
            return Some(*event);
        }

        if let MpeMode::SingleChannelRotation { channel } = mode
            && !profile
        {
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

    /// A bend sensitivity (RPN #00/00) in a zone, as the messages it sets the
    /// bend ranges by, sent through `out`; whether the event is taken (`true`)
    /// or still goes on (`false`). Anything else sends nothing and goes on.
    ///
    /// The manager's is its own range and goes on; it is also each member's
    /// channel bend range (RPN #00/00 on each member), since the manager's bend
    /// is copied onto them. Under the Profile it sets the manager and every
    /// member alike (M2-120 section 3.5), so it is also a per-note bend
    /// sensitivity (RPN #00/07) on each member. Under MPE 1.1 a member's is
    /// the range of every member's own bends (M1-100 section 2.2.5, "the last
    /// Pitch Bend Sensitivity received on any Member Channel applies to all of
    /// them"), which the transform has made per-note: it is taken, and a
    /// per-note sensitivity on each member in its place. Under the Profile a
    /// member's is prohibited and ignored by the receive table.
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
        let Some(zone) = self.groups[address.group.index()].zone_of(address.channel) else {
            return false;
        };
        let role = zone.role_of(address.channel);
        if role == Role::Master {
            out(*event);
            send_rpns(
                address.group,
                event.0,
                zone.member_channels(),
                Rpn::PITCH_BEND_SENSITIVITY,
                value.get(),
                out,
            );
        }
        let per_note = match (zone.spec, role) {
            (MpeSpec::Mpe11, Role::Member) | (MpeSpec::Profile, Role::Master) => true,
            (MpeSpec::Mpe11, Role::Master) | (MpeSpec::Profile, Role::Member) => false,
        };
        if per_note {
            send_rpns(
                address.group,
                event.0,
                zone.member_channels(),
                Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
                value.get(),
                out,
            );
        }
        // The manager's went on above, ahead of its copies.
        role == Role::Master || zone.spec == MpeSpec::Mpe11
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
        let configured = MpeZoneConfig::new(
            zone,
            members,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        );
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
        let state = &mut self.groups[group.index()];
        let changed = state.reconfigure(mode, state.profile);
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
        let state = &mut self.groups[group.index()];
        // M1-100 section 2.2.1: a channel in no Zone is for conventional use.
        let Some(zone) = state.zone_of(ch) else {
            return Some(*event);
        };
        let role = zone.role_of(ch);
        if receive(zone.spec, role, &voice, zone.monophonic) == Receive::Ignore {
            return None;
        }
        let master = zone.manager;
        let slot = usize::from(ch.get());
        let manager = state.told[usize::from(master.get())];
        let at = event.0;
        if role == Role::Master && zone_wide(&voice) {
            // The Manager Channel's message is the whole zone's: it goes on,
            // and each member channel is told it too, so a receiver that reads
            // one channel at a time does the right thing.
            out(*event);
            to_members(group, zone.member_channels(), at, voice, out);
            if matches!(voice, Voice2::ControlChange { controller, .. } if controller == CCNumber::RESET_ALL)
            {
                // The reset centres every wheel of the zone, the copies included.
                for channel in core::iter::once(master).chain(zone.member_channels()) {
                    state.told[channel.index()].bend = None;
                }
            }
            return None;
        }
        match voice {
            // M2-104 7.4.2: a MIDI 2.0 Note On at velocity 0 is a Note On, and
            // binds its channel like any other (the translator before this
            // turns a MIDI 1.0 one into a Note Off).
            Voice2::NoteOn { note, .. } => {
                if role == Role::Member {
                    state.held.bind(ch, note);
                    // M1-100 section 2.2.6: the pitch of a new note is
                    // affected by the Pitch Bend most recently received on
                    // both the Manager Channel and that note's Member
                    // Channel before Note On, and sections 2.2.7 and
                    // 2.2.8 the same of Channel Pressure and Control Change
                    // #74. The note's Number is known only now, so what the
                    // channel was told goes ahead of the Note On, for the
                    // note it starts (the manager's bend is the instrument's
                    // own, summed with the member's).
                    let told = state.told[slot];
                    if let Some(value) = told.bend {
                        out(message(
                            address,
                            at,
                            Voice2::PerNotePitchBend {
                                note,
                                value: BendValue::new(value),
                            },
                        ));
                    }
                    if let Some(value) = combined(told.pressure, manager.pressure) {
                        out(message(
                            address,
                            at,
                            Voice2::PolyPressure {
                                note,
                                value: ControlValue::new(value),
                            },
                        ));
                    }
                    if let Some(value) = combined(told.brightness, manager.brightness) {
                        out(message(address, at, assignable(note, value)));
                    }
                }
                // The note itself always flows through unchanged.
                Some(*event)
            }
            Voice2::NoteOff { note, .. } => {
                // A released note ceases to be affected by Pitch Bend from the
                // Member Channels after its Note Off (M1-100 section 2.2.6):
                // the channel holds no note, and what it is
                // told from here on waits for the next one.
                if role == Role::Member && state.held.note_on(ch) == Some(note) {
                    state.held.release(ch);
                }
                Some(*event)
            }
            Voice2::PitchBend(value) => match role {
                // Master-channel bend is global (Appendix C.5): it goes on, and
                // each member channel carries it as its own channel bend, which
                // a receiver sums with the member's per-note bend (M2-104
                // section 7.4.13), and tracks with no note sounding.
                Role::Master => {
                    state.told[slot].bend = Some(value.get());
                    out(*event);
                    to_members(
                        group,
                        zone.member_channels(),
                        at,
                        Voice2::PitchBend(value),
                        out,
                    );
                    None
                }
                Role::Member => {
                    state.told[slot].bend = Some(value.get());
                    let note = state.held.note_on(ch)?;
                    Some(message(
                        address,
                        at,
                        Voice2::PerNotePitchBend { note, value },
                    ))
                }
            },
            Voice2::ChannelPressure(value) => {
                state.told[slot].pressure = Some(value.get());
                match role {
                    Role::Master => {
                        for member in zone.member_channels() {
                            if let Some(note) = state.held.note_on(member) {
                                let value = combined(
                                    state.told[usize::from(member.get())].pressure,
                                    Some(value.get()),
                                )
                                .unwrap_or(value.get());
                                let to = Address {
                                    group,
                                    channel: member,
                                };
                                out(message(
                                    to,
                                    at,
                                    Voice2::PolyPressure {
                                        note,
                                        value: ControlValue::new(value),
                                    },
                                ));
                            }
                        }
                        Some(*event)
                    }
                    Role::Member => {
                        let note = state.held.note_on(ch)?;
                        let value = combined(Some(value.get()), manager.pressure)?;
                        Some(message(
                            address,
                            at,
                            Voice2::PolyPressure {
                                note,
                                value: ControlValue::new(value),
                            },
                        ))
                    }
                }
            }
            Voice2::ControlChange { controller, value } if controller == CCNumber::BRIGHTNESS => {
                state.told[slot].brightness = Some(value.get());
                match role {
                    Role::Master => {
                        for member in zone.member_channels() {
                            if let Some(note) = state.held.note_on(member) {
                                let value = combined(
                                    state.told[usize::from(member.get())].brightness,
                                    Some(value.get()),
                                )
                                .unwrap_or(value.get());
                                let to = Address {
                                    group,
                                    channel: member,
                                };
                                out(message(to, at, assignable(note, value)));
                            }
                        }
                        Some(*event)
                    }
                    Role::Member => {
                        let note = state.held.note_on(ch)?;
                        let value = combined(Some(value.get()), manager.brightness)?;
                        // CC74 -> Assignable Per-Note Controller index 74 (the MPE slide).
                        Some(message(address, at, assignable(note, value)))
                    }
                }
            }
            Voice2::ControlChange { controller, .. }
                if role == Role::Member && controller == CCNumber::RESET_ALL =>
            {
                // The member's wheel is centred, and with it the copy of the
                // manager's bend it carries: the manager's wheel has not
                // moved, so it is told again after the reset.
                state.told[slot].bend = None;
                out(*event);
                if let Some(value) = manager.bend {
                    out(message(
                        address,
                        at,
                        Voice2::PitchBend(BendValue::new(value)),
                    ));
                }
                None
            }
            Voice2::ControlChange { controller, .. } => {
                // Section 2.2.4.3: a Mode message to the lowest numbered
                // Member Channel of a zone, not to the Manager Channel,
                // switches the zone between Mode 3 and Mode 4.
                if let Some(mono) = mode_switch(zone.spec, controller)
                    && zone.member_channels().next() == Some(ch)
                {
                    state.mono[usize::from(zone.manager.get())] = mono;
                }
                Some(*event)
            }
            // Everything else (native per-note messages, program change, ...)
            // passes through unchanged.
            _ => Some(*event),
        }
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

    /// [`reset`](Self::reset) for one Group's channels: a Group is a channel
    /// space of its own, so a source's reset of one cable leaves the held
    /// notes of the others mapped.
    pub fn reset_group(&mut self, group: MidiGroup) {
        let state = &mut self.groups[group.index()];
        state.held.clear();
        state.told = [Told::default(); MidiChannel::COUNT as usize];
    }
}

/// CC74 as an Assignable Per-Note Controller of `note`.
fn assignable(note: NoteNumber, value: u32) -> Voice2 {
    Voice2::AssignablePerNote {
        note,
        controller: CCNumber::BRIGHTNESS.get(),
        value: ControlValue::new(value),
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
            self.0[channel.index()] = Some(note);
        }

        /// Frees `channel`.
        pub(super) fn release(&mut self, channel: MidiChannel) {
            self.0[channel.index()] = None;
        }

        /// The note `channel` is playing, if any.
        pub(super) fn note_on(&self, channel: MidiChannel) -> Option<NoteNumber> {
            self.0[channel.index()]
        }

        /// Frees every channel.
        pub(super) fn clear(&mut self) {
            self.0 = [None; 16];
        }
    }
}

#[cfg(test)]
mod tests;
