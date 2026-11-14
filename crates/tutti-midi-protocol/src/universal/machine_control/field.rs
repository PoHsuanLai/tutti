//! Information fields: what a MIDI Machine Control response carries and a WRITE
//! loads (RP-013 pp.44 to 74).

use tutti_ump::U7;

use crate::smpte::MtcRate;

use super::values::{
    MmcFragment, MmcName, MmcPosition, MmcShortTimeCode, MmcSlot, MmcSpeed, MmcTimeCode,
    MmcTrackBitmap, MmcUserBits, wire_enum,
};
use super::{MmcCommands, MmcNames};

wire_enum! {
    /// The time code fields of five bytes (names `01` to `0F`, RP-013 pp.44 to 47);
    /// the short form of each is its name plus `20`.
    MmcTimeCodeName {
        /// `01`, read/write.
        SelectedTimeCode = 0x01,
        /// `02`, read only.
        SelectedMasterCode = 0x02,
        /// `03`, read/write.
        RequestedOffset = 0x03,
        /// `04`, read only.
        ActualOffset = 0x04,
        /// `05`, read only.
        LockDeviation = 0x05,
        /// `06`, read/write.
        GeneratorTimeCode = 0x06,
        /// `07`, read only.
        MidiTimeCodeInput = 0x07,
        /// `08`: general purpose register 0, the locate point.
        Gp0 = 0x08,
        /// `09`: general purpose register 1.
        Gp1 = 0x09,
        /// `0A`: general purpose register 2.
        Gp2 = 0x0A,
        /// `0B`: general purpose register 3.
        Gp3 = 0x0B,
        /// `0C`: general purpose register 4.
        Gp4 = 0x0C,
        /// `0D`: general purpose register 5.
        Gp5 = 0x0D,
        /// `0E`: general purpose register 6.
        Gp6 = 0x0E,
        /// `0F`: general purpose register 7.
        Gp7 = 0x0F,
    }
}

impl From<MmcTimeCodeName> for MmcName {
    fn from(name: MmcTimeCodeName) -> Self {
        MmcName::of(name.wire())
    }
}

wire_enum! {
    /// Success of a motion state (`aaa` of the tally's status byte, RP-013 pp.56 and 57).
    McsSuccess {
        /// `000`: transition in progress.
        InTransition = 0,
        /// `001`: completely stopped, motion achieved, resolved, ejected or step completed.
        Achieved = 1,
        /// `010`: failure.
        Failure = 2,
        /// `011`: motion deduced, not commanded (STOP, FAST FORWARD, REWIND, PLAY).
        Deduced = 3,
        /// `100`: STEP in progress.
        StepInProgress = 4,
        /// `101`: playing but not resolved.
        NotResolved = 5,
    }
}

wire_enum! {
    /// Success of a motion process (`bbb` of the tally's status byte, RP-013 p.57).
    McpSuccess {
        /// `000`: actively locating, or chasing in play.
        Active = 0,
        /// `001`: locate complete, or synchronized.
        Complete = 1,
        /// `010`: failure.
        Failure = 2,
        /// `100`: locating with DEFERRED PLAY pending, or chasing in non-play mode.
        DeferredPlayPending = 4,
        /// `110`: locating with DEFERRED VARIABLE PLAY pending, or parked in a chase.
        DeferredVariablePlayPending = 6,
    }
}

wire_enum! {
    /// The motion control state a tally names (RP-013 p.56).
    MotionState {
        /// `01`.
        Stop = 0x01,
        /// `02`.
        Play = 0x02,
        /// `04`.
        FastForward = 0x04,
        /// `05`.
        Rewind = 0x05,
        /// `09`.
        Pause = 0x09,
        /// `0A`.
        Eject = 0x0A,
        /// `45`.
        VariablePlay = 0x45,
        /// `46`.
        Search = 0x46,
        /// `47`.
        Shuttle = 0x47,
        /// `48`.
        Step = 0x48,
    }
}

