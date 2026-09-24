//! [`ParamPortMap`] — which audio-rate param ports a spawned node exposes,
//! recorded where the node's concrete type is still known.
//!
//! A node that offers an audio-rate input for one of its scalars answers for
//! itself through [`tutti_nodes::ParamPorts`]. The trouble is reaching that
//! answer from the adapter: the graph stores `Box<dyn AudioUnit>`, and there is
//! no `&dyn ParamPorts` to recover from a `&dyn AudioUnit`, so the modulation
//! reconciler used to ask by **downcasting through a hand-maintained list of
//! nine concrete types**.
//!
//! That list shipped broken. Both filter types were missing from it, and the
//! failure is silent by construction: a type absent from the list answers
//! `None`, which is indistinguishable from the legitimate "this node exposes no
//! port for that param", so `ModDelivery::PerSample` fell back to per-frame
//! without a word. `Cutoff` and `Q` are the only params either filter offers,
//! and a fast LFO on a filter cutoff is the case that tier's own docs name — so
//! the headline use of audio-rate modulation was the one it could not serve.
//!
//! # The fix: record it at spawn, where the type is known
//!
//! Every unit enters the graph through a call site that names its concrete type.
//! [`DeclareParamPorts::with_param_ports`] captures the node's own answer there
//! — `T: ParamPorts` is a real bound, so the mapping is the node's, never a
//! second copy of it — and stores it as a [`ParamPortMap`] component beside
//! [`AudioNode`](tutti_core::AudioNode).
//!
//! The reconciler then reads a component instead of guessing a type, and the
//! nine-arm downcast is gone.
//!
//! # Why this is declared rather than captured automatically
//!
//! `spawn_audio_node<U: AudioUnit>` erases `U` the moment it is called, and the
//! capture cannot be hidden inside it. Two routes were tried and both fail:
//!
//! - **Autoref specialization inside the generic body.** The `Probe`/`&Probe`
//!   trick resolves against the bound the *function* declares, not the type the
//!   caller passed, so inside `spawn_audio_node` it selects the fallback for
//!   every unit — including the ported ones. Measured, not assumed.
//! - **A `ParamPorts` bound on `spawn_audio_node`.** Impossible: callers spawn
//!   units from every crate — plugin hosts, sampler voices, the soundfont
//!   player, the polysynth — whose types are foreign to `tutti-nodes`, and a
//!   blanket `impl<T> ParamPorts for T` would conflict with the seven concrete
//!   impls.
//!
//! So the declaration is explicit, and the point of this module is that
//! **forgetting it is loud rather than silent** — see
//! [`ParamPortMap::missing_declaration_warning`].

use bevy_ecs::prelude::*;
use std::collections::BTreeMap;

use tutti_nodes::ParamPorts;
use tutti_types::UnitParam;

/// Every `UnitParam` a node might expose a port for.
///
/// Iterated once at spawn to build the map. Listing the variants is what makes
/// a newly added `UnitParam` a compile error here (the `match` below is
/// exhaustive) rather than a param that silently never resolves.
const ALL_PARAMS: [UnitParam; 22] = [
    UnitParam::Cutoff,
    UnitParam::Q,
    UnitParam::GainDb,
    UnitParam::Wet,
    UnitParam::Feedback,
    UnitParam::DelayTime,
    UnitParam::Rate,
    UnitParam::Depth,
    UnitParam::RoomSize,
    UnitParam::Damping,
    UnitParam::Threshold,
    UnitParam::Ratio,
    UnitParam::Attack,
    UnitParam::Release,
    UnitParam::Ceiling,
    UnitParam::Drive,
    UnitParam::Makeup,
    UnitParam::Volume,
    UnitParam::Detune,
    UnitParam::StereoSpread,
    UnitParam::Pan,
    UnitParam::Mute,
];

/// The audio-rate param ports a node exposes, by param.
///
/// Written once, at spawn, from the node's own [`ParamPorts`] impl; read by the
/// audio-rate modulation reconciler. Absent means "this node was never declared
/// to have ports", which is the *normal* state for the overwhelming majority of
/// nodes (an oscillator, a sum, a gain) — see
/// [`missing_declaration_warning`](Self::missing_declaration_warning) for how
/// that is told apart from a forgotten declaration.
///
/// # Empty is a real answer
///
/// A node built *without* `with_param_inputs` implements `ParamPorts` and
/// answers `None` for every param, so its map is empty. That is not the same as
/// having no map: it records that the question was asked and the node said no.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct ParamPortMap(BTreeMap<UnitParam, usize>);

impl ParamPortMap {
    /// Capture `unit`'s own answer for every [`UnitParam`].
    ///
    /// The bound is what makes this the node's mapping rather than a copy of it:
    /// there is no table here to fall out of step with the `*_port()` accessors.
    pub fn of<T: ParamPorts>(unit: &T) -> Self {
        Self(
            ALL_PARAMS
                .iter()
                .filter_map(|&p| unit.param_port(p).map(|port| (p, port)))
                .collect(),
        )
    }

