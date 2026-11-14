//! The Machine Control grammar, on top of the Universal one: the bytes after
//! `06` (commands) or `07` (responses) as [`MmcCommands`] and [`MmcFields`].
//!
//! ```text
//! commands   := 06 command+                          -- to a device, group or all
//! responses  := 07 field+                            -- from one device
//! command    := code0 | 40..77 count data | 78..7F   -- 00 prefixes an extension
//! field      := 01..0F time5 | 21..2F time2 | 40..77 count data | 78..7F
//! name       := nz | 00 nz | 00 00 nz                -- nz is 01..7F
//! time5      := hr mn sc fr tail                     -- tail: subframes or status
//! speed      := sh sm sl
//! ```
//!
//! A message is at most 48 bytes after the sub-ID (RP-013 p.2). A command or
//! field the specification leaves undefined is kept as reserved, its length
//! given by its code (RP-013 p.6); anything that is not a whole number of
//! well-formed messages does not match.

use winnow::Parser;
use winnow::error::{ContextError, ErrMode};
use winnow::token::{any, take};

use tutti_ump::{U4, U7};

use super::command::{
    MmcArithmetic, MmcCommand, MmcEvent, MmcGroup, MmcLocate, MmcMaskedWrite, MmcMove,
    MmcProcedure, MmcReservedCommand, MmcSystemMaster, MmcUpdate,
};
use super::field::*;
use super::values::{
    MmcExtension, MmcFragment, MmcName, MmcPosition, MmcSelection, MmcShortTimeCode, MmcSlot,
    MmcSpeed, MmcStep, MmcTimeCode, MmcTimeLabel, MmcTimeTail, MmcTrackBitmap, MmcUserBitFlags,
    MmcUserBits, sign_of,
};
use super::{MMC_MAX_DATA, MmcBytes, MmcCommands, MmcFields, MmcNames, MmcSequence};
use crate::smpte::MtcRate;
use crate::universal::{Recipient, SysexDeviceId};

pub(crate) type Res<T> = winnow::ModalResult<T, ContextError>;

fn bad<T>() -> Res<T> {
    Err(ErrMode::Backtrack(ContextError::new()))
}

fn byte(input: &mut &[u8]) -> Res<u8> {
    any.parse_next(input)
}

fn u7(input: &mut &[u8]) -> Res<U7> {
    any.verify_map(U7::new).parse_next(input)
}

/// A name of one to three bytes (RP-013 p.5).
pub(crate) fn name(input: &mut &[u8]) -> Res<MmcName> {
    for extension in [
        MmcExtension::Base,
        MmcExtension::First,
        MmcExtension::Second,
    ] {
        let byte = u7.parse_next(input)?;
        if let Some(name) = MmcName::new(extension, byte) {
            return Ok(name);
        }
    }
    bad()
}

pub(crate) fn recipient(input: &mut &[u8]) -> Res<Recipient> {
    u7.map(Recipient::of_byte).parse_next(input)
}

/// A slice parsed whole: `parser` must use every byte.
fn within<'a, T>(bytes: &'a [u8], mut parser: impl FnMut(&mut &'a [u8]) -> Res<T>) -> Option<T> {
    let mut rest = bytes;
    let value = parser(&mut rest).ok()?;
    rest.is_empty().then_some(value)
}

/// The data bytes of one message, `count` and then that many.
fn counted<'a>(input: &mut &'a [u8]) -> Res<&'a [u8]> {
    let count = u7.parse_next(input)?;
    take(usize::from(count.get())).parse_next(input)
}

/// How a code's data is laid out (RP-013 p.6).
enum Shape {
    /// Nothing follows.
    None,
    /// Five bytes of standard time code.
    Time5,
    /// Two bytes of short time code.
    Time2,
    /// A count and the data.
    Counted,
}

fn shape(code: u8) -> Shape {
    match code {
        0x01..=0x1F => Shape::Time5,
        0x20..=0x3F => Shape::Time2,
        0x40..=0x77 => Shape::Counted,
        _ => Shape::None,
    }
}