wire_enum! {
    /// The motion control process a tally names (RP-013 p.56).
    MotionProcess {
        /// `0B`.
        Chase = 0x0B,
        /// `44`.
        Locate = 0x44,
        /// `7F`: no process is active.
        None = 0x7F,
    }
}

/// MOTION CONTROL TALLY (`48`, RP-013 pp.56 and 57).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MotionControlTally {
    /// The most recently activated state.
    pub state: MotionState,
    /// The most recently activated process.
    pub process: MotionProcess,
    /// How the state fared.
    pub state_success: McsSuccess,
    /// How the process fared.
    pub process_success: McpSuccess,
}

wire_enum! {
    /// SELECTED TIME CODE SOURCE (`46`, RP-013 p.55).
    MmcTimeCodeSource {
        /// `00`: longitudinal time code.
        Ltc = 0x00,
        /// `01`: vertical interval time code.
        Vitc = 0x01,
        /// `02`: the tape counter.
        TapeCounter = 0x02,
        /// `04`: VITC or LTC, whichever is there.
        AutoVitcLtc = 0x04,
        /// `7F`: as defined locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// STOP MODE (`4A`, RP-013 p.58).
    MmcStopMode {
        /// `00`: no monitoring.
        Disabled = 0x00,
        /// `01`: monitoring.
        Enabled = 0x01,
        /// `7F`: as defined locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// FAST MODE (`4B`, RP-013 p.59).
    MmcFastMode {
        /// `00`: maximum velocity, no monitoring.
        Unmonitored = 0x00,
        /// `01`: the highest velocity that still monitors.
        Monitored = 0x01,
        /// `7F`: as defined locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// RECORD MODE (`4C`, RP-013 p.60).
    MmcRecordMode {
        /// `00`.
        Disabled = 0x00,
        /// `01`: ATR record, VTR insert.
        Insert = 0x01,
        /// `02`: ATR record, VTR assemble.
        Assemble = 0x02,
        /// `04`.
        Rehearse = 0x04,
        /// `05`: ATR record, VTR crash or full record.
        CrashRecord = 0x05,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// The activity of RECORD STATUS (`aaaa`, RP-013 p.61).
    MmcRecordActivity {
        /// `0`.
        None = 0,
        /// `1`: ATR recording, VTR insert recording.
        Recording = 1,
        /// `2`: VTR assemble recording.
        Assembling = 2,
        /// `4`.
        Rehearsing = 4,
        /// `5`: VTR crash or full record.
        CrashRecording = 5,
        /// `6`.
        RecordPause = 6,
    }
}

/// RECORD STATUS (`4D`, RP-013 p.61).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcRecordStatus {
    /// What is recording or rehearsing.
    pub activity: MmcRecordActivity,
    /// `b`: local record inhibit.
    pub record_inhibited: bool,
    /// `c`: local rehearse inhibit.
    pub rehearse_inhibited: bool,
    /// `d`: no track is active (valid when the activity is not `None`).
    pub no_tracks_active: bool,
}

wire_enum! {
    /// GLOBAL MONITOR (`50`, RP-013 p.62).
    MmcGlobalMonitor {
        /// `00`: synchronous playback.
        PlaybackSynchronous = 0x00,
        /// `01`: input, full.
        Input = 0x01,
        /// `02`: repro playback.
        PlaybackRepro = 0x02,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// RECORD MONITOR (`51`, RP-013 p.63).
    MmcRecordMonitor {
        /// `00`.
        RecordOnly = 0x00,
        /// `01`.
        RecordOrNonPlay = 0x01,
        /// `02`.
        RecordOrRecordReady = 0x02,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// PLAY SPEED REFERENCE (`55`, RP-013 p.65).
    MmcPlaySpeedReference {
        /// `00`.
        Internal = 0x00,
        /// `01`.
        External = 0x01,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// LIFTER DEFEAT (`57`, RP-013 p.66).
    MmcLifterDefeat {
        /// `00`: lifters work.
        NoDefeat = 0x00,
        /// `01`: lifters defeated.
        Defeat = 0x01,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// CONTROL DISABLE (`58`, RP-013 p.66).
    MmcControlDisable {
        /// `00`: control enabled.
        Enable = 0x00,
        /// `01`: transport control and synchronization commands ignored.
        Disable = 0x01,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// RESOLVED PLAY MODE (`59`, RP-013 p.67).
    MmcResolvedPlayMode {
        /// `00`.
        Normal = 0x00,
        /// `01`.
        FreeResolve = 0x01,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// CHASE MODE (`5A`, RP-013 p.67).
    MmcChaseMode {
        /// `00`.
        AbsoluteStandard = 0x00,
        /// `01`.
        AbsoluteResolve = 0x01,
        /// `7F`: as selected locally (write only).
        Local = 0x7F,
    }
}

wire_enum! {
    /// The action of GENERATOR COMMAND (`4A`, RP-013 p.31) and the command
    /// GENERATOR COMMAND TALLY reports.
    GeneratorAction {
        /// `00`.
        Stop = 0x00,
        /// `01`: run, frame locked as the set up says.
        Run = 0x01,
        /// `02`: copy or jam the source time code.
        CopyJam = 0x02,
    }
}

wire_enum! {
    /// The action of MIDI TIME CODE COMMAND (`4B`, RP-013 p.31) and the command
    /// MIDI TIME CODE COMMAND TALLY reports.
    MtcAction {
        /// `00`.
        Off = 0x00,
        /// `02`: follow the time code the set up defines.
        Follow = 0x02,
    }
}

wire_enum! {
    /// The success level of a generator or MIDI time code command
    /// (RP-013 pp.68 and 69).
    MmcCommandSuccess {
        /// `000`.
        InTransition = 0,
        /// `001`.
        Successful = 1,
        /// `010`.
        Failure = 2,
    }
}

/// GENERATOR COMMAND TALLY (`5B`, RP-013 p.68).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GeneratorTally {
    /// The most recent command.
    pub command: GeneratorAction,
    /// `c`: the external frame sync reference was lost.
    pub frame_sync_lost: bool,
    /// `b`: the source time code was lost during copy or jam.
    pub source_lost: bool,
    /// How the command fared.
    pub success: MmcCommandSuccess,
}

/// MIDI TIME CODE COMMAND TALLY (`5E`, RP-013 p.69).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MtcTally {
    /// The most recent command.
    pub command: MtcAction,
    /// How it fared.
    pub success: MmcCommandSuccess,
}

wire_enum! {
    /// The frame sync reference for run mode (`nnn` of GENERATOR SET UP, RP-013 p.68).
    RunReference {
        /// `000`: internal crystal, standard.
        Standard = 0,
        /// `001`: a locally defined external frame reference.
        External = 1,
        /// `010`: internal crystal, drop A.
        DropA = 2,
        /// `011`: internal crystal, drop B.
        DropB = 3,
        /// `111`: as locally defined (write only).
        Local = 7,
    }
}

wire_enum! {
    /// The frame sync reference for copy or jam (`yyy` of GENERATOR SET UP, RP-013 p.68).
    JamReference {
        /// `000`: the time code source's frame edges.
        SourceFrameEdges = 0,
        /// `001`: a locally defined external frame reference.
        External = 1,
        /// `111`: as locally defined (write only).
        Local = 7,
    }
}

wire_enum! {
    /// The copy or jam mode of GENERATOR SET UP (RP-013 p.68).
    JamMode {
        /// `00`: stop when the source stops.
        StopWithSource = 0,
        /// `01`: keep running when the source stops.
        FreeRun = 1,
    }
}

/// GENERATOR SET UP (`5C`, RP-013 p.68).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GeneratorSetUp {
    /// The reference for run mode.
    pub run: RunReference,
    /// The reference for copy or jam mode.
    pub jam: JamReference,
    /// The source of copy or jam: `01` selected time code, `02` selected
    /// master code, `7F` as defined locally.
    pub source: MmcName,
    /// What a vanished source does to the generator.
    pub mode: JamMode,
}

/// The flags of MIDI TIME CODE SET UP (`5F`, RP-013 p.70).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MtcFlags {
    /// `a`: keep transmitting while the source is stopped.
    pub send_while_stopped: bool,
    /// `b`: send full messages, not quarter frames, while stopped.
    pub full_messages_stopped: bool,
    /// `c`: keep transmitting while the source is fast.
    pub send_while_fast: bool,
    /// `d`: send full messages, not quarter frames, while fast.
    pub full_messages_fast: bool,
    /// `e`: transmit the user bits.
    pub send_user_bits: bool,
    /// `f`: do not transmit on the MMC response cable.
    pub mute_response_cable: bool,
}

/// MIDI TIME CODE SET UP (`5F`, RP-013 p.70).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MtcSetUp {
    /// The flags.
    pub flags: MtcFlags,
    /// The time code source: `01`, `02`, `06`, `07`, or `7F` as defined locally.
    pub source: MmcName,
}

/// VITC INSERT ENABLE (`63`, RP-013 p.72).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VitcInsert {
    /// `00` disable, `01` enable, `7F` as selected locally.
    pub control: U7,
    /// The first horizontal line, `7F` as selected locally.
    pub first_line: U7,
    /// The second, non-adjacent line, `7F` as selected locally.
    pub second_line: U7,
}

/// The flags of an event (RP-013 p.36): when it triggers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcEventFlags {
    /// Which direction of motion triggers it.
    pub direction: MmcEventDirection,
    /// `a`: trigger at any speed, as soon as the time is reached or passed.
    pub all_speeds: bool,
    /// `k`: keep the event after it triggers.
    pub keep: bool,
}

wire_enum! {
    /// The direction modes of an event (`dd`, RP-013 p.36).
    MmcEventDirection {
        /// `00`: forward only.
        Forward = 0,
        /// `01`: reverse only.
        Reverse = 1,
        /// `10`: either.
        Either = 2,
    }
}

/// EVENT RESPONSE (`61`, RP-013 p.71).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcEventResponse {
    /// `7F`: no event is set.
    Invalid,
    /// An event as defined.
    Event {
        /// Its name.
        event: MmcSlot,
        /// When it triggers.
        flags: MmcEventFlags,
        /// The time code stream it triggers against: `01`, `02`, `06` or `07`.
        trigger_source: MmcName,
        /// The trigger time.
        time: MmcPosition,
        /// The command it runs.
        command: MmcCommands,
    },
}

/// PROCEDURE RESPONSE (`60`, RP-013 p.71).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcProcedureResponse {
    /// `7F`: no procedure is set.
    Invalid,
    /// A procedure as assembled.
    Procedure {
        /// Its name.
        procedure: MmcSlot,
        /// Its commands.
        commands: MmcCommands,
    },
}

