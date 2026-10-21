//! When reconcile work runs, and whether it runs at all.
//!
//! The ordering anchors ([`GraphReconcileSystems`], and [`SessionPhaseSystems`]
//! chained from the session's `PHASES`) and the run-condition every
//! engine-touching system is gated on ([`engine_ready`]).
//!
//! Whether the frame ends in a commit is the session's edit flag, not
//! state here: every edit through [`AudioGraphRes`](super::AudioGraphRes)
//! sets it, and the frame's commit (the session's `Commit` phase) sends the
//! frame's edits at once, iff it is set.

use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::SystemSet;
use tutti_session::{Phase, PHASES};

use crate::AudioEngineState;

/// System-set ordering anchor for the reconcile pipeline.
///
/// Apps can schedule their own systems against these sets. The plugin
/// runs them in the order: `Spawn` → `Params` → `Despawn` → `Compensate` →
/// `Commit`, all inside `Update`.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum GraphReconcileSystems {
    /// Initial spawn of new graph nodes (rare; mostly app-driven via
    /// [`SpawnAudioNode`](super::SpawnAudioNode)). Apps can hook here to
    /// populate parameter components on the same frame the node is created.
    Spawn,
    /// Parameter-component changes are written into the graph here.
    Params,
    /// Reserved for despawn-phase work hosts may want to order here.
    /// Graph-node removal itself is an `On<Remove, AudioNode>` observer
    /// ([`reconcile_node_despawn`](super::reconcile_node_despawn)), which fires
    /// at command-flush time rather than in this set.
    Despawn,
    /// Work that must see the frame's final topology before it is committed.
    ///
    /// **Empty by default.** The graph compensates every commit itself, and
    /// `commit_graph` publishes the figures; the only system this crate puts
    /// here is [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin)'s
    /// debug cross-check, when a host adds it.
    Compensate,
    /// The session's `Commit` phase: one commit, iff any edit was made
    /// since the last.
    Commit,
}

/// The systems that run one of the session's phases
/// (`tutti_session::Session::run_phase`), one set per [`Phase`].
///
/// A headless session runs its phases in `tutti_session::PHASES` order in one
/// `update()`; a Bevy app runs each from its own system, and these sets are
/// how the frame keeps the same order:
/// [`GraphReconcilePlugin`](crate::graph::GraphReconcilePlugin) chains them
/// from `PHASES` itself (`configure_phases`), so the order is the constant's
/// and cannot drift from it. Each is also placed against [`GraphReconcileSystems`]:
///
/// - `Collect` ([`collect_graph`](super::commit::collect_graph)): before
///   `Spawn`, first in the frame;
/// - `Plugins` (`plugin_health_poll`, feature `plugin`): in `Spawn`, after
///   a load's promotion;
/// - `RenderMode` (`plugin_render_mode`, features `plugin` and `export`)
///   and `Modulation` (`ModulationSystems::Apply`, feature `modulation`):
///   after `Spawn`, before `Params`;
/// - `Commit` ([`commit_graph`](super::commit_graph)): in `Commit`;
/// - `AfterCommit` (`ModulationSystems::AfterCommit`), `MidiOut`
///   (`pump_clock_out_system`, feature `midi`), `Poll`
///   (`device_fault_poll`, added by
///   [`TuttiPlugin`](crate::TuttiPlugin)) and `Reclaim`
///   ([`reclaim_graph`](super::reclaim_graph)): after `Commit`.
#[derive(SystemSet, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct SessionPhaseSystems(pub Phase);

/// Chains [`SessionPhaseSystems`] in `PHASES` order and places each against
/// [`GraphReconcileSystems`] (see [`SessionPhaseSystems`]). Called by
/// [`GraphReconcilePlugin`](crate::graph::GraphReconcilePlugin).
pub(crate) fn configure_phases(app: &mut App) {
    for pair in PHASES.windows(2) {
        app.configure_sets(
            Update,
            SessionPhaseSystems(pair[0]).before(SessionPhaseSystems(pair[1])),
        );
    }
    for &phase in PHASES {
        let set = SessionPhaseSystems(phase);
        match phase {
            Phase::Collect => {
                app.configure_sets(Update, set.before(GraphReconcileSystems::Spawn));
            }
            Phase::Plugins => {
                app.configure_sets(Update, set.in_set(GraphReconcileSystems::Spawn));
            }
            Phase::RenderMode | Phase::Modulation => {
                app.configure_sets(
                    Update,
                    set.after(GraphReconcileSystems::Spawn)
                        .before(GraphReconcileSystems::Params),
                );
            }
            Phase::Commit => {
                app.configure_sets(Update, set.in_set(GraphReconcileSystems::Commit));
            }
            // `AfterCommit`, `MidiOut`, `Poll`, `Reclaim`, and a phase added later: after
            // the frame's commit, where the chain above already orders them.
            _ => {
                app.configure_sets(Update, set.after(GraphReconcileSystems::Commit));
            }
        }
    }
}