/// A sequence of `T` over the whole of `bytes`.
pub(crate) fn sequence<T: super::sealed::Sealed>(bytes: &[u8]) -> Option<MmcSequence<T>> {
    let mut rest = bytes;
    while !rest.is_empty() {
        T::parse(&mut rest).ok()?;
    }
    Some(MmcSequence::from_bytes(MmcBytes::from_slice(bytes)?))
}

fn fragment(bytes: &[u8]) -> Option<MmcFragment> {
    MmcFragment::new(bytes)
}

// --- time code, speed, user bits ---

fn time_code_bytes(bytes: &[u8]) -> Option<MmcTimeCode> {
    let [hr, mn, sc, fr, last] = *bytes else {
        return None;
    };
    let (label, status) = MmcTimeLabel::from_head(hr, mn, sc, fr)?;
    Some(MmcTimeCode {
        label,
        tail: MmcTimeTail::from_wire(status, last)?,
    })
}

fn position(input: &mut &[u8]) -> Res<MmcPosition> {
    let bytes = take(5usize).parse_next(input)?;
    time_code_bytes(bytes)
        .and_then(|code| MmcPosition::try_from(code).ok())
        .map_or_else(bad, Ok)
}

fn short_time_code(bytes: &[u8]) -> Option<MmcShortTimeCode> {
    let [fr, last] = *bytes else {
        return None;
    };
    if fr >= 0x80 {
        return None;
    }
    let tail = MmcTimeTail::from_wire(fr & 0x20 != 0, last)?;
    MmcShortTimeCode::new(fr & 0x1F, sign_of(fr), tail)
}

fn speed(bytes: &[u8]) -> Option<MmcSpeed> {
    let [sh, sm, sl] = *bytes else {
        return None;
    };
    MmcSpeed::from_wire(sh, sm, sl)
}

fn user_bits(bytes: &[u8]) -> Option<MmcUserBits> {
    let [a, b, c, d, e, f, g, h, flags] = *bytes else {
        return None;
    };
    if flags >= 0x08 {
        return None;
    }
    let nibble = |n: u8| (n < 0x10).then(|| U4::masked(n));
    Some(MmcUserBits {
        groups: [
            nibble(a)?,
            nibble(b)?,
            nibble(c)?,
            nibble(d)?,
            nibble(e)?,
            nibble(f)?,
            nibble(g)?,
            nibble(h)?,
        ],
        flags: MmcUserBitFlags {
            secondary_time_code: flags & 4 != 0,
            group_flag_1: flags & 2 != 0,
            group_flag_0: flags & 1 != 0,
        },
    })
}

fn one<T>(bytes: &[u8], from_wire: fn(u8) -> Option<T>) -> Option<T> {
    match *bytes {
        [byte] => from_wire(byte),
        _ => None,
    }
}

fn one_u7(bytes: &[u8]) -> Option<U7> {
    match *bytes {
        [byte] => U7::new(byte),
        _ => None,
    }
}

fn slot(byte: u8) -> Option<MmcSlot> {
    MmcSlot::new(U7::new(byte)?)
}

fn selection(byte: u8) -> Option<MmcSelection> {
    if byte == 0x7F {
        Some(MmcSelection::All)
    } else {
        slot(byte).map(MmcSelection::One)
    }
}

fn event_flags(byte: u8) -> Option<MmcEventFlags> {
    if byte & !0b0101_0011 != 0 {
        return None;
    }
    Some(MmcEventFlags {
        direction: MmcEventDirection::from_wire(byte & 0x03)?,
        all_speeds: byte & 0x10 != 0,
        keep: byte & 0x40 != 0,
    })
}

// --- information fields ---

