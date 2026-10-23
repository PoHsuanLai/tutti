#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

mod compile;
mod curve;
mod program;
mod shape;
mod sources;

pub use audio_automation::CurveType;
pub use compile::{
    compile, layer_id, Adsr, CompileError, Compiled, Declarations, RouteDecl, RouteKey,
    RouteTarget, SignalKind, Source, SourceKey, SourceNode, TargetDecl, TargetKey,
    RATE_ANCHOR_MARGIN,
};
pub use curve::{Curve, CurveSegment};
pub use program::{
    Amount, Clock, ControlProgram, Corners, DepthBy, InputLayer, Layer, LayerId, PeriodPoint,
    ProgramError, ProgramTime, RateAnchor, SourceId, Wave, MAX_LANE_INPUTS, MAX_LANE_LAYERS,
};
pub use shape::{shape, LfoShape, Polarity};
pub use sources::{Follower, Lfo, LfoRate, Macro, NoteEnvelope};
