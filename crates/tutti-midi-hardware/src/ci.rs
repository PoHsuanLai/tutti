//! MIDI-CI on a device's wire: the Management side of
//! [`CiDevice`] (Discovery, Invalidate MUID, NAK)
//! run for the life of an open input, and the peers it finds
//! ([`CiPeerInfo`], reported as [`CiChange`]).
//!
//! # Where it runs
//!
//! One [`CiDevice`] is one MIDI-CI Device on one Group, with its own MUID.
//! The session runs them for every open input whose device has an output (the
//! one the negotiation sends on):
//!
//! - **A UMP Endpoint that answered its Stream negotiation** gets one for each
//!   Function Block that declares MIDI-CI (`midi_ci_version() != 0`, M2-104
//!   §7.1.8), on that block's first Group, so a block is a MIDI-CI Device of
//!   its own with its own MUID (M2-101 §3.2.2). **A block that declares no
//!   MIDI-CI gets none**: it said it does not speak it. An Endpoint that
//!   declares no Function Block at all, and one that never answered the
//!   negotiation, get one on Group 0: with no block to say otherwise the
//!   Endpoint's Group 0 is "treated as a Function Block with only one member
//!   Group" (§3.2.2).
//! - **A MIDI 1.0 port** gets one on Group 0, the port's own, once the
//!   Identity Request ([`Negotiation`](crate::stream)) has ended, answered or
//!   given up, so the two never read the SysEx stream at once.
//!
//! # What it does
//!
//! It runs as long as the input does: the Discovery it sends when it starts
//! is only the first round, and it answers a Discovery or an Invalidate MUID
//! from anyone, at any time (M2-101 Appendix E: a Responder). It speaks
//! Management only ([`CiCategories::empty`]; no Profile, no Property
//! Exchange), so every other message addressed to it is NAK'd by the core.
//!
//! The port copies the device's SysEx7 packets to the stream ring while any
//! device runs ([`hears_sysex`](crate::stream)); the graph still gets them
//! all. They are reassembled per Group, decoded, and routed to that Group's
//! device. What a device sends goes out as SysEx7 on its Group, on the output
//! of the negotiation, never the session's MIDI output; a refused send is
//! logged and not repeated, since the protocol is built to tolerate loss.
//!
//! # What it declares
//!
//! - **Identity**: [`identity`]. tutti has no System Exclusive ID of its own,
//!   and MIDI 1.0 sets aside `0x7D`, the Non-Commercial / Research id
//!   ([`ManufacturerId::NON_COMMERCIAL`], M2-104 Table 20), for what is not a
//!   released product. Family, model and revision are zero.
//! - **Categories**: none.
//! - **Receivable Maximum SysEx Message Size**: [`MAX_SYSEX`], 512 bytes, the
//!   floor of a device that will initiate Profile Configuration or Property
//!   Exchange (§5.5.3); the reassembler is sized for it, delimiters included.
//! - **MUIDs** are drawn from std's OS-seeded hasher keys ([`entropy`]), which
//!   M2-101 Appendix A asks of a desktop OS's random numbers; no dependency.
//!
//! Time is an argument: nothing here reads a clock.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Instant;

use tutti_graph::Offset;
use tutti_midi_device::{
    CiAction, CiCategories, CiDevice, CiMessage, CiPeer, Decoded, MaxSysexSize, Muid,
};
use tutti_midi_runtime::MidiOut;
use tutti_ump::{DeviceIdentity, ManufacturerId, MidiGroup, SysexAssembler, SysexStep, U7, Ump};

use crate::devices::OpenOutput;
use crate::stream::DeviceStream;

/// The Receivable Maximum SysEx Message Size the session declares: 512 bytes
/// (see the module).
pub(crate) const MAX_SYSEX: MaxSysexSize = MaxSysexSize::of(MaxSysexSize::INITIATOR_MIN);

/// What the session's MIDI-CI Devices declare of themselves in Discovery: the
/// Non-Commercial / Research id (the module's "Identity") and zeros.
#[must_use]
pub(crate) fn identity() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::NON_COMMERCIAL,
        family: [U7::default(); 2],
        model: [U7::default(); 2],
        revision: [U7::default(); 4],
    }
}