fn signature(d: &[u8]) -> Option<MmcSignature> {
    let [major, minor, 0, 0, rest @ ..] = d else {
        return None;
    };
    let (&n1, rest) = rest.split_first()?;
    let (commands, rest) = rest.split_at_checked(usize::from(n1))?;
    let (&n2, rest) = rest.split_first()?;
    (rest.len() == usize::from(n2)).then_some(())?;
    Some(MmcSignature {
        major: U7::new(*major)?,
        minor: U7::new(*minor)?,
        commands: MmcSupportBitmap::from_bytes(commands)?,
        fields: MmcSupportBitmap::from_bytes(rest)?,
    })
}

fn command_error(d: &[u8]) -> Option<MmcErrorReport> {
    within(d, |input: &mut &[u8]| {
        let flags = byte(input)?;
        if flags & 0b0100_1000 != 0 || flags >= 0x80 {
            return bad();
        }
        let level = u7.parse_next(input)?;
        let error = match name.parse_next(input)? {
            code if code.extension() != MmcExtension::Base => MmcErrorCode::Extended(code),
            code => match MmcErrorKind::from_wire(code.code().get()) {
                Some(kind) => MmcErrorCode::Kind(kind),
                None => return bad(),
            },
        };
        let count = u7.parse_next(input)?.get();
        let command = if count == 0 {
            None
        } else {
            let offset = u7.parse_next(input)?;
            let rest = take(usize::from(count) - 1).parse_next(input)?;
            Some(MmcErrorCommand {
                offset,
                command: fragment(rest).map_or_else(bad, Ok)?,
            })
        };
        Ok(MmcErrorReport {
            flags: MmcErrorFlags {
                halted: flags & 1 != 0,
                procedure_error: flags & 2 != 0,
                event_error: flags & 4 != 0,
                unsolicited: flags & 0x10 != 0,
                sent_before: flags & 0x20 != 0,
            },
            level,
            error,
            command,
        })
    })
}

fn motion_tally(d: &[u8]) -> Option<MotionControlTally> {
    let [state, process, status] = *d else {
        return None;
    };
    if status & 0b1000_1000 != 0 {
        return None;
    }
    Some(MotionControlTally {
        state: MotionState::from_wire(state)?,
        process: MotionProcess::from_wire(process)?,
        state_success: McsSuccess::from_wire(status & 7)?,
        process_success: McpSuccess::from_wire(status >> 4 & 7)?,
    })
}

fn record_status(d: &[u8]) -> Option<MmcRecordStatus> {
    let [byte] = *d else {
        return None;
    };
    if byte >= 0x80 {
        return None;
    }
    Some(MmcRecordStatus {
        activity: MmcRecordActivity::from_wire(byte & 0x0F)?,
        record_inhibited: byte & 0x10 != 0,
        rehearse_inhibited: byte & 0x20 != 0,
        no_tracks_active: byte & 0x40 != 0,
    })
}

fn generator_tally(d: &[u8]) -> Option<GeneratorTally> {
    let [command, status] = *d else {
        return None;
    };
    if status & !0b0011_0111 != 0 {
        return None;
    }
    Some(GeneratorTally {
        command: GeneratorAction::from_wire(command)?,
        frame_sync_lost: status & 0x20 != 0,
        source_lost: status & 0x10 != 0,
        success: MmcCommandSuccess::from_wire(status & 7)?,
    })
}

fn mtc_tally(d: &[u8]) -> Option<MtcTally> {
    let [command, status] = *d else {
        return None;
    };
    Some(MtcTally {
        command: MtcAction::from_wire(command)?,
        success: MmcCommandSuccess::from_wire(status)?,
    })
}

fn generator_set_up(d: &[u8]) -> Option<GeneratorSetUp> {
    within(d, |input: &mut &[u8]| {
        let reference = byte(input)?;
        let source = name.parse_next(input)?;
        let mode = byte(input)?;
        let run = RunReference::from_wire(reference & 0x07);
        let jam = JamReference::from_wire(reference >> 4 & 0x07);
        match (reference & 0b1000_1000, run, jam, JamMode::from_wire(mode)) {
            (0, Some(run), Some(jam), Some(mode)) => Ok(GeneratorSetUp {
                run,
                jam,
                source,
                mode,
            }),
            _ => bad(),
        }
    })
}

