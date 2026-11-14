//! The Machine Control encoder: every command and information field as the bytes
//! the grammar in `read` takes back.

use super::command::{
    MmcArithmetic, MmcCommand, MmcEvent, MmcGroup, MmcLocate, MmcMaskedWrite, MmcMove,
    MmcProcedure, MmcSystemMaster, MmcUpdate,
};
use super::field::*;
use super::values::{
    MmcExtension, MmcName, MmcPosition, MmcSelection, MmcShortTimeCode, MmcSlot, MmcTimeCode,
    MmcTimeLabel, MmcTimeTail, MmcUserBits,
};
use super::{MmcBytes, MmcTooLong};
use crate::universal::Recipient;

type Out<'a> = &'a mut MmcBytes;
type Done = Result<(), MmcTooLong>;

pub(crate) fn name(name: MmcName, out: Out<'_>) -> Done {
    match name.extension() {
        MmcExtension::Base => {}
        MmcExtension::First => out.push(0)?,
        MmcExtension::Second => out.extend(&[0, 0])?,
    }
    out.push(name.code().get())
}

pub(crate) fn recipient(to: Recipient, out: Out<'_>) -> Done {
    out.push(match to {
        Recipient::AllCall => 0x7F,
        Recipient::Device(device) => device.get().get(),
    })
}

/// `code`, a count and what `body` writes.
fn counted(code: u8, out: Out<'_>, body: impl FnOnce(Out<'_>) -> Done) -> Done {
    let mut inner = MmcBytes::EMPTY;
    body(&mut inner)?;
    out.push(code)?;
    out.push(inner.len())?;
    out.extend(inner.as_slice())
}

fn time_label(label: MmcTimeLabel, status: bool, out: Out<'_>) -> Done {
    let mut head = label.head();
    head[3] |= u8::from(status) << 5;
    out.extend(&head)
}

fn tail(tail: MmcTimeTail, out: Out<'_>) -> Done {
    out.push(tail.wire().1)
}

fn time_code(code: MmcTimeCode, out: Out<'_>) -> Done {
    time_label(code.label, code.tail.wire().0, out)?;
    tail(code.tail, out)
}

fn position(position: MmcPosition, out: Out<'_>) -> Done {
    time_code(position.into(), out)
}

fn short_time_code(code: MmcShortTimeCode, out: Out<'_>) -> Done {
    let (status, last) = code.tail().wire();
    out.push(code.sign().wire() << 6 | u8::from(status) << 5 | code.frames())?;
    out.push(last)
}

fn user_bits(bits: MmcUserBits, out: Out<'_>) -> Done {
    for group in bits.groups {
        out.push(group.get())?;
    }
    out.push(
        u8::from(bits.flags.secondary_time_code) << 2
            | u8::from(bits.flags.group_flag_1) << 1
            | u8::from(bits.flags.group_flag_0),
    )
}

fn selection(selection: MmcSelection) -> u8 {
    match selection {
        MmcSelection::One(slot) => slot.get().get(),
        MmcSelection::All => 0x7F,
    }
}

fn slot(slot: MmcSlot) -> u8 {
    slot.get().get()
}

fn event_flags(flags: MmcEventFlags) -> u8 {
    u8::from(flags.keep) << 6 | u8::from(flags.all_speeds) << 4 | flags.direction.wire()
}

fn names(names: &super::MmcNames, out: Out<'_>) -> Done {
    out.extend(names.as_bytes())
}

fn motion_tally(t: MotionControlTally, out: Out<'_>) -> Done {
    out.push(t.state.wire())?;
    out.push(t.process.wire())?;
    out.push(t.process_success.wire() << 4 | t.state_success.wire())
}

fn command_error(e: MmcErrorReport, out: Out<'_>) -> Done {
    let f = e.flags;
    out.push(
        u8::from(f.sent_before) << 5
            | u8::from(f.unsolicited) << 4
            | u8::from(f.event_error) << 2
            | u8::from(f.procedure_error) << 1
            | u8::from(f.halted),
    )?;
    out.push(e.level.get())?;
    match e.error {
        MmcErrorCode::Kind(kind) => out.push(kind.wire())?,
        MmcErrorCode::Extended(code) => name(code, out)?,
    }
    match e.command {
        None => out.push(0),
        Some(c) => {
            out.push(c.command.len() + 1)?;
            out.push(c.offset.get())?;
            out.extend(c.command.as_bytes())
        }
    }
}