/// A MIDI-CI peer found on a device: the Group it was found on, its MUID, and
/// what Discovery and the Endpoint Inquiry told of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiPeerInfo {
    group: MidiGroup,
    muid: Muid,
    peer: CiPeer,
}

impl CiPeerInfo {
    /// The Group of the connection it answered on: the Group of the
    /// MIDI-CI Device that found it.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// Its MUID.
    #[must_use]
    pub const fn muid(&self) -> Muid {
        self.muid
    }

    /// What it declared: identity, categories, receivable size, Function
    /// Block, and its Product Instance Id once it was asked.
    #[must_use]
    pub const fn peer(&self) -> &CiPeer {
        &self.peer
    }
}

/// A change of the peers found on a device, once each from
/// [`MidiSession::poll`](crate::MidiSession::poll).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiChange {
    /// A peer answered Discovery.
    Found(CiPeerInfo),
    /// What a found peer holds changed (its Product Instance Id arrived).
    Updated(CiPeerInfo),
    /// A peer is gone: it was invalidated, collided, or went with the
    /// device's own MUID, which a collision changes.
    Lost {
        /// The Group it was found on.
        group: MidiGroup,
        /// Its MUID.
        muid: Muid,
    },
}

/// The MUID source: std's OS-seeded hasher keys over a counter.
type Entropy = Box<dyn FnMut() -> u32 + Send>;

/// A source of words for MUIDs (the module's "MUIDs"). Each call of `entropy`
/// has keys of its own.
fn entropy() -> Entropy {
    let keys = RandomState::new();
    let mut count = 0u64;
    Box::new(move || {
        count += 1;
        let mut hasher = keys.build_hasher();
        hasher.write_u64(count);
        // The high half: the low 28 bits are the MUID's, and a hash mixes all.
        (hasher.finish() >> 32) as u32
    })
}

/// One MIDI-CI Device on its Group.
struct Member {
    group: MidiGroup,
    device: CiDevice<Entropy>,
}

/// The devices running on one input, and what hears their SysEx.
pub(crate) struct Running {
    members: Vec<Member>,
    assembler: SysexAssembler<Box<[u8]>>,
}

/// The state of one input's MIDI-CI.
pub(crate) enum Ci {
    /// None runs and none is to: no output, or a MIDI 1.0 port still being
    /// asked its identity.
    No,
    /// Decided, not started: the port copies SysEx from now on, and the next
    /// poll starts the devices, so no reply can arrive unheard.
    Idle(Vec<MidiGroup>),
    Running(Box<Running>),
}

/// Group 0, the Group of a MIDI 1.0 port and of an Endpoint that says nothing
/// of its blocks.
const GROUP_ZERO: MidiGroup = MidiGroup::of(0);

impl Ci {
    /// The devices a settled Stream negotiation calls for (the module's
    /// "Where it runs").
    pub(crate) fn after_settle(stream: &DeviceStream) -> Self {
        let endpoint = stream.endpoint();
        if stream.basis() != crate::Basis::Negotiated || endpoint.function_blocks().next().is_none()
        {
            return Self::Idle(vec![GROUP_ZERO]);
        }
        let mut groups: Vec<MidiGroup> = endpoint
            .function_blocks()
            .filter(|block| block.midi_ci_version() != 0)
            .filter_map(|block| MidiGroup::new(block.first_group().get()))
            .collect();
        groups.sort_unstable();
        groups.dedup();
        if groups.is_empty() {
            Self::No
        } else {
            Self::Idle(groups)
        }
    }

    /// The device of a MIDI 1.0 port.
    pub(crate) fn legacy() -> Self {
        Self::Idle(vec![GROUP_ZERO])
    }

    /// Whether the port should copy SysEx7 packets to the stream ring.
    pub(crate) const fn hears_sysex(&self) -> bool {
        !matches!(self, Self::No)
    }