fn mtc_set_up(d: &[u8]) -> Option<MtcSetUp> {
    within(d, |input: &mut &[u8]| {
        let flags = byte(input)?;
        if flags >= 0x40 {
            return bad();
        }
        Ok(MtcSetUp {
            flags: MtcFlags {
                send_while_stopped: flags & 1 != 0,
                full_messages_stopped: flags & 2 != 0,
                send_while_fast: flags & 4 != 0,
                full_messages_fast: flags & 8 != 0,
                send_user_bits: flags & 0x10 != 0,
                mute_response_cable: flags & 0x20 != 0,
            },
            source: name.parse_next(input)?,
        })
    })
}

fn vitc(d: &[u8]) -> Option<VitcInsert> {
    let [control, first_line, second_line] = *d else {
        return None;
    };
    Some(VitcInsert {
        control: U7::new(control)?,
        first_line: U7::new(first_line)?,
        second_line: U7::new(second_line)?,
    })
}

fn segment(d: &[u8]) -> Option<MmcSegment> {
    let (&id, rest) = d.split_first()?;
    if id >= 0x80 {
        return None;
    }
    MmcSegment::new(
        if id & 0x40 != 0 {
            MmcSegmentPlace::First
        } else {
            MmcSegmentPlace::Later
        },
        id & 0x3F,
        fragment(rest)?,
    )
}

fn procedure_response(d: &[u8]) -> Option<MmcProcedureResponse> {
    if *d == [0x7F] {
        return Some(MmcProcedureResponse::Invalid);
    }
    let (&first, rest) = d.split_first()?;
    Some(MmcProcedureResponse::Procedure {
        procedure: slot(first)?,
        commands: sequence(rest)?,
    })
}

fn one_command(commands: MmcCommands) -> Option<MmcCommands> {
    let mut items = commands.iter();
    items.next()?;
    items.next().is_none().then_some(commands)
}

fn event_response(d: &[u8]) -> Option<MmcEventResponse> {
    if *d == [0x7F] {
        return Some(MmcEventResponse::Invalid);
    }
    let mut input = d;
    let event = slot(byte(&mut input).ok()?)?;
    let flags = event_flags(byte(&mut input).ok()?)?;
    let trigger_source = name.parse_next(&mut input).ok()?;
    let time = position.parse_next(&mut input).ok()?;
    Some(MmcEventResponse::Event {
        event,
        flags,
        trigger_source,
        time,
        command: one_command(sequence(input)?)?,
    })
}

