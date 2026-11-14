//! [`MachineControlFollower`]: what a transport that is controlled by MIDI
//! Machine Control does, as a pure state machine.
//!
//! One follower reads one source's UMPs and says what the commands addressed to
//! this device ask of a transport ([`MachineControlEvent`]). It holds no clock
//! and no transport; whoever owns the transport places each event (the session,
//! in `tutti-session`).
//!
//! # Addressing
//!
//! A command is read when its device ID is the all-call (`7F`), this device's
//! own ID, or a group this device has been assigned to by GROUP (RP-013 pp.5 and
//! 39). MMC RESET clears the groups (RP-013 p.25).
//!
//! # What is done
//!
//! - **STOP** and **PAUSE** stop the transport.
//! - **PLAY** and **DEFERRED PLAY** play. A locate is instant here, so a
//!   locate in progress never delays a deferred play.
//! - **LOCATE (target)** locates to the time; **LOCATE (register)** to the time a WRITE
//!   put in the general purpose register it names (`08` to `0F`). A time is
//!   read from `00:00:00:00` with drop-frame labels counted as SMPTE counts them
//!   and the subframes added.
//! - **MOVE** between general purpose registers copies the time.
//! - **VARIABLE PLAY** and **DEFERRED VARIABLE PLAY** at exactly play speed play.
//! - **GROUP** and **MMC RESET** act on the follower itself.
//! - WAIT and RESUME are handshakes for a device that answers; this one
//!   does not, and they change nothing.
//!
//! Every other command is reported as [`MachineControlEvent::Ignored`]: the
//! transport has no record, no shuttle and no tape to eject, and the follower
//! answers no READ.

use tutti_midi_protocol::{
    Addressed, MmcCommand, MmcFields, MmcGroup, MmcInfoField, MmcLocate, MmcMove, MmcPosition,
    MmcSign, MmcSpeed, Read, Recipient, SysexDeviceId, Universal, read,
};
use tutti_types::Seconds;
use tutti_ump::{SysexAssembler, SysexStep, Ump};

/// What a Machine Control source asks of the transport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MachineControlEvent {
    /// Roll from where the transport stands.
    Play,
    /// Stop where the transport stands.
    Stop,
    /// Jump to `time` since `00:00:00:00`, keeping whatever the transport does
    /// after the locate.
    Locate(Seconds),
    /// A command this follower does not act on.
    Ignored(MmcCommand),
}

/// Longest message read: 3 header bytes and 48 of data.
const SYSEX_MAX: usize = 56;
const SYSEX_ROOM: usize = 2 * SYSEX_MAX + 8;

/// The seconds a time code position is at: the frames since `00:00:00:00`
/// and the subframes' hundredths of the next.
fn seconds_of(position: MmcPosition) -> Seconds {
    let timecode = position.label.time_code().timecode();
    let subframes = f64::from(position.subframes.get()) / 100.0;
    let seconds = timecode.to_seconds().get() + subframes / timecode.frame_rate().fps();
    Seconds::new(match position.label.sign() {
        MmcSign::Positive => seconds,
        MmcSign::Negative => -seconds,
    })
}

/// Follows Machine Control commands; the rules are in the module docs.
///
/// ```
/// use tutti_midi_runtime::{MachineControlEvent, MachineControlFollower};
/// use tutti_midi_protocol::SysexDeviceId;
/// use tutti_ump::{MidiGroup, Sysex7Packets};
///
/// let mut follower = MachineControlFollower::new(SysexDeviceId::of(1));
/// // F0 7F 01 06 02 F7: PLAY to device 1.
/// let mut heard = Vec::new();
/// for packet in Sysex7Packets::from_payload(MidiGroup::FIRST, &[0x7F, 0x01, 0x06, 0x02]).unwrap() {
///     follower.step(packet, &mut |e| heard.push(e));
/// }
/// assert_eq!(heard, [MachineControlEvent::Play]);
/// ```
#[derive(Debug)]
pub struct MachineControlFollower {
    device: SysexDeviceId,
    /// The groups this device is assigned to, a bit a device ID.
    groups: u128,
    /// The general purpose registers `08` to `0F`.
    registers: [Option<MmcPosition>; 8],
    sysex: SysexAssembler<[u8; SYSEX_ROOM]>,
}

impl MachineControlFollower {
    /// A follower that answers to `device`, and to the all-call, in no group.
    #[must_use]
    pub fn new(device: SysexDeviceId) -> Self {
        Self {
            device,
            groups: 0,
            registers: [None; 8],
            sysex: SysexAssembler::new([0; SYSEX_ROOM], SYSEX_MAX, 1),
        }
    }

    /// The device ID the follower answers to besides the all-call.
    #[must_use]
    pub fn device(&self) -> SysexDeviceId {
        self.device
    }

    /// Whether the follower is in `group`.
    #[must_use]
    pub fn in_group(&self, group: SysexDeviceId) -> bool {
        self.groups >> group.get().get() & 1 != 0
    }

