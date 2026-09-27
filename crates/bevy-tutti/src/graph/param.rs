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
//! // Under the `modulation` feature the reconciler asks the matrix whether a
//! // param has a second writer, so the plugin that owns it must be present.
//! #[cfg(feature = "modulation")]
//! app.add_plugins(bevy_tutti::modulation::TuttiModulationPlugin);
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
//! A param the control-rate modulation driver owns is not written directly:
//! the driver writes `base + Σ layers` into the same cell every frame, so a
//! plain write would snap back. The reconciler routes the authored value to
//! the accumulator's *base* instead (see [`write_param`]).

use bevy_app::{App, Update};
use bevy_ecs::prelude::*;

use tutti_core::AudioNode;
// `ParamAddr` only appears in the modulation-gated arm of the reconciler, which
// is where an authored value is handed to the accumulator's base instead of
// being written straight to the node.
#[cfg(feature = "modulation")]
use tutti_types::ParamAddr;
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
    /// The authored value, in `U`. Written by the host; read by the reconciler,
    /// which decides where it lands (see [`write_param`]).
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

/// Writes one authored scalar to `param` on `node`, respecting modulation.
///
/// The one path by which an authored value reaches the graph. It is public for
/// params whose address is known only at runtime, which an [`AudioParam`]
/// cannot carry; the [`AudioParam`] reconciler calls it too. The value goes:
///
/// 1. for a param **modulated at control rate**, to the modulation
///    accumulator's *base*, where it rides under the modulation. The driver
///    writes `clamp(base + Σ layers)` into the node's cell every frame, so a
///    direct write would snap back on the next frame. A fork of the node (an
///    export) starts from the base too;
/// 2. for any other param, including one **modulated at audio rate**, straight
///    to the node's own control ([`AudioGraphRes::set_param`]). The graph's
///    audio-rate modulation rides on that control as its base.
///
/// Call it on the main thread (from a system); the node reads the value at the
/// start of its next block. The `matrix` argument exists only with the
/// `modulation` feature.
///
/// Hosted plugin parameters do not go through here: they are
/// runtime-discovered `u32` ids with a transport of their own.
// Ordering against `modulation::drive` does not matter: `set_base` and the
// driver's `accumulate` touch different fields of the same mutex-guarded
// `LayeredCurve` and each recomputes the composite, so the writes commute.
pub fn write_param(
    graph: &mut AudioGraphRes,
    #[cfg(feature = "modulation")] matrix: &crate::modulation::ModulationMatrix,
    entity: Entity,
    node: &AudioNode,
    param: UnitParam,
    value: f32,
) {
    // 1. Control rate: the driver owns the atomic, so the value rides the base.
    // The node's fork snapshot gets the base too: the driver's live composite
    // never reaches a fork (an export runs its own modulation), and without
    // this the fork would start from the value the node was built with.
    #[cfg(feature = "modulation")]
    if matrix.set_base(entity, ParamAddr::Unit(param), value) {
        graph.set_param_snapshot(*node, param, value);
        return;
    }
    #[cfg(not(feature = "modulation"))]
    let _ = entity;

    // 2. Unmodulated, or modulated at audio rate (the graph's modulation rides
    // on this control): the node's own atomic is the value. Through the graph's
    // settings path — the node's ring, which is also
    // what its shadow (and so any fork of it) records. Never through a handle
    // captured from the unit: that would move the live unit and leave a fork
    // at the value it was built with.
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
    #[cfg(feature = "modulation")] matrix: Res<crate::modulation::ModulationMatrix>,
    changed: Query<(Entity, &AudioNode, &AudioParam<U, P>), Changed<AudioParam<U, P>>>,
) {
    let Ok(param) = UnitParam::try_from(P) else {
        return;
    };
    for (entity, node, value) in &changed {
        write_param(
            &mut graph,
            #[cfg(feature = "modulation")]
            &matrix,
            entity,
            node,
            param,
            value.value.to_raw(),
        );
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