/// The data of the counted field `code`, typed.
fn field_data(code: u8, d: &[u8]) -> Option<MmcInfoField> {
    let tracks = |d: &[u8]| MmcTrackBitmap::from_bytes(d);
    Some(match code {
        0x40 => MmcInfoField::Signature(signature(d)?),
        0x41 => MmcInfoField::UpdateRate(one_u7(d)?),
        0x42 => MmcInfoField::ResponseError(sequence(d)?),
        0x43 => MmcInfoField::CommandError(command_error(d)?),
        0x44 => MmcInfoField::CommandErrorLevel(one_u7(d)?),
        0x45 => MmcInfoField::TimeStandard(match *d {
            [byte] if byte & 0x9F == 0 => MtcRate::masked(byte >> 5),
            _ => return None,
        }),
        0x46 => MmcInfoField::TimeCodeSource(one(d, MmcTimeCodeSource::from_wire)?),
        0x47 => MmcInfoField::TimeCodeUserBits(user_bits(d)?),
        0x48 => MmcInfoField::MotionControlTally(motion_tally(d)?),
        0x49 => MmcInfoField::VelocityTally(speed(d)?),
        0x4A => MmcInfoField::StopMode(one(d, MmcStopMode::from_wire)?),
        0x4B => MmcInfoField::FastMode(one(d, MmcFastMode::from_wire)?),
        0x4C => MmcInfoField::RecordMode(one(d, MmcRecordMode::from_wire)?),
        0x4D => MmcInfoField::RecordStatus(record_status(d)?),
        0x4E => MmcInfoField::TrackRecordStatus(tracks(d)?),
        0x4F => MmcInfoField::TrackRecordReady(tracks(d)?),
        0x50 => MmcInfoField::GlobalMonitor(one(d, MmcGlobalMonitor::from_wire)?),
        0x51 => MmcInfoField::RecordMonitor(one(d, MmcRecordMonitor::from_wire)?),
        0x52 => MmcInfoField::TrackSyncMonitor(tracks(d)?),
        0x53 => MmcInfoField::TrackInputMonitor(tracks(d)?),
        0x54 => MmcInfoField::StepLength(one_u7(d)?),
        0x55 => MmcInfoField::PlaySpeedReference(one(d, MmcPlaySpeedReference::from_wire)?),
        0x56 => MmcInfoField::FixedSpeed(one_u7(d)?),
        0x57 => MmcInfoField::LifterDefeat(one(d, MmcLifterDefeat::from_wire)?),
        0x58 => MmcInfoField::ControlDisable(one(d, MmcControlDisable::from_wire)?),
        0x59 => MmcInfoField::ResolvedPlayMode(one(d, MmcResolvedPlayMode::from_wire)?),
        0x5A => MmcInfoField::ChaseMode(one(d, MmcChaseMode::from_wire)?),
        0x5B => MmcInfoField::GeneratorCommandTally(generator_tally(d)?),
        0x5C => MmcInfoField::GeneratorSetUp(generator_set_up(d)?),
        0x5D => MmcInfoField::GeneratorUserBits(user_bits(d)?),
        0x5E => MmcInfoField::MtcCommandTally(mtc_tally(d)?),
        0x5F => MmcInfoField::MtcSetUp(mtc_set_up(d)?),
        0x60 => MmcInfoField::ProcedureResponse(procedure_response(d)?),
        0x61 => MmcInfoField::EventResponse(event_response(d)?),
        0x62 => MmcInfoField::TrackMute(tracks(d)?),
        0x63 => MmcInfoField::VitcInsertEnable(vitc(d)?),
        0x64 => MmcInfoField::ResponseSegment(segment(d)?),
        0x65 => MmcInfoField::Failure(fragment(d)?),
        _ => return None,
    })
}

/// One information field.
pub(crate) fn info_field(input: &mut &[u8]) -> Res<MmcInfoField> {
    let name = name.parse_next(input)?;
    let code = name.code().get();
    let base = name.extension() == MmcExtension::Base;
    let other = |data: &[u8]| {
        fragment(data).map_or_else(bad, |data| {
            Ok(MmcInfoField::Other(MmcOtherField { name, data }))
        })
    };
    match shape(code) {
        Shape::Time5 => {
            let bytes = take(5usize).parse_next(input)?;
            match (
                base,
                MmcTimeCodeName::from_wire(code),
                time_code_bytes(bytes),
            ) {
                (true, Some(field), Some(time)) => Ok(MmcInfoField::TimeCode(field, time)),
                (true, None, _) | (false, ..) => other(bytes),
                _ => bad(),
            }
        }
        Shape::Time2 => {
            let bytes = take(2usize).parse_next(input)?;
            match (
                base,
                code.checked_sub(0x20).and_then(MmcTimeCodeName::from_wire),
                short_time_code(bytes),
            ) {
                (true, Some(field), Some(time)) => Ok(MmcInfoField::ShortTimeCode(field, time)),
                (true, None, _) | (false, ..) => other(bytes),
                _ => bad(),
            }
        }
        Shape::Counted => {
            let data = counted.parse_next(input)?;
            if base {
                if let Some(field) = field_data(code, data) {
                    return Ok(field);
                }
                if (0x40..=0x65).contains(&code) {
                    return bad();
                }
            }
            other(data)
        }
        Shape::None if base && code == 0x7C => Ok(MmcInfoField::Wait),
        Shape::None if base && code == 0x7F => Ok(MmcInfoField::Resume),
        Shape::None => other(&[]),
    }
}

