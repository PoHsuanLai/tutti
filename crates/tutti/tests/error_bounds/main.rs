//! Every error type the umbrella reaches is an `Error` a caller can box and move.
//!
//! `?` into `Box<dyn Error + Send + Sync>` or `anyhow::Error` needs
//! `Error + Send + Sync + 'static`; the check is the bound, so a type that gains
//! a non-`Send` field stops compiling here. A feature-gated type is held where
//! the umbrella exposes it.

use std::error::Error;

fn err<T: Error + Send + Sync + 'static>() {}

#[test]
fn types_errors_are_boxable() {
    use tutti::types::{
        BeatsPerCycleOutOfRange, BpmOutOfRange, BusTooWide, ChannelCountOutOfRange,
        DepthOutOfRange, FoldError, InvalidBeatsPerBar, InvalidMaxBlock, InvalidNoteValue,
        LoopRangeError, NotOnMidiScale, NoteNumberOutOfRange, SampleRateOutOfRange, SinkFailed,
        UnitParamOutOfRange,
    };
    err::<BeatsPerCycleOutOfRange>();
    err::<BpmOutOfRange>();
    err::<BusTooWide>();
    err::<ChannelCountOutOfRange>();
    err::<DepthOutOfRange>();
    err::<FoldError>();
    err::<InvalidBeatsPerBar>();
    err::<InvalidMaxBlock>();
    err::<InvalidNoteValue>();
    err::<LoopRangeError>();
    err::<NotOnMidiScale>();
    err::<NoteNumberOutOfRange>();
    err::<SampleRateOutOfRange>();
    err::<SinkFailed>();
    err::<UnitParamOutOfRange>();
}

#[test]
fn rt_transport_and_engine_errors_are_boxable() {
    err::<tutti::rt::ClaimConflict>();
    err::<tutti::rt::Contended>();
    err::<tutti::rt::RequestBitOutOfRange>();
    err::<tutti::transport::AudioTaken>();
    err::<tutti::transport::QueueFull>();
    err::<tutti::transport::ScheduleFull>();
    err::<tutti::engine::GraphEngineError>();
}

#[test]
fn graph_errors_are_boxable() {
    use tutti::graph::{
        BusError, CloseError, CommitError, CompileError, EventCapacityError, EventOrderError,
        ForkError, ForkFault, MissingSource, MixError, ModeError, ParamPortsError, PassError,
        Refused, ScheduleError, TransportChangeRejected, VerifyError,
    };
    err::<BusError>();
    err::<CloseError>();
    err::<CommitError>();
    err::<CompileError>();
    err::<EventCapacityError>();
    err::<EventOrderError>();
    err::<ForkError>();
    err::<ForkFault>();
    err::<MissingSource>();
    err::<MixError>();
    err::<ModeError>();
    err::<ParamPortsError>();
    err::<PassError>();
    err::<Refused>();
    err::<ScheduleError>();
    err::<TransportChangeRejected>();
    err::<VerifyError>();
}

#[test]
fn session_errors_are_boxable() {
    err::<tutti::Error>();
    err::<tutti::session::TempoError>();
}

#[cfg(feature = "device")]
#[test]
fn device_errors_are_boxable() {
    err::<tutti::session::RestartError>();
}

#[cfg(feature = "modulation")]
#[test]
fn modulation_errors_are_boxable() {
    err::<tutti::modulation::CompileError>();
    err::<tutti::modulation::ProgramError>();
    err::<tutti::session::ModRefusal>();
    err::<tutti::session::Problem>();
}

#[cfg(feature = "sampler")]
#[test]
fn sampler_errors_are_boxable() {
    err::<tutti::sampler::StreamerGone>();
    err::<tutti::sampler::StreamingAddError>();
}

#[cfg(feature = "plugin")]
#[test]
fn recovery_errors_are_boxable() {
    err::<tutti::session::RecoveryFailure>();
    err::<tutti::session::ReloadCause>();
}