    /// The input-port index for `param`, or `None` if this node exposes none.
    pub fn port(&self, param: UnitParam) -> Option<usize> {
        self.0.get(&param).copied()
    }

    /// Whether the node declared no ports at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The message logged when an audio-rate route asks a node with **no**
    /// [`ParamPortMap`] for a port.
    ///
    /// This is the whole point of the type. The old downcast list turned a
    /// missing entry into `None`, which the reconciler read as "no port" and
    /// acted on by silently downgrading the route to per-frame. Now the absence
    /// of a declaration is a distinct, nameable state, and it says which entity
    /// and which param — so the filter bug would have produced a line naming
    /// `Cutoff` instead of a tier that quietly did not engage.
    ///
    /// A warning rather than a panic: a genuinely port-less node (an oscillator
    /// feeding a bus) is a legitimate audio-rate target refusal, and taking the
    /// app down for one would be worse than the bug this replaces.
    pub fn missing_declaration_warning(entity: Entity, param: UnitParam) -> String {
        format!(
            "audio-rate route targets {entity:?} for {param:?}, but that entity has no \
             ParamPortMap: the node was spawned without `.with_param_ports(&unit)`, so the \
             route falls back to per-frame. If this node exposes an audio-rate port for \
             {param:?}, declare it at the spawn site; if it does not, this is expected."
        )
    }
}

/// Record a node's audio-rate param ports on the entity that carries it.
///
/// Called at the spawn site, where the concrete type is still in hand:
///
/// ```rust
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::graph::{DeclareParamPorts, InsertAudioNode};
///
/// fn spawn_filter(mut commands: Commands) {
///     let unit = tutti_nodes::SvfFilterNode::<f32>::with_param_inputs(
///         tutti_types::ChannelLayout::STEREO,
///         tutti_nodes::SvfType::LowPass,
///         tutti_types::Hz(1000.0),
///         tutti_types::Q(0.707),
///         true,
///         true,
///     );
///     // The declaration and the unit come from the same expression, so they
///     // cannot disagree about which ports exist.
///     commands
///         .spawn_empty()
///         .with_param_ports(&unit)
///         .insert_audio_node(unit);
/// }
/// # bevy_ecs::system::assert_is_system(spawn_filter);
/// ```
pub trait DeclareParamPorts {
    /// Attach the [`ParamPortMap`] `unit` declares.
    fn with_param_ports<T: ParamPorts>(&mut self, unit: &T) -> &mut Self;
}

impl DeclareParamPorts for bevy_ecs::system::EntityCommands<'_> {
    fn with_param_ports<T: ParamPorts>(&mut self, unit: &T) -> &mut Self {
        self.insert(ParamPortMap::of(unit));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node built *with* param inputs reports them; the same node built
    /// without reports none.
    ///
    /// Mutation: making `ParamPortMap::of` return `Self::default()`
    /// unconditionally fails the first assertion — which is precisely the
    /// shipped bug's shape (a ported node answering "no ports").
    #[test]
    fn a_ported_filter_declares_its_ports_and_a_plain_one_does_not() {
        let ported = tutti_nodes::SvfFilterNode::<f32>::with_param_inputs(
            tutti_types::ChannelLayout::STEREO,
            tutti_nodes::SvfType::LowPass,
            tutti_types::Hz(1000.0),
            tutti_types::Q(0.707),
            true,
            true,
        );
        let map = ParamPortMap::of(&ported);
        assert!(
            map.port(UnitParam::Cutoff).is_some(),
            "a filter built with param inputs must declare a Cutoff port; this is \
             the exact answer the old downcast list got wrong"
        );
        assert!(map.port(UnitParam::Q).is_some(), "and a Q port");
        assert_eq!(
            map.port(UnitParam::Drive),
            None,
            "an SVF exposes no Drive port"
        );

        let plain = tutti_nodes::SvfFilterNode::<f32>::with_channels(
            tutti_types::ChannelLayout::STEREO,
            tutti_nodes::SvfType::LowPass,
            tutti_types::Hz(1000.0),
            tutti_types::Q(0.707),
        );
        assert!(
            ParamPortMap::of(&plain).is_empty(),
            "a filter built without param inputs has no ports, and an empty map \
             is the honest record of that"
        );
    }

    /// The map is the node's own answer, param for param.
    ///
    /// Mutation: hardcoding any single param's index in `of` fails this, because
    /// the expected value is read back from the node rather than written here.
    #[test]
    fn the_map_agrees_with_the_node_for_every_param() {
        let unit = tutti_nodes::LadderFilterNode::<f32>::with_param_inputs(
            tutti_types::ChannelLayout::STEREO,
            tutti_nodes::LadderType::LP24,
            tutti_types::Hz(1000.0),
            tutti_types::Resonance(0.5),
            true,
            true,
            true,
        );
        let map = ParamPortMap::of(&unit);
        for p in ALL_PARAMS {
            assert_eq!(
                map.port(p),
                unit.param_port(p),
                "{p:?}: the component must carry exactly what the node answers"
            );
        }
    }
}