    /// One step at `now`: starts the devices if they are waiting, else hands
    /// the running ones what the port heard (`incoming`, which also holds
    /// Stream messages and is read for its SysEx7 only) and the time. What
    /// they send goes out on `out`. Returns the peers' changes.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        incoming: &[Ump],
        out: &OpenOutput,
    ) -> Vec<CiChange> {
        match self {
            Self::No => Vec::new(),
            Self::Idle(groups) => {
                let (running, changes) = Running::start(groups, now, out);
                *self = Self::Running(Box::new(running));
                changes
            }
            Self::Running(running) => running.step(now, incoming, out),
        }
    }

    /// Every peer found, by Group then MUID.
    pub(crate) fn peers(&self) -> Vec<CiPeerInfo> {
        let Self::Running(running) = self else {
            return Vec::new();
        };
        running
            .members
            .iter()
            .flat_map(|member| {
                member.device.peers().map(|(muid, peer)| CiPeerInfo {
                    group: member.group,
                    muid,
                    peer: peer.clone(),
                })
            })
            .collect()
    }
}

impl Running {
    /// The devices of `groups`, each with its first Discovery sent on `out`.
    fn start(groups: &[MidiGroup], now: Instant, out: &OpenOutput) -> (Self, Vec<CiChange>) {
        let mut sends = Vec::new();
        let mut firsts = Vec::new();
        let members: Vec<Member> = groups
            .iter()
            .map(|&group| {
                let (device, first) =
                    CiDevice::new(identity(), CiCategories::empty(), MAX_SYSEX, entropy(), now);
                firsts.push(first);
                Member { group, device }
            })
            .collect();
        let max = MAX_SYSEX.get() as usize;
        let room = SysexAssembler::room_for(max, 1, members.len());
        let assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), max, members.len());
        let running = Self { members, assembler };
        let mut changes = Vec::new();
        for (member, actions) in running.members.iter().zip(firsts) {
            member.apply(actions, &mut sends, &mut changes);
        }
        send(out, &sends);
        (running, changes)
    }

    /// Hands the devices what the port heard and the time (see [`Ci::poll`]).
    fn step(&mut self, now: Instant, incoming: &[Ump], out: &OpenOutput) -> Vec<CiChange> {
        let mut sends = Vec::new();
        let mut changes = Vec::new();
        for &ump in incoming {
            // Only a message of a Group that has a device is read.
            let Some(group) = ump.group() else { continue };
            if ump.message_type() != 0x3 || !self.members.iter().any(|m| m.group == group) {
                continue;
            }
            self.assembler.begin_block();
            let SysexStep::Complete(done) = self.assembler.push(ump) else {
                continue;
            };
            let decoded = CiMessage::decode(self.assembler.message(done).payload());
            if matches!(decoded, Decoded::NotCi) {
                continue;
            }
            for member in self.members.iter_mut().filter(|m| m.group == group) {
                let actions = member.device.on_message(&decoded, now);
                member.apply(actions, &mut sends, &mut changes);
            }
        }
        for member in &mut self.members {
            if member.device.deadline().is_some_and(|at| now >= at) {
                let actions = member.device.on_timeout(now);
                member.apply(actions, &mut sends, &mut changes);
            }
        }
        send(out, &sends);
        changes
    }
}

impl Member {
    /// Carries out `actions`: what is to be sent goes in `sends`, what changed
    /// for the app in `changes`.
    fn apply(
        &self,
        actions: Vec<CiAction>,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut Vec<CiChange>,
    ) {
        let info = |muid| {
            self.device.peer(muid).map(|peer| CiPeerInfo {
                group: self.group,
                muid,
                peer: peer.clone(),
            })
        };
        for action in actions {
            match action {
                CiAction::Send(message) => sends.push((self.group, message)),
                CiAction::Found(muid) => changes.extend(info(muid).map(CiChange::Found)),
                CiAction::Updated(muid) => changes.extend(info(muid).map(CiChange::Updated)),
                CiAction::Lost(muid) => changes.push(CiChange::Lost {
                    group: self.group,
                    muid,
                }),
                // The device's own MUID changing reaches the app as every peer
                // lost; the window closing is the core's own bookkeeping.
                _ => {}
            }
        }
    }
}

/// Queues the messages as SysEx7 on `out`. A refusal is not retried: Discovery
/// and its replies are repeated by their protocol, and a lost one is a peer
/// found later or never.
fn send(out: &OpenOutput, sends: &[(MidiGroup, CiMessage)]) {
    for (group, message) in sends {
        let packets: Vec<_> = message
            .packets(*group)
            .map(|ump| (Offset::ZERO, ump))
            .collect();
        if out.queue(&packets) < packets.len() {
            tracing::debug!("a MIDI-CI message was refused by {}", out.name());
        }
    }
}