wire_enum! {
    /// Where a segment sits in its string (RP-013 p.5): the `f` bit.
    MmcSegmentPlace {
        /// `0`: a later segment.
        Later = 0,
        /// `1`: the first segment.
        First = 1,
    }
}

/// One segment of a longer command or response string (RP-013 pp.5, 40 and 73).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcSegment {
    place: MmcSegmentPlace,
    remaining: u8,
    bytes: MmcFragment,
}

impl MmcSegment {
    /// The segment `bytes` at `place` in its string, with `remaining`
    /// segments still to come (0..=63; the last is `0`), or `None` above 63.
    #[must_use]
    pub const fn new(place: MmcSegmentPlace, remaining: u8, bytes: MmcFragment) -> Option<Self> {
        if remaining < 64 {
            Some(Self {
                place,
                remaining,
                bytes,
            })
        } else {
            None
        }
    }

    /// Whether this is the first segment of its string.
    #[must_use]
    pub const fn place(&self) -> MmcSegmentPlace {
        self.place
    }

    /// The segments still to come; the last is `0`.
    #[must_use]
    pub const fn remaining(&self) -> u8 {
        self.remaining
    }

    /// This segment's bytes, which need not end on a message boundary.
    #[must_use]
    pub const fn bytes(&self) -> &MmcFragment {
        &self.bytes
    }
}

