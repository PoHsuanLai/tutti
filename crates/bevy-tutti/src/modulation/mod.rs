//! Modulation as declarations (feature `modulation`; doc 014 §8).
//!
//! A source is a [`ModSource`] entity ([`Lfo`], [`Macro`], [`Follower`],
//! [`NoteEnvelope`], or a host's own [`ModSourceKind`]); a [`ModRoute`]
//! entity sends it into a param ([`ParamRef`]) by a depth of the param's
//! [`ModRange`]; an [`Automation`] gives a param a curve. [`Mods`] spawns
//! each in one call. Add [`TuttiModulationPlugin`] yourself: `TuttiPlugin`
//! does not.
//!
//! Every frame [`reconcile`] collects them and hands them, keyed by
//! entities, to the session's modulation phases (`tutti_session`'s
//! `Phase::Modulation` and `Phase::AfterCommit`), which apply them through
//! `tutti_nodes::Modulation`: each modulated param gets one control
//! lane, a graph node sending its value (`clamp((automation else knob) + Σ
//! offsets, range)`) into the param's port, sample-accurately on a 32-frame
//! grid; a follower or a note envelope is a node of its own. The lanes and
//! signal nodes are graph nodes, so an export forks them with the graph and
//! renders the same modulation as live.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, SpawnAudioNode, TransportRes};
//! use bevy_tutti::modulation::{Lfo, Mods, ParamRef, TuttiModulationPlugin};
//! use bevy_tutti::AudioEngineState;
//! use tutti_core::transport::Transport;
//! use tutti_core::SampleRate;
//! use tutti_nodes::{SvfFilterNode, SvfType};
//! use tutti_types::{BeatDuration, Depth, Hz, UnitParam, Q};
//!
//! let mut graph = AudioGraphRes::headless(0, 1);
//! graph.set_sample_rate(SampleRate(48_000.0));
//! let mut app = App::new();
//! app.insert_resource(graph);
//! app.insert_resource(TransportRes(Transport::new(48_000.0)));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
//!
//! let filter = app
//!     .world_mut()
//!     .commands()
//!     .spawn_audio_node(SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.7)))
//!     .id();
//! app.world_mut().flush();
//!
//! fn wobble(mut mods: Mods, filter: Single<Entity, With<tutti_core::AudioNode>>) {
//!     let cutoff = ParamRef::unit(*filter, UnitParam::Cutoff);
//!     mods.range(cutoff, 200.0, 4_000.0);
//!     let lfo = mods.lfo(Lfo::sine().rate(BeatDuration(0.5)));
//!     mods.route(lfo, cutoff, Depth(0.3));
//! }
//! app.world_mut().run_system_cached(wobble).unwrap();
//! app.update();
//!
//! // The cutoff has its lane, its knob the node's own cutoff cell.
//! let state = app.world().resource::<AudioGraphRes>().modulation();
//! let (_, lane) = state.lane(ParamRef::unit(filter, UnitParam::Cutoff)).unwrap();
//! assert_eq!(lane.knob(), Some(1_000.0));
//! ```
//!
//! # Knobs
//!
//! A modulated param's base is its **knob** where it has no automation: a
//! native param's knob is its node's own `Param` cell, written as any param
//! is ([`AudioParam`](crate::graph::AudioParam),
//! [`write_param`](crate::graph::write_param)); a plugin parameter's is its
//! lane's, started at the plugin's value and moved by an edit in the
//! plugin's GUI or by [`ModulationState::set_knob`]
//! ([`AudioGraphRes::modulation`](crate::graph::AudioGraphRes::modulation)).
//! Nothing else writes it
//! (doc 014 §8.2, "one writer per knob").
//!
//! # Takes
//!
//! An [`AutomationTake`] on an [`Automation`]'s entity suspends the
//! automation, so the knob (the gesture) plays, and on release its
//! envelope becomes the automation.
//!
//! # Errors
//!
//! A declaration the reconciler refuses (a follower whose input is
//! downstream of its own target, a route to a param with no range, a
//! target its node does not have) changes nothing; it is logged, sent as a
//! [`ModulationRefused`] message and held in [`ModulationState::refusal`]
//! until the declarations apply again. A target that stays unresolved for
//! four seconds (no node is ever bound to its entity) is logged, once.

mod declare;
mod reconcile;
#[cfg(test)]
mod tests;

pub use declare::{
    close_takes, Automation, AutomationTake, Follower, ModFeed, ModRange, ModRoute, ModSource,
    ModSourceKind, Mods, NoteEnvelope, ParamRef,
};
pub use reconcile::{after_commit, reconcile, Declared, ModulationRefused, ModulationState};
// The time-function builders are `tutti-mod`'s, declared as they are.
pub use tutti_mod::{Adsr, CurveType, Lfo, LfoRate, LfoShape, Macro, Polarity};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_session::Phase;

use crate::graph::{engine_ready, GraphReconcileSystems, SessionPhaseSystems};

/// Where the modulation systems run, for a host's own to order against.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ModulationSystems {
    /// [`close_takes`] then [`reconcile`]: after
    /// [`GraphReconcileSystems::Spawn`], before
    /// [`GraphReconcileSystems::Params`], in the session's `Modulation`
    /// phase ([`SessionPhaseSystems`]).
    Apply,
    /// [`after_commit`]: after [`GraphReconcileSystems::Commit`], in the
    /// session's `AfterCommit` phase.
    AfterCommit,
}

/// Adds modulation on control lanes: the [`ModulationRefused`] message and
/// the systems that hand the declarations to the session
/// ([`ModulationSystems`]). What they applied is
/// [`AudioGraphRes::modulation`](crate::graph::AudioGraphRes::modulation).
///
/// Not added by [`TuttiPlugin`](crate::TuttiPlugin); add it after it. Its
/// systems are gated on [`engine_ready`] and need
/// [`AudioGraphRes`](crate::graph::AudioGraphRes).
pub struct TuttiModulationPlugin;

impl Plugin for TuttiModulationPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ModulationRefused>();
        app.configure_sets(
            Update,
            (
                ModulationSystems::Apply
                    .after(GraphReconcileSystems::Spawn)
                    .before(GraphReconcileSystems::Params)
                    .in_set(SessionPhaseSystems(Phase::Modulation)),
                ModulationSystems::AfterCommit
                    .after(GraphReconcileSystems::Commit)
                    .in_set(SessionPhaseSystems(Phase::AfterCommit)),
            ),
        );
        app.add_systems(
            Update,
            (
                (close_takes, reconcile)
                    .chain()
                    .in_set(ModulationSystems::Apply),
                after_commit.in_set(ModulationSystems::AfterCommit),
            )
                .run_if(engine_ready),
        );
    }
}
