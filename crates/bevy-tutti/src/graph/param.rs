//! One component per modulatable parameter, reconciled generically.
//!
//! [`AudioParam<U, const P: u16>`] is a scalar param on a node entity: `U` is
//! its unit, `P` its [`UnitParam`] address. Registering one with
//! [`add_audio_param`](AudioParamAppExt::add_audio_param) adds a system that
//! pushes changes into the graph — so a new param is one line, not a new
//! component type plus a `QueryData` field plus an `Or<Changed<…>>` arm plus an
//! `if let` branch.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::{AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin};
//! use bevy_tutti::AudioEngineState;
//! use tutti_core::SampleRate;
//! use tutti_types::{Drive, UnitParam};
//! use tutti_nodes::{DistortionNode, ShapeKind};
//!
//! /// One line per param — the unit and the address, both load-bearing.
//! type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;
//!
//! let unit = DistortionNode::new(ShapeKind::Tanh, 1.0);
//! // The node's own drive atomic — shared with every clone of the node, so it
//! // is the cell the DSP reads, wherever the graph keeps the unit.
//! let live = unit.drive();
//! // A graph with no device: its audio side stays here, and `render_frame`
//! // plays it.
//! let mut graph = AudioGraphRes::headless(0, 1);
//! // A `ParamNode`: its controls are its `ParamSet`, addressed on the node so
//! // a param write lands on its cell (what `spawn_audio_node` does).
//! let (node, params) = graph.insert(unit);
//! graph.set_node_params(node, Some(params));
//! graph.set_sample_rate(SampleRate(48_000.0));
//!
//! let mut app = App::new();
//! app.insert_resource(graph);
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins(GraphReconcilePlugin);
//! app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();
//!
//! let entity = app.world_mut().spawn((node, DriveParam::new(Drive(4.0)))).id();
//! app.update();
//! // The write lands on the cell, which the node reads at the start of its
//! // next block: render one.
//! app.world_mut().resource_mut::<AudioGraphRes>().render_frame(&mut [0.0]);
//!
//! // Read the node's own atomic — the cell the DSP reads, not the component.
//! assert_eq!(live.load(std::sync::atomic::Ordering::Acquire), 4.0);
//! ```
//!
//! # Why this can be generic at all
//!
//! Pushing a param needs no node-type dispatch:
//! [`AudioGraphRes::set_param`] writes a `(param, value)` pair through the
//! node's [`ParamSet`](tutti_graph::ParamSet), and a node without that param
//! takes nothing.
//!
//! # Modulated params
//!
//! A modulated param has one writer too (doc 014 §8.2): the write is its
//! **knob**. Its control lane reads the same cell as its base and adds the
//! modulation in the lane's own events, so nothing else writes the cell and
//! the knob is never snapped back.

use bevy_app::{App, Update};
use bevy_ecs::prelude::*;

use tutti_core::AudioNode;
use tutti_types::{Unit, UnitParam};

use crate::graph::{engine_ready, AudioGraphRes, GraphReconcileSystems};

/// A scalar parameter on a node entity: unit `U`, address `P`.
///
/// `P` is a [`UnitParam`] discriminant rather than the enum itself because
/// const generics cannot yet be arbitrary enums. Spell it
/// `{ UnitParam::Cutoff as u16 }` at the use site; [`param`](Self::param)
/// converts back.
///
/// Both parameters are load-bearing. `U` keeps a cutoff in `Hz` from being
/// assigned seconds, and `P` keeps two params of the same unit — a filter's
/// cutoff and an LFO's rate are both `Hz` — from being confused for one
/// another. Either alone would let one of those through.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct AudioParam<U: Unit<Raw = f32>, const P: u16> {
    /// The value, in `U`: the param's knob. Written by the host; the
    /// reconciler writes it to the node ([`write_param`]).
    pub value: U,
}

impl<U: Unit<Raw = f32>, const P: u16> AudioParam<U, P> {
    /// Creates a param holding `value`.
    pub fn new(value: U) -> Self {
        Self { value }
    }

