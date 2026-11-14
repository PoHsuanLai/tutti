//! Commands: what a controller says to a controlled device (RP-013 pp.20 to 43).

use tutti_ump::U7;

use super::field::{GeneratorAction, MmcEventFlags, MmcSegment, MtcAction};
use super::values::{MmcFragment, MmcName, MmcPosition, MmcSelection, MmcSlot, MmcSpeed, MmcStep};
use super::{MmcCommands, MmcDevices, MmcFields, MmcNames};
use crate::universal::{Recipient, SysexDeviceId};

/// MASKED WRITE (`41`, RP-013 p.26): changes some bits of one byte of a bitmap
/// field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcMaskedWrite {
    /// The mask-writeable field.
    pub field: MmcName,
    /// The byte of the bitmap, 0 being the first after the count.
    pub byte: U7,
    /// A one marks a bit to change.
    pub mask: U7,
    /// The new bits.
    pub data: U7,
}

/// UPDATE (`43`, RP-013 pp.27 and 28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcUpdate {
    /// `00`: send the fields now, and again whenever they change.
    Begin(MmcNames),
    /// `01`: stop sending them; a `7F` anywhere in the list stops every update.
    End(MmcNames),
}

/// LOCATE (`44`, RP-013 pp.28 and 29).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcLocate {
    /// `00`: to the time in a general purpose register, `08` to `0F`.
    Field(MmcName),
    /// `01`: to the time given, which carries subframes.
    Target(MmcPosition),
}

/// ASSIGN SYSTEM MASTER (`49`, RP-013 p.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcSystemMaster {
    /// `7F`: no device is the master.
    Nobody,
    /// The device that is.
    Device(SysexDeviceId),
}

/// An arithmetic command's fields: `destination` takes the result of the
/// sources (RP-013 pp.32 and 33).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcArithmetic {
    /// Where the result goes.
    pub destination: MmcName,
    /// The first source.
    pub first: MmcName,
    /// The second source.
    pub second: MmcName,
}

/// MOVE (`4C`, RP-013 p.32): copies one time code field to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcMove {
    /// Where it goes.
    pub destination: MmcName,
    /// Where it comes from.
    pub source: MmcName,
}

/// PROCEDURE (`50`, RP-013 pp.34 and 35).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcProcedure {
    /// `00`: store these commands under the name.
    Assemble {
        /// The name.
        procedure: MmcSlot,
        /// The commands.
        commands: MmcCommands,
    },
    /// `01`: delete one or all.
    Delete(MmcSelection),
    /// `02`: choose which PROCEDURE RESPONSE reads back.
    Set(MmcSelection),
    /// `03`: run it now.
    Execute(MmcSlot),
}

/// EVENT (`51`, RP-013 pp.36 to 38).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcEvent {
    /// `00`: run a command when a time code stream reaches a time.
    Define {
        /// The event's name.
        event: MmcSlot,
        /// When it triggers.
        flags: MmcEventFlags,
        /// The stream: `01` selected time code, `02` selected master code, `06`
        /// generator time code, `07` MIDI time code input.
        trigger_source: MmcName,
        /// The register holding the trigger time, `08` to `0F`.
        time: MmcName,
        /// The one command it runs.
        command: MmcCommands,
    },
    /// `01`: delete one or all.
    Delete(MmcSelection),
    /// `02`: choose which EVENT RESPONSE reads back.
    Set(MmcSelection),
    /// `03`: run the event's command now.
    Test(MmcSlot),
}

/// GROUP (`52`, RP-013 p.39). A group number is a device ID that no device has
/// as its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcGroup {
    /// `00`: the listed devices join the group.
    Assign {
        /// The group's number.
        group: SysexDeviceId,
        /// The devices; `7F` in the list is the all-call.
        devices: MmcDevices,
    },
    /// `01`: the listed devices leave the group, or every group.
    DisAssign {
        /// The group, `7F` for all of them.
        group: Recipient,
        /// The devices; a `7F` in the list is every device.
        devices: MmcDevices,
    },
}

/// A command whose code the specification leaves undefined, skipped by the
/// length its code gives (RP-013 p.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcReservedCommand {
    /// Its code.
    pub name: MmcName,
    /// Its data, without the count byte.
    pub data: MmcFragment,
}

/// One Machine Control command (RP-013 pp.15 and 20 to 43).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcCommand {
    /// `01`: stop as soon as possible.
    Stop,
    /// `02`: play.
    Play,
    /// `03`: play, after a locate in progress completes.
    DeferredPlay,
    /// `04`.
    FastForward,
    /// `05`.
    Rewind,
    /// `06`: enter or leave record.
    RecordStrobe,
    /// `07`.
    RecordExit,
    /// `08`.
    RecordPause,
    /// `09`: stop, ready to start quickly.
    Pause,
    /// `0A`: eject the media.
    Eject,
    /// `0B`: follow the selected master code.
    Chase,
    /// `0C`.
    CommandErrorReset,
    /// `0D`.
    MmcReset,
    /// `40`: load the fields.
    Write(MmcFields),
    /// `41`.
    MaskedWrite(MmcMaskedWrite),
    /// `42`: send these fields.
    Read(MmcNames),
    /// `43`.
    Update(MmcUpdate),
    /// `44`.
    Locate(MmcLocate),
    /// `45`: play at a speed.
    VariablePlay(MmcSpeed),
    /// `46`: move at a speed with monitoring.
    Search(MmcSpeed),
    /// `47`: move at a speed.
    Shuttle(MmcSpeed),
    /// `48`: move by step lengths.
    Step(MmcStep),
    /// `49`.
    AssignSystemMaster(MmcSystemMaster),
    /// `4A`.
    GeneratorCommand(GeneratorAction),
    /// `4B`: MIDI TIME CODE COMMAND.
    MtcCommand(MtcAction),
    /// `4C`.
    Move(MmcMove),
    /// `4D`.
    Add(MmcArithmetic),
    /// `4E`.
    Subtract(MmcArithmetic),
    /// `4F`: convert the field to drop frame.
    DropFrameAdjust(MmcName),
    /// `50`.
    Procedure(MmcProcedure),
    /// `51`.
    Event(MmcEvent),
    /// `52`.
    Group(MmcGroup),
    /// `53`: a piece of a command string too long for one message.
    CommandSegment(MmcSegment),
    /// `54`: variable play, after a locate in progress completes.
    DeferredVariablePlay(MmcSpeed),
    /// `55`.
    RecordStrobeVariable(MmcSpeed),
    /// `7C`: the controller asks for responses to be held.
    Wait,
    /// `7F`: the controller is ready for responses again.
    Resume,
    /// A command the specification leaves undefined.
    Reserved(MmcReservedCommand),
}
