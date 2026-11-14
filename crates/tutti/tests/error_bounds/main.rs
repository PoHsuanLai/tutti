//! Every error type the umbrella reaches is an `Error` a caller can box and move.
//!
//! `?` into `Box<dyn Error + Send + Sync>` or anyhow's error type needs
//! `Error + Send + Sync + 'static`; the check is the bound, so a type that gains
//! a non-`Send` field stops compiling here. A feature-gated type is held where
//! the umbrella exposes it.
//!
//! The functions are never called: the compiler checks each bound where the
//! function is written, so the failure is a build error, not a test result.
#![allow(dead_code)]

use std::error::Error;

fn err<T: Error + Send + Sync + 'static>() {}

fn types_errors_are_boxable() {
    use tutti::types::{
        BeatsPerCycleOutOfRange, BpmOutOfRange, BusTooWide, ChannelCountOutOfRange,
        DepthOutOfRange, InvalidBeatsPerBar, InvalidMaxBlock, InvalidNoteValue, LoopRangeError,
        NotOnMidiScale, NoteNumberOutOfRange, SampleRateOutOfRange, SinkFailed,
        UnitParamOutOfRange,
    };
    err::<BeatsPerCycleOutOfRange>();
    err::<BpmOutOfRange>();
    err::<BusTooWide>();
    err::<ChannelCountOutOfRange>();
    err::<DepthOutOfRange>();
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

fn buffers_errors_are_boxable() {
    err::<tutti::buffers::BufferError>();
    err::<tutti::buffers::FoldError>();
    err::<tutti::buffers::I24OutOfRange>();
}

fn rt_transport_and_engine_errors_are_boxable() {
    err::<tutti::rt::ClaimConflict>();
    err::<tutti::rt::Contended>();
    err::<tutti::rt::RequestBitOutOfRange>();
    err::<tutti::transport::AudioTaken>();
    err::<tutti::transport::QueueFull>();
    err::<tutti::transport::ScheduleFull>();
    err::<tutti::engine::GraphEngineError>();
}

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

fn session_errors_are_boxable() {
    err::<tutti::Error>();
    err::<tutti::session::TempoError>();
}

#[cfg(feature = "device")]
fn device_errors_are_boxable() {
    err::<tutti::session::RestartError>();
}

#[cfg(feature = "modulation")]
fn modulation_errors_are_boxable() {
    err::<tutti::modulation::CompileError>();
    err::<tutti::modulation::ProgramError>();
    err::<tutti::session::ModRefusal>();
    err::<tutti::session::Problem>();
}

#[cfg(feature = "sampler")]
fn sampler_errors_are_boxable() {
    err::<tutti::sampler::StreamerGone>();
    err::<tutti::sampler::StreamingAddError>();
}

#[cfg(feature = "plugin")]
fn recovery_errors_are_boxable() {
    err::<tutti::session::RecoveryFailure>();
    err::<tutti::session::ReloadCause>();
}
