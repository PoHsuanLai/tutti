#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

mod compile;
mod curve;
mod program;
mod shape;
mod sources;

pub use audio_automation::CurveType;
pub use compile::{
    CompileError, Compiled, Declarations, RATE_ANCHOR_MARGIN, RouteDecl, RouteKey, RouteTarget,
    SignalKind, Source, SourceKey, SourceNode, TargetDecl, TargetKey, compile, layer_id,
};
pub use curve::{Curve, CurveSegment};
pub use program::{
    Amount, Clock, ControlProgram, Corners, DepthBy, InputLayer, Layer, LayerId, MAX_LANE_INPUTS,
    MAX_LANE_LAYERS, PeriodPoint, ProgramError, ProgramTime, RateAnchor, SourceId, Wave,
};
pub use shape::{LfoShape, Polarity, shape};
pub use sources::{Follower, Lfo, LfoRate, Macro, NoteEnvelope};
pub use tutti_types::Adsr;