    /// This param's address, or `None` if `P` is not a known [`UnitParam`].
    ///
    /// `None` is unreachable for a param declared with the
    /// `{ UnitParam::X as u16 }` idiom; it stays total rather than panicking so
    /// a stale discriminant degrades to "this param never reaches the graph"
    /// instead of taking the app down.
    pub fn param(self) -> Option<UnitParam> {
        UnitParam::try_from(P).ok()
    }
}

impl<U: Unit<Raw = f32> + Default, const P: u16> Default for AudioParam<U, P> {
    fn default() -> Self {
        Self::new(U::default())
    }
}

/// Writes one scalar to `param` on `node`: the param's knob.
///
/// The one path by which a value reaches a node's param. It is public for
/// params whose address is known only at runtime, which an [`AudioParam`]
/// cannot carry; the [`AudioParam`] reconciler calls it too. The value goes
/// to the node's own control ([`AudioGraphRes::set_param`]), and a fork of the
/// node starts from it. A modulated param is no different: its control lane
/// reads that control as its knob and adds the modulation to it (doc 014
/// §8.2, "one writer per knob").
///
/// Call it on the main thread (from a system); the node reads the value at the
/// start of its next block.
///
/// Hosted plugin parameters do not go through here: they are
/// runtime-discovered `u32` ids, and a modulated one's knob is its lane's
/// (`ModulationState::set_knob`).
pub fn write_param(graph: &mut AudioGraphRes, node: &AudioNode, param: UnitParam, value: f32) {
    // Through the node's `ParamSet`, never a handle captured from the unit:
    // that would move the live unit and leave a fork at the value it was
    // built with.
    graph.set_param(*node, param, value);
}

/// Pushes every changed [`AudioParam<U, P>`] into its node.
///
/// The system [`add_audio_param`](AudioParamAppExt::add_audio_param)
/// schedules, in [`GraphReconcileSystems::Params`]. Change-detection-gated, so
/// a steady frame does no work. Each write goes through [`write_param`].
#[allow(
    clippy::type_complexity,
    reason = "Bevy queries are tuple-shaped by design"
)]
pub fn reconcile_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
    mut graph: ResMut<AudioGraphRes>,
    changed: Query<(&AudioNode, &AudioParam<U, P>), Changed<AudioParam<U, P>>>,
) {
    let Ok(param) = UnitParam::try_from(P) else {
        return;
    };
    for (node, value) in &changed {
        write_param(&mut graph, node, param, value.value.to_raw());
    }
}

/// Which `AudioParam<U, P>` reconcilers are already scheduled.
///
/// `add_systems` does not deduplicate, so without this a param declared by both
/// a host and a library plugin would push its value twice per frame. Harmless
/// for an idempotent store, but it doubles the per-frame cost of every shared
/// param and makes the schedule depend on how many callers happened to ask.
#[derive(Resource, Default)]
struct RegisteredAudioParams(std::collections::HashSet<(core::any::TypeId, u16)>);

/// Registers the reconciler for one [`AudioParam`] type.
pub trait AudioParamAppExt {
    /// Schedules [`reconcile_audio_param`] for `AudioParam<U, P>`, so the
    /// component reaches the graph every frame it changes.
    ///
    /// Idempotent — registering the same `(U, P)` twice schedules one system —
    /// so a host and a library plugin can both declare a param they share.
    fn add_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
        &mut self,
    ) -> &mut Self;
}

impl AudioParamAppExt for App {
    fn add_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
        &mut self,
    ) -> &mut Self {
        let key = (core::any::TypeId::of::<U>(), P);
        if !self
            .world_mut()
            .get_resource_or_init::<RegisteredAudioParams>()
            .0
            .insert(key)
        {
            return self;
        }
        self.add_systems(
            Update,
            reconcile_audio_param::<U, P>
                .in_set(GraphReconcileSystems::Params)
                .run_if(engine_ready),
        )
    }
}