/// Run-condition: the audio engine is running.
///
/// Reads [`AudioEngineState`], which `build_into` sets to
/// [`Running`](AudioEngineState::Running) only after every engine resource
/// (`AudioGraphRes`, `TransportRes`, `MeteringRes`, `AudioConfig`, and the
/// feature subsystem resources) has been inserted. That happens synchronously
/// during plugin build, before frame 1, so a system gated on this can take its
/// engine resources as plain `Res`/`ResMut` rather than `Option<Res<_>>` plus a
/// `let Some(..) else` guard — it simply does not run when there is no engine
/// (the idiomatic Bevy shape, mirroring `bevy_audio`'s
/// `audio_output_available`).
///
/// The state is the source of truth rather than `resource_exists::<AudioGraphRes>`
/// so that "is the engine up?" has one answer. `Option<Res<_>>` here also keeps
/// the condition usable in a `World` that never added
/// [`TuttiPlugin`](crate::TuttiPlugin) — a missing state reads as not-ready.
///
/// # What this does **not** guarantee
///
/// This reads a *state*, not the resources. `AudioEngineState::Running` is a
/// value any host can insert, and the crate's own tests and examples do exactly
/// that to exercise graph systems without a device. So a system gated here may
/// still run in a `World` where `build_into` never executed, and **every
/// resource `build_into` inserts can be absent**: `MetronomeRes`, `MeteringRes`,
/// `AudioTapRes`, `MidiEngineNodes`, `MpeModeRes`, `ClockMasterRes`,
/// `DiskStreamerRes`, `MidiIoRes`, and the compensation cell. Add `PluginsRes`,
/// which is inserted lazily and not by the engine block at all.
///
/// In Bevy 0.19 a missing `Res<T>` is a **parameter-validation failure that
/// panics the schedule**, not a skipped system — so a hard `Res` on any of the
/// above turns "this host did not build an engine" into a crash. Take them as
/// `Option<Res<_>>` and return early.
///
/// The safe ones are those inserted by the *same plugin* that schedules the
/// system, since a host cannot have one without the other. That is the real
/// test — not whether the engine is up, but who owns the insertion. Treating the
/// list above as short enough to take hard is how a host that adds
/// `TuttiMidiPlugin` without the full engine bootstrap gets a panic instead of a
/// no-op.
pub fn engine_ready(state: Option<Res<AudioEngineState>>) -> bool {
    state.is_some_and(|s| s.is_running())
}

#[cfg(test)]
mod tests {
    //! The adapter runs the session's phases in `PHASES` order (design 015
    //! §4.4): `Session::update`'s order, one system per phase.

    use bevy_app::prelude::*;
    use tutti_session::{Phase, PHASES};

    use crate::graph::{AudioGraphRes, GraphReconcilePlugin};
    use crate::AudioEngineState;

    /// Where `phase` is in `PHASES`.
    fn rank(phase: Phase) -> usize {
        PHASES
            .iter()
            .position(|&p| p == phase)
            .expect("every phase is in PHASES")
    }