// --- commands ---

fn names_pair(d: &[u8]) -> Option<(MmcName, MmcName)> {
    within(d, |input: &mut &[u8]| {
        Ok((name.parse_next(input)?, name.parse_next(input)?))
    })
}

fn locate(d: &[u8]) -> Option<MmcLocate> {
    match d.split_first()? {
        (0, rest) => within(rest, name).map(MmcLocate::Field),
        (1, rest) => within(rest, position).map(MmcLocate::Target),
        _ => None,
    }
}

fn update(d: &[u8]) -> Option<MmcUpdate> {
    match d.split_first()? {
        (0, rest) => Some(MmcUpdate::Begin(sequence(rest)?)),
        (1, rest) => Some(MmcUpdate::End(sequence(rest)?)),
        _ => None,
    }
}

fn procedure(d: &[u8]) -> Option<MmcProcedure> {
    match d.split_first()? {
        (0, [first, rest @ ..]) => Some(MmcProcedure::Assemble {
            procedure: slot(*first)?,
            commands: sequence(rest)?,
        }),
        (1, [id]) => Some(MmcProcedure::Delete(selection(*id)?)),
        (2, [id]) => Some(MmcProcedure::Set(selection(*id)?)),
        (3, [id]) => Some(MmcProcedure::Execute(slot(*id)?)),
        _ => None,
    }
}

fn event(d: &[u8]) -> Option<MmcEvent> {
    match d.split_first()? {
        (0, rest) => {
            let mut input = rest;
            let event = slot(byte(&mut input).ok()?)?;
            let flags = event_flags(byte(&mut input).ok()?)?;
            let trigger_source = name.parse_next(&mut input).ok()?;
            let time = name.parse_next(&mut input).ok()?;
            Some(MmcEvent::Define {
                event,
                flags,
                trigger_source,
                time,
                command: one_command(sequence(input)?)?,
            })
        }
        (1, [id]) => Some(MmcEvent::Delete(selection(*id)?)),
        (2, [id]) => Some(MmcEvent::Set(selection(*id)?)),
        (3, [id]) => Some(MmcEvent::Test(slot(*id)?)),
        _ => None,
    }
}

fn group(d: &[u8]) -> Option<MmcGroup> {
    match d.split_first()? {
        (0, [id, rest @ ..]) => Some(MmcGroup::Assign {
            group: SysexDeviceId::new(U7::new(*id)?)?,
            devices: sequence(rest)?,
        }),
        (1, [id, rest @ ..]) => Some(MmcGroup::DisAssign {
            group: Recipient::of_byte(U7::new(*id)?),
            devices: sequence(rest)?,
        }),
        _ => None,
    }
}

fn masked_write(d: &[u8]) -> Option<MmcMaskedWrite> {
    within(d, |input: &mut &[u8]| {
        Ok(MmcMaskedWrite {
            field: name.parse_next(input)?,
            byte: u7.parse_next(input)?,
            mask: u7.parse_next(input)?,
            data: u7.parse_next(input)?,
        })
    })
}