/// One information field.
pub(crate) fn info_field(field: &MmcInfoField, out: Out<'_>) -> Done {
    let one = |code: u8, out: Out<'_>, byte: u8| counted(code, out, |b| b.push(byte));
    let tracks = |code: u8, out: Out<'_>, t: &super::values::MmcTrackBitmap| {
        counted(code, out, |b| b.extend(t.as_bytes()))
    };
    match *field {
        MmcInfoField::TimeCode(which, code) => {
            out.push(which.wire())?;
            time_code(code, out)
        }
        MmcInfoField::ShortTimeCode(which, code) => {
            out.push(which.wire() + 0x20)?;
            short_time_code(code, out)
        }
        MmcInfoField::Signature(s) => counted(0x40, out, |b| {
            b.extend(&[s.major.get(), s.minor.get(), 0, 0])?;
            b.push(s.commands.len())?;
            b.extend(s.commands.as_bytes())?;
            b.push(s.fields.len())?;
            b.extend(s.fields.as_bytes())
        }),
        MmcInfoField::UpdateRate(v) => one(0x41, out, v.get()),
        MmcInfoField::ResponseError(n) => counted(0x42, out, |b| names(&n, b)),
        MmcInfoField::CommandError(e) => counted(0x43, out, |b| command_error(e, b)),
        MmcInfoField::CommandErrorLevel(v) => one(0x44, out, v.get()),
        MmcInfoField::TimeStandard(t) => one(0x45, out, t.code() << 5),
        MmcInfoField::TimeCodeSource(v) => one(0x46, out, v.wire()),
        MmcInfoField::TimeCodeUserBits(u) => counted(0x47, out, |b| user_bits(u, b)),
        MmcInfoField::MotionControlTally(t) => counted(0x48, out, |b| motion_tally(t, b)),
        MmcInfoField::VelocityTally(s) => counted(0x49, out, |b| b.extend(&s.wire())),
        MmcInfoField::StopMode(v) => one(0x4A, out, v.wire()),
        MmcInfoField::FastMode(v) => one(0x4B, out, v.wire()),
        MmcInfoField::RecordMode(v) => one(0x4C, out, v.wire()),
        MmcInfoField::RecordStatus(s) => one(
            0x4D,
            out,
            u8::from(s.no_tracks_active) << 6
                | u8::from(s.rehearse_inhibited) << 5
                | u8::from(s.record_inhibited) << 4
                | s.activity.wire(),
        ),
        MmcInfoField::TrackRecordStatus(t) => tracks(0x4E, out, &t),
        MmcInfoField::TrackRecordReady(t) => tracks(0x4F, out, &t),
        MmcInfoField::GlobalMonitor(v) => one(0x50, out, v.wire()),
        MmcInfoField::RecordMonitor(v) => one(0x51, out, v.wire()),
        MmcInfoField::TrackSyncMonitor(t) => tracks(0x52, out, &t),
        MmcInfoField::TrackInputMonitor(t) => tracks(0x53, out, &t),
        MmcInfoField::StepLength(v) => one(0x54, out, v.get()),
        MmcInfoField::PlaySpeedReference(v) => one(0x55, out, v.wire()),
        MmcInfoField::FixedSpeed(v) => one(0x56, out, v.get()),
        MmcInfoField::LifterDefeat(v) => one(0x57, out, v.wire()),
        MmcInfoField::ControlDisable(v) => one(0x58, out, v.wire()),
        MmcInfoField::ResolvedPlayMode(v) => one(0x59, out, v.wire()),
        MmcInfoField::ChaseMode(v) => one(0x5A, out, v.wire()),
        MmcInfoField::GeneratorCommandTally(t) => counted(0x5B, out, |b| {
            b.push(t.command.wire())?;
            b.push(
                u8::from(t.frame_sync_lost) << 5 | u8::from(t.source_lost) << 4 | t.success.wire(),
            )
        }),
        MmcInfoField::GeneratorSetUp(s) => counted(0x5C, out, |b| {
            b.push(s.jam.wire() << 4 | s.run.wire())?;
            name(s.source, b)?;
            b.push(s.mode.wire())
        }),
        MmcInfoField::GeneratorUserBits(u) => counted(0x5D, out, |b| user_bits(u, b)),
        MmcInfoField::MtcCommandTally(t) => counted(0x5E, out, |b| {
            b.push(t.command.wire())?;
            b.push(t.success.wire())
        }),
        MmcInfoField::MtcSetUp(s) => counted(0x5F, out, |b| {
            let f = s.flags;
            b.push(
                u8::from(f.mute_response_cable) << 5
                    | u8::from(f.send_user_bits) << 4
                    | u8::from(f.full_messages_fast) << 3
                    | u8::from(f.send_while_fast) << 2
                    | u8::from(f.full_messages_stopped) << 1
                    | u8::from(f.send_while_stopped),
            )?;
            name(s.source, b)
        }),
        MmcInfoField::ProcedureResponse(p) => counted(0x60, out, |b| match p {
            MmcProcedureResponse::Invalid => b.push(0x7F),
            MmcProcedureResponse::Procedure {
                procedure,
                commands,
            } => {
                b.push(slot(procedure))?;
                b.extend(commands.as_bytes())
            }
        }),
        MmcInfoField::EventResponse(e) => counted(0x61, out, |b| match e {
            MmcEventResponse::Invalid => b.push(0x7F),
            MmcEventResponse::Event {
                event,
                flags,
                trigger_source,
                time,
                command,
            } => {
                b.push(slot(event))?;
                b.push(event_flags(flags))?;
                name(trigger_source, b)?;
                position(time, b)?;
                b.extend(command.as_bytes())
            }
        }),
        MmcInfoField::TrackMute(t) => tracks(0x62, out, &t),
        MmcInfoField::VitcInsertEnable(v) => counted(0x63, out, |b| {
            b.extend(&[v.control.get(), v.first_line.get(), v.second_line.get()])
        }),
        MmcInfoField::ResponseSegment(s) => counted(0x64, out, |b| segment(s, b)),
        MmcInfoField::Failure(text) => counted(0x65, out, |b| b.extend(text.as_bytes())),
        MmcInfoField::Wait => out.push(0x7C),
        MmcInfoField::Resume => out.push(0x7F),
        MmcInfoField::Other(other) => {
            name(other.name, out)?;
            if (0x40..=0x77).contains(&other.name.code().get()) {
                out.push(other.data.len())?;
            }
            out.extend(other.data.as_bytes())
        }
    }
}