wire_enum! {
    /// The error codes of COMMAND ERROR (RP-013 pp.52 and 53). `01` to `09` are
    /// major errors, `20` to `26` immediate operational ones, `40` to `63` implementation
    /// ones.
    MmcErrorKind {
        /// `01`: receive buffer overflow.
        ReceiveBufferOverflow = 0x01,
        /// `02`: the Sysex ends or a status byte comes off a message boundary.
        CommandSysexLength = 0x02,
        /// `03`: a command's count disagrees with the Sysex length.
        CommandCount = 0x03,
        /// `04`: a field's count disagrees with the Sysex length during WRITE.
        FieldCountDuringWrite = 0x04,
        /// `05`: a group named `7F`.
        IllegalGroupName = 0x05,
        /// `06`: a procedure named `7F`.
        IllegalProcedureName = 0x06,
        /// `07`: an event named `7F`.
        IllegalEventName = 0x07,
        /// `08`: a name extended past the second level.
        IllegalNameExtension = 0x08,
        /// `09`: segmentation error.
        Segmentation = 0x09,
        /// `20`: UPDATE list overflow.
        UpdateListOverflow = 0x20,
        /// `21`: GROUP buffer overflow.
        GroupBufferOverflow = 0x21,
        /// `22`: undefined procedure.
        UndefinedProcedure = 0x22,
        /// `23`: PROCEDURE buffer overflow.
        ProcedureBufferOverflow = 0x23,
        /// `24`: undefined event.
        UndefinedEvent = 0x24,
        /// `25`: EVENT buffer overflow.
        EventBufferOverflow = 0x25,
        /// `26`: blank time code.
        BlankTimeCode = 0x26,
        /// `40`: unsupported command.
        UnsupportedCommand = 0x40,
        /// `41`: unrecognized sub-command.
        UnrecognizedSubCommand = 0x41,
        /// `42`: unrecognized command data.
        UnrecognizedCommandData = 0x42,
        /// `43`: unsupported information field name in command data.
        UnsupportedFieldInCommandData = 0x43,
        /// `44`: unsupported information field name in a READ or UPDATE within a procedure.
        UnsupportedFieldInProcedure = 0x44,
        /// `45`: event trigger source unavailable or unsupported.
        EventTriggerUnavailable = 0x45,
        /// `46`: nested PROCEDURE assemble.
        NestedProcedureAssemble = 0x46,
        /// `47`: recursive PROCEDURE execute.
        RecursiveProcedureExecute = 0x47,
        /// `48`: nested EVENT define.
        NestedEventDefine = 0x48,
        /// `49`: PROCEDURE assemble within EVENT define.
        ProcedureAssembleInEventDefine = 0x49,
        /// `60`: a WRITE to an unsupported field.
        WriteToUnsupportedField = 0x60,
        /// `61`: a WRITE to a read-only field.
        WriteToReadOnlyField = 0x61,
        /// `62`: unrecognized field data during WRITE.
        UnrecognizedFieldData = 0x62,
        /// `63`: unsupported field name in field data during WRITE.
        UnsupportedFieldInFieldData = 0x63,
        /// `7F`: no errors since power up or MMC RESET.
        NoErrors = 0x7F,
    }
}