fn counted_command(code: u8, d: &[u8]) -> Option<MmcCommand> {
    Some(match code {
        0x40 => MmcCommand::Write(sequence(d)?),
        0x41 => MmcCommand::MaskedWrite(masked_write(d)?),
        0x42 => MmcCommand::Read(sequence::<MmcName>(d)? as MmcNames),
        0x43 => MmcCommand::Update(update(d)?),
        0x44 => MmcCommand::Locate(locate(d)?),
        0x45 => MmcCommand::VariablePlay(speed(d)?),
        0x46 => MmcCommand::Search(speed(d)?),
        0x47 => MmcCommand::Shuttle(speed(d)?),
        0x48 => MmcCommand::Step(match *d {
            [byte] => MmcStep::from_wire(byte)?,
            _ => return None,
        }),
        0x49 => MmcCommand::AssignSystemMaster(match *d {
            [0x7F] => MmcSystemMaster::Nobody,
            [id] => MmcSystemMaster::Device(SysexDeviceId::new(U7::new(id)?)?),
            _ => return None,
        }),
        0x4A => MmcCommand::GeneratorCommand(one(d, GeneratorAction::from_wire)?),
        0x4B => MmcCommand::MtcCommand(one(d, MtcAction::from_wire)?),
        0x4C => {
            let (destination, source) = names_pair(d)?;
            MmcCommand::Move(MmcMove {
                destination,
                source,
            })
        }
        0x4D | 0x4E => {
            let arithmetic = within(d, |input: &mut &[u8]| {
                Ok(MmcArithmetic {
                    destination: name.parse_next(input)?,
                    first: name.parse_next(input)?,
                    second: name.parse_next(input)?,
                })
            })?;
            if code == 0x4D {
                MmcCommand::Add(arithmetic)
            } else {
                MmcCommand::Subtract(arithmetic)
            }
        }
        0x4F => MmcCommand::DropFrameAdjust(within(d, name)?),
        0x50 => MmcCommand::Procedure(procedure(d)?),
        0x51 => MmcCommand::Event(event(d)?),
        0x52 => MmcCommand::Group(group(d)?),
        0x53 => MmcCommand::CommandSegment(segment(d)?),
        0x54 => MmcCommand::DeferredVariablePlay(speed(d)?),
        0x55 => MmcCommand::RecordStrobeVariable(speed(d)?),
        _ => return None,
    })
}

fn unit_command(code: u8) -> Option<MmcCommand> {
    Some(match code {
        0x01 => MmcCommand::Stop,
        0x02 => MmcCommand::Play,
        0x03 => MmcCommand::DeferredPlay,
        0x04 => MmcCommand::FastForward,
        0x05 => MmcCommand::Rewind,
        0x06 => MmcCommand::RecordStrobe,
        0x07 => MmcCommand::RecordExit,
        0x08 => MmcCommand::RecordPause,
        0x09 => MmcCommand::Pause,
        0x0A => MmcCommand::Eject,
        0x0B => MmcCommand::Chase,
        0x0C => MmcCommand::CommandErrorReset,
        0x0D => MmcCommand::MmcReset,
        0x7C => MmcCommand::Wait,
        0x7F => MmcCommand::Resume,
        _ => return None,
    })
}

/// One command.
pub(crate) fn command(input: &mut &[u8]) -> Res<MmcCommand> {
    let name = name.parse_next(input)?;
    let code = name.code().get();
    let base = name.extension() == MmcExtension::Base;
    let reserved = |data: &[u8]| {
        fragment(data).map_or_else(bad, |data| {
            Ok(MmcCommand::Reserved(MmcReservedCommand { name, data }))
        })
    };
    if let Shape::Counted = shape(code) {
        let data = counted.parse_next(input)?;
        if base && (0x40..=0x55).contains(&code) {
            return counted_command(code, data).map_or_else(bad, Ok);
        }
        return reserved(data);
    }
    match unit_command(code).filter(|_| base) {
        Some(command) => Ok(command),
        None => reserved(&[]),
    }
}

/// The bytes after `06`: one or more commands, at most [`MMC_MAX_DATA`].
pub(crate) fn commands(rest: &[u8]) -> Option<MmcCommands> {
    if rest.is_empty() || rest.len() > MMC_MAX_DATA {
        return None;
    }
    sequence(rest)
}

/// The bytes after `07`: one or more information fields, at most [`MMC_MAX_DATA`].
pub(crate) fn fields(rest: &[u8]) -> Option<MmcFields> {
    if rest.is_empty() || rest.len() > MMC_MAX_DATA {
        return None;
    }
    sequence(rest)
}