fn segment(s: MmcSegment, out: Out<'_>) -> Done {
    out.push(s.place().wire() << 6 | s.remaining())?;
    out.extend(s.bytes().as_bytes())
}

fn speed(code: u8, s: super::values::MmcSpeed, out: Out<'_>) -> Done {
    counted(code, out, |b| b.extend(&s.wire()))
}

/// One command.
pub(crate) fn command(command: &MmcCommand, out: Out<'_>) -> Done {
    let unit = |code: u8, out: Out<'_>| out.push(code);
    match *command {
        MmcCommand::Stop => unit(0x01, out),
        MmcCommand::Play => unit(0x02, out),
        MmcCommand::DeferredPlay => unit(0x03, out),
        MmcCommand::FastForward => unit(0x04, out),
        MmcCommand::Rewind => unit(0x05, out),
        MmcCommand::RecordStrobe => unit(0x06, out),
        MmcCommand::RecordExit => unit(0x07, out),
        MmcCommand::RecordPause => unit(0x08, out),
        MmcCommand::Pause => unit(0x09, out),
        MmcCommand::Eject => unit(0x0A, out),
        MmcCommand::Chase => unit(0x0B, out),
        MmcCommand::CommandErrorReset => unit(0x0C, out),
        MmcCommand::MmcReset => unit(0x0D, out),
        MmcCommand::Write(fields) => counted(0x40, out, |b| b.extend(fields.as_bytes())),
        MmcCommand::MaskedWrite(MmcMaskedWrite {
            field,
            byte,
            mask,
            data,
        }) => counted(0x41, out, |b| {
            name(field, b)?;
            b.extend(&[byte.get(), mask.get(), data.get()])
        }),
        MmcCommand::Read(n) => counted(0x42, out, |b| names(&n, b)),
        MmcCommand::Update(MmcUpdate::Begin(n)) => counted(0x43, out, |b| {
            b.push(0)?;
            names(&n, b)
        }),
        MmcCommand::Update(MmcUpdate::End(n)) => counted(0x43, out, |b| {
            b.push(1)?;
            names(&n, b)
        }),
        MmcCommand::Locate(MmcLocate::Field(field)) => counted(0x44, out, |b| {
            b.push(0)?;
            name(field, b)
        }),
        MmcCommand::Locate(MmcLocate::Target(target)) => counted(0x44, out, |b| {
            b.push(1)?;
            position(target, b)
        }),
        MmcCommand::VariablePlay(s) => speed(0x45, s, out),
        MmcCommand::Search(s) => speed(0x46, s, out),
        MmcCommand::Shuttle(s) => speed(0x47, s, out),
        MmcCommand::Step(step) => counted(0x48, out, |b| b.push(step.wire())),
        MmcCommand::AssignSystemMaster(master) => counted(0x49, out, |b| {
            b.push(match master {
                MmcSystemMaster::Nobody => 0x7F,
                MmcSystemMaster::Device(device) => device.get().get(),
            })
        }),
        MmcCommand::GeneratorCommand(a) => counted(0x4A, out, |b| b.push(a.wire())),
        MmcCommand::MtcCommand(a) => counted(0x4B, out, |b| b.push(a.wire())),
        MmcCommand::Move(MmcMove {
            destination,
            source,
        }) => counted(0x4C, out, |b| {
            name(destination, b)?;
            name(source, b)
        }),
        MmcCommand::Add(a) => arithmetic(0x4D, a, out),
        MmcCommand::Subtract(a) => arithmetic(0x4E, a, out),
        MmcCommand::DropFrameAdjust(field) => counted(0x4F, out, |b| name(field, b)),
        MmcCommand::Procedure(p) => counted(0x50, out, |b| match p {
            MmcProcedure::Assemble {
                procedure,
                commands,
            } => {
                b.extend(&[0, slot(procedure)])?;
                b.extend(commands.as_bytes())
            }
            MmcProcedure::Delete(s) => b.extend(&[1, selection(s)]),
            MmcProcedure::Set(s) => b.extend(&[2, selection(s)]),
            MmcProcedure::Execute(s) => b.extend(&[3, slot(s)]),
        }),
        MmcCommand::Event(e) => counted(0x51, out, |b| match e {
            MmcEvent::Define {
                event,
                flags,
                trigger_source,
                time,
                command,
            } => {
                b.extend(&[0, slot(event), event_flags(flags)])?;
                name(trigger_source, b)?;
                name(time, b)?;
                b.extend(command.as_bytes())
            }
            MmcEvent::Delete(s) => b.extend(&[1, selection(s)]),
            MmcEvent::Set(s) => b.extend(&[2, selection(s)]),
            MmcEvent::Test(s) => b.extend(&[3, slot(s)]),
        }),
        MmcCommand::Group(g) => counted(0x52, out, |b| match g {
            MmcGroup::Assign { group, devices } => {
                b.extend(&[0, group.get().get()])?;
                b.extend(devices.as_bytes())
            }
            MmcGroup::DisAssign { group, devices } => {
                b.push(1)?;
                recipient(group, b)?;
                b.extend(devices.as_bytes())
            }
        }),
        MmcCommand::CommandSegment(s) => counted(0x53, out, |b| segment(s, b)),
        MmcCommand::DeferredVariablePlay(s) => speed(0x54, s, out),
        MmcCommand::RecordStrobeVariable(s) => speed(0x55, s, out),
        MmcCommand::Wait => unit(0x7C, out),
        MmcCommand::Resume => unit(0x7F, out),
        MmcCommand::Reserved(r) => {
            name(r.name, out)?;
            if let 0x40..=0x77 = r.name.code().get() {
                out.push(r.data.len())?;
            }
            out.extend(r.data.as_bytes())
        }
    }
}

fn arithmetic(code: u8, a: MmcArithmetic, out: Out<'_>) -> Done {
    counted(code, out, |b| {
        name(a.destination, b)?;
        name(a.first, b)?;
        name(a.second, b)
    })
}