/// The error code of a COMMAND ERROR: one of the table, or a code of an
/// extension set, which the specification reserves and classes as the code's
/// final byte is (RP-013 p.52).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcErrorCode {
    /// A code of the table.
    Kind(MmcErrorKind),
    /// `00 nn` or `00 00 nn`.
    Extended(MmcName),
}

/// A COMMAND ERROR's flags (RP-013 p.51).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcErrorFlags {
    /// `a`: error halt in effect.
    pub halted: bool,
    /// `b`: an error was found pre-checking a procedure assembly.
    pub procedure_error: bool,
    /// `c`: an error was found pre-checking an event definition.
    pub event_error: bool,
    /// `e`: sent unsolicited, not in answer to a READ.
    pub unsolicited: bool,
    /// `f`: sent before since the most recent error was recorded.
    pub sent_before: bool,
}

/// The command a COMMAND ERROR names: where in it the error lay, and as much of
/// it as fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcErrorCommand {
    /// The offset of the offending byte in the command string, `7F` if unknown.
    pub offset: U7,
    /// The command, possibly truncated.
    pub command: MmcFragment,
}

/// COMMAND ERROR (`43`, RP-013 pp.51 to 53).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcErrorReport {
    /// The flags.
    pub flags: MmcErrorFlags,
    /// The COMMAND ERROR LEVEL in force.
    pub level: U7,
    /// The error code.
    pub error: MmcErrorCode,
    /// The command at fault, `None` for the errors that name none.
    pub command: Option<MmcErrorCommand>,
}