    /// Whether a command to `to` is for this device: the all-call, its own ID
    /// or one of its groups.
    fn is_for_us(&self, to: Recipient) -> bool {
        match to {
            Recipient::AllCall => true,
            Recipient::Device(id) => id == self.device || self.in_group(id),
        }
    }

    /// Forgets the groups and registers, as MMC RESET does.
    pub fn reset(&mut self) {
        self.groups = 0;
        self.registers = [None; 8];
        self.sysex.reset();
    }

    /// Takes in one UMP of the source's stream and passes each event it makes
    /// to `out`, in order. Allocates nothing.
    pub fn step(&mut self, ump: Ump, out: &mut impl FnMut(MachineControlEvent)) {
        self.sysex.begin_block();
        let SysexStep::Complete(done) = self.sysex.push(ump) else {
            return;
        };
        let Read::Universal(Addressed {
            to,
            message: Universal::MachineControl(commands),
        }) = read(self.sysex.message(done))
        else {
            return;
        };
        if !self.is_for_us(to) {
            return;
        }
        for command in &commands {
            self.act(command, out);
        }
    }

    fn act(&mut self, command: MmcCommand, out: &mut impl FnMut(MachineControlEvent)) {
        match command {
            MmcCommand::Stop | MmcCommand::Pause => out(MachineControlEvent::Stop),
            MmcCommand::Play | MmcCommand::DeferredPlay => out(MachineControlEvent::Play),
            MmcCommand::VariablePlay(speed) | MmcCommand::DeferredVariablePlay(speed)
                if speed == MmcSpeed::PLAY =>
            {
                out(MachineControlEvent::Play);
            }
            MmcCommand::Locate(MmcLocate::Target(position)) => {
                out(MachineControlEvent::Locate(seconds_of(position)));
            }
            MmcCommand::Locate(MmcLocate::Field(name)) => {
                match register(name).and_then(|n| self.registers[n]) {
                    Some(position) => {
                        out(MachineControlEvent::Locate(seconds_of(position)));
                    }
                    None => out(MachineControlEvent::Ignored(command)),
                }
            }
            MmcCommand::Write(fields) => self.write(&fields),
            MmcCommand::Move(MmcMove {
                destination,
                source,
            }) => match (register(destination), register(source)) {
                (Some(to), Some(from)) => self.registers[to] = self.registers[from],
                _ => out(MachineControlEvent::Ignored(command)),
            },
            MmcCommand::Group(MmcGroup::Assign { group, devices }) => {
                if devices.iter().any(|d| d == Recipient::Device(self.device)) {
                    self.groups |= 1 << group.get().get();
                }
            }
            MmcCommand::Group(MmcGroup::DisAssign { group, devices }) => {
                if devices
                    .iter()
                    .any(|d| d == Recipient::AllCall || d == Recipient::Device(self.device))
                {
                    match group {
                        Recipient::AllCall => self.groups = 0,
                        Recipient::Device(group) => self.groups &= !(1 << group.get().get()),
                    }
                }
            }
            MmcCommand::MmcReset => {
                self.groups = 0;
                self.registers = [None; 8];
            }
            MmcCommand::Wait | MmcCommand::Resume => {}
            MmcCommand::FastForward
            | MmcCommand::Rewind
            | MmcCommand::RecordStrobe
            | MmcCommand::RecordExit
            | MmcCommand::RecordPause
            | MmcCommand::Eject
            | MmcCommand::Chase
            | MmcCommand::CommandErrorReset
            | MmcCommand::MaskedWrite(_)
            | MmcCommand::Read(_)
            | MmcCommand::Update(_)
            | MmcCommand::VariablePlay(_)
            | MmcCommand::Search(_)
            | MmcCommand::Shuttle(_)
            | MmcCommand::Step(_)
            | MmcCommand::AssignSystemMaster(_)
            | MmcCommand::GeneratorCommand(_)
            | MmcCommand::MtcCommand(_)
            | MmcCommand::Add(_)
            | MmcCommand::Subtract(_)
            | MmcCommand::DropFrameAdjust(_)
            | MmcCommand::Procedure(_)
            | MmcCommand::Event(_)
            | MmcCommand::CommandSegment(_)
            | MmcCommand::DeferredVariablePlay(_)
            | MmcCommand::RecordStrobeVariable(_)
            | MmcCommand::Reserved(_) => out(MachineControlEvent::Ignored(command)),
        }
    }

    /// A WRITE: the general purpose registers take their time; the other
    /// fields are state this follower does not keep.
    fn write(&mut self, fields: &MmcFields) {
        for field in fields {
            if let MmcInfoField::TimeCode(name, code) = field
                && let Some(n) = register(name.into())
                && let Ok(position) = MmcPosition::try_from(code)
            {
                self.registers[n] = Some(position);
            }
        }
    }
}

/// The general purpose register `08` to `0F` a name is, as an index.
fn register(name: tutti_midi_protocol::MmcName) -> Option<usize> {
    let code = name.code().get();
    (name.extension() == tutti_midi_protocol::MmcExtension::Base && (0x08..=0x0F).contains(&code))
        .then(|| usize::from(code - 0x08))
}