    /// Each frame runs the session's phases in `PHASES` order, each at
    /// most once, and every phase the enabled features give a system runs:
    /// what the phase systems record through `AudioGraphRes::run_phase`,
    /// over the plugins `TuttiPlugin` adds (graph, plugin hosting, export,
    /// MIDI's clock out) and `TuttiModulationPlugin`.
    ///
    /// The order is `PHASES`' own (`rank`); only which phases have a system
    /// here is written down. `AfterCommit` runs only on a frame with work
    /// (`after_commit` skips an idle one), so a modulated filter is spawned
    /// before the first frame, whose target is new until that frame's
    /// commit.
    ///
    /// Before design 015 §9.7, main ran `Collect` in
    /// `commit_graph`, right before `Commit` (after `Plugins` and
    /// `Modulation`), and `MidiOut` unordered: this test failed there.
    ///
    /// Mutations (run with `full,plugin`, each failed, then reverted):
    /// - `collect_graph` in `SessionPhaseSystems(Phase::Commit)`, before
    ///   `commit_graph` (main's layout): `[Plugins, RenderMode, Modulation,
    ///   Collect, Commit, ..]`.
    /// - `ModulationSystems::AfterCommit` configured `before`
    ///   `GraphReconcileSystems::Commit`, outside its phase set:
    ///   `AfterCommit` before `Plugins`.
    /// - `windows(2)` → no pairs in `configure_phases` (the chain from
    ///   `PHASES` gone, the anchors to `GraphReconcileSystems` kept):
    ///   `AfterCommit` and `MidiOut`, both only after `Commit`, ran
    ///   `MidiOut` first (the plugins are added in the order that makes an
    ///   unordered pair run wrong).
    /// - `device_fault_poll` in no phase set: `Poll` before `Commit`.
    /// - `plugin_health_poll` in `SessionPhaseSystems(Phase::AfterCommit)`
    ///   instead of `Plugins`: it is also in `Spawn`, so the schedule
    ///   refuses the cycle and the first `update` panics.
    #[test]
    fn the_adapter_runs_the_session_phases_in_phases_order() {
        let mut app = App::new();
        app.add_plugins(bevy_app::TaskPoolPlugin::default());
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(crate::graph::AudioConfig {
            sample_rate: tutti_core::SampleRate(48_000.0),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.add_plugins(GraphReconcilePlugin);
        app.add_plugins(crate::engine::device_state::DeviceStatePlugin);
        #[cfg(feature = "modulation")]
        {
            use crate::graph::SpawnAudioNode;
            use crate::modulation::{Lfo, Mods, ParamRef, TuttiModulationPlugin};
            use bevy_ecs::prelude::*;
            use tutti_nodes::{SvfFilterNode, SvfType};
            use tutti_types::{Depth, Hz, UnitParam, Q};

            app.insert_resource(crate::graph::TransportRes(
                tutti_core::transport::Transport::new(48_000.0),
            ));
            app.add_plugins(TuttiModulationPlugin);
            let filter = app
                .world_mut()
                .commands()
                .spawn_audio_node(SvfFilterNode::<f64>::new(
                    SvfType::LowPass,
                    Hz(1_000.0),
                    Q(0.7),
                ))
                .id();
            app.world_mut().flush();
            app.world_mut()
                .run_system_cached_with(
                    |In(filter): In<Entity>, mut mods: Mods| {
                        let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
                        mods.range(cutoff, 200.0, 4_000.0);
                        let lfo = mods.lfo(Lfo::sine().hz(Hz(1.0)));
                        mods.route(lfo, cutoff, Depth(0.3));
                    },
                    filter,
                )
                .expect("runs");
        }
        // After the modulation plugin, so that an unordered pair runs in
        // the wrong order by insertion: `Modulation` before `RenderMode`,
        // `AfterCommit` after `MidiOut`, unless the chain from `PHASES`
        // orders them.
        #[cfg(feature = "midi")]
        app.add_plugins(crate::midi::ClockOutPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(crate::plugin_host::TuttiHostingPlugin);
        #[cfg(feature = "export")]
        app.add_plugins(crate::export::ExportPlugin);

        // The phases with a system here, each frame.
        #[allow(unused_mut)]
        let mut every_frame = vec![Phase::Collect, Phase::Commit, Phase::Poll, Phase::Reclaim];
        #[cfg(feature = "plugin")]
        every_frame.push(Phase::Plugins);
        #[cfg(all(feature = "plugin", feature = "export"))]
        every_frame.push(Phase::RenderMode);
        #[cfg(feature = "modulation")]
        every_frame.push(Phase::Modulation);
        #[cfg(feature = "midi")]
        every_frame.push(Phase::MidiOut);

        #[cfg(feature = "modulation")]
        let mut seen = Vec::new();
        for frame in 0..3 {
            app.update();
            let ran = app
                .world_mut()
                .resource_mut::<AudioGraphRes>()
                .take_phase_log();
            assert!(
                ran.windows(2).all(|w| rank(w[0]) < rank(w[1])),
                "frame {frame}: {ran:?} is not in PHASES order ({PHASES:?})"
            );
            for phase in &every_frame {
                assert!(ran.contains(phase), "frame {frame}: {phase:?} did not run");
            }
            #[cfg(feature = "modulation")]
            seen.extend(ran);
        }
        #[cfg(feature = "modulation")]
        assert!(
            seen.contains(&Phase::AfterCommit),
            "a new target's frame runs AfterCommit"
        );
    }
}