/// A bitmap of the commands or fields a device supports (RP-013 p.48): a bit
/// a name, seven to a byte (the last byte of each run of 32 holds four), with
/// the extension sets after the base set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcSupportBitmap(MmcFragment);

impl MmcSupportBitmap {
    /// The bitmap of wire bytes, or `None` for a byte over 127 or more than
    /// [`MmcFragment::MAX`] bytes.
    #[must_use]
    pub fn from_bytes(wire: &[u8]) -> Option<Self> {
        MmcFragment::new(wire).map(Self)
    }

    /// The wire bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// The number of wire bytes.
    pub(super) const fn len(&self) -> u8 {
        self.0.len()
    }

    /// Whether `name` is supported: its bit is set. Names past the bytes sent
    /// are unsupported.
    #[must_use]
    pub fn supports(&self, name: MmcName) -> bool {
        let set = match name.extension() {
            super::values::MmcExtension::Base => 0,
            super::values::MmcExtension::First => 20,
            super::values::MmcExtension::Second => 40,
        };
        // Each run of 32 codes is five bytes: four of seven bits and one of four.
        let code = usize::from(name.code().get());
        let (run, within) = (code / 32, code % 32);
        self.as_bytes()
            .get(set + run * 5 + within / 7)
            .is_some_and(|b| b >> (within % 7) & 1 != 0)
    }
}

/// SIGNATURE (`40`, RP-013 pp.48 and 49).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcSignature {
    /// The MMC version, integer part.
    pub major: U7,
    /// The MMC version, fractional part in hundredths.
    pub minor: U7,
    /// The commands supported.
    pub commands: MmcSupportBitmap,
    /// The responses and information fields supported.
    pub fields: MmcSupportBitmap,
}

/// A field the specification leaves undefined, whose length its name gives
/// (RP-013 p.6): a response is skipped without knowing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcOtherField {
    /// Its name.
    pub name: MmcName,
    /// Its data, without the count byte.
    pub data: MmcFragment,
}

/// An information field: one response of a Machine Control Response message,
/// or one field a WRITE loads (RP-013 pp.44 to 74).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcInfoField {
    /// A time code field (`01` to `0F`).
    TimeCode(MmcTimeCodeName, MmcTimeCode),
    /// The short form of a time code field (`21` to `2F`).
    ShortTimeCode(MmcTimeCodeName, MmcShortTimeCode),
    /// SIGNATURE (`40`).
    Signature(MmcSignature),
    /// UPDATE RATE (`41`): the minimum interval between updates in frames.
    UpdateRate(U7),
    /// RESPONSE ERROR (`42`): the fields that were asked for and are not there.
    ResponseError(MmcNames),
    /// COMMAND ERROR (`43`).
    CommandError(MmcErrorReport),
    /// COMMAND ERROR LEVEL (`44`): `00` all disabled, `7F` all enabled.
    CommandErrorLevel(U7),
    /// TIME STANDARD (`45`).
    TimeStandard(MtcRate),
    /// SELECTED TIME CODE SOURCE (`46`).
    TimeCodeSource(MmcTimeCodeSource),
    /// SELECTED TIME CODE USERBITS (`47`).
    TimeCodeUserBits(MmcUserBits),
    /// MOTION CONTROL TALLY (`48`).
    MotionControlTally(MotionControlTally),
    /// VELOCITY TALLY (`49`).
    VelocityTally(MmcSpeed),
    /// STOP MODE (`4A`).
    StopMode(MmcStopMode),
    /// FAST MODE (`4B`).
    FastMode(MmcFastMode),
    /// RECORD MODE (`4C`).
    RecordMode(MmcRecordMode),
    /// RECORD STATUS (`4D`).
    RecordStatus(MmcRecordStatus),
    /// TRACK RECORD STATUS (`4E`).
    TrackRecordStatus(MmcTrackBitmap),
    /// TRACK RECORD READY (`4F`).
    TrackRecordReady(MmcTrackBitmap),
    /// GLOBAL MONITOR (`50`).
    GlobalMonitor(MmcGlobalMonitor),
    /// RECORD MONITOR (`51`).
    RecordMonitor(MmcRecordMonitor),
    /// TRACK SYNC MONITOR (`52`).
    TrackSyncMonitor(MmcTrackBitmap),
    /// TRACK INPUT MONITOR (`53`).
    TrackInputMonitor(MmcTrackBitmap),
    /// STEP LENGTH (`54`): hundredths of a frame.
    StepLength(U7),
    /// PLAY SPEED REFERENCE (`55`).
    PlaySpeedReference(MmcPlaySpeedReference),
    /// FIXED SPEED (`56`): `3F` next lower, `40` medium, `41` next higher,
    /// `7F` as selected locally.
    FixedSpeed(U7),
    /// LIFTER DEFEAT (`57`).
    LifterDefeat(MmcLifterDefeat),
    /// CONTROL DISABLE (`58`).
    ControlDisable(MmcControlDisable),
    /// RESOLVED PLAY MODE (`59`).
    ResolvedPlayMode(MmcResolvedPlayMode),
    /// CHASE MODE (`5A`).
    ChaseMode(MmcChaseMode),
    /// GENERATOR COMMAND TALLY (`5B`).
    GeneratorCommandTally(GeneratorTally),
    /// GENERATOR SET UP (`5C`).
    GeneratorSetUp(GeneratorSetUp),
    /// GENERATOR USERBITS (`5D`).
    GeneratorUserBits(MmcUserBits),
    /// MIDI TIME CODE COMMAND TALLY (`5E`).
    MtcCommandTally(MtcTally),
    /// MIDI TIME CODE SET UP (`5F`).
    MtcSetUp(MtcSetUp),
    /// PROCEDURE RESPONSE (`60`).
    ProcedureResponse(MmcProcedureResponse),
    /// EVENT RESPONSE (`61`).
    EventResponse(MmcEventResponse),
    /// TRACK MUTE (`62`).
    TrackMute(MmcTrackBitmap),
    /// VITC INSERT ENABLE (`63`).
    VitcInsertEnable(VitcInsert),
    /// RESPONSE SEGMENT (`64`).
    ResponseSegment(MmcSegment),
    /// FAILURE (`65`): ASCII text for the controller's display.
    Failure(MmcFragment),
    /// WAIT (`7C`): hold transmissions.
    Wait,
    /// RESUME (`7F`): transmit again.
    Resume,
    /// A field the specification leaves undefined, skipped by its length.
    Other(MmcOtherField),
}
