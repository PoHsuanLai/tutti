//! Tell hosted plugins when a bounce is rendering them, and when it has
//! stopped: the session's `Phase::RenderMode`, each frame.
//!
//! A plugin may render differently without realtime pressure (a convolution
//! reverb switching to a longer partition, an oversampling saturator raising
//! its factor), and every format carries the request to
//! `PluginHandle::set_render_mode`. The session owns the answer (design 015
//! §4.3): `Offline` while any export's token lives, `Realtime` otherwise,
//! announced by `Session::export` before it returns and by
//! `Session::finish_export` after it drops the token.
//!
//! What neither of those sees is an export that never finishes: an
//! [`ExportInFlight`](crate::export::ExportInFlight) cancelled or despawned
//! drops its task, and with it the token, but fires no `ExportDone` and no
//! `finish_export`. The mode is a level, so [`plugin_render_mode`] runs the
//! phase every frame: the frame after the token goes, every plugin is told
//! `Realtime`. A plugin added mid-export is told `Offline` the same way.

use bevy_ecs::prelude::*;
use tutti_session::Phase;

use crate::graph::AudioGraphRes;

/// Runs the session's `Phase::RenderMode`. Added (by
/// [`TuttiHostingPlugin`](super::TuttiHostingPlugin)) after both export
/// systems when [`ExportPlugin`](crate::export::ExportPlugin) is, and
/// ungated on the engine: a render in flight when audio stops still has to
/// put its plugins back.
///
/// **Pinned to the main thread** ([`NonSendMarker`](bevy_ecs::system::NonSendMarker))
/// with the session's other plugin calls, which design 015 §4.2 makes
/// main-thread calls. No format needs it for the announcement today (each
/// carries it on a lock or over IPC); the pin keeps a format that does from
/// being called on a pool thread.
pub fn plugin_render_mode(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
) {
    if let Some(mut graph) = graph {
        let _ = graph
            .bypass_change_detection()
            .run_phase(Phase::RenderMode, std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    //! The system over a real session and real export jobs (a `dc` node on
    //! the outputs; no plugin: the mode the session records is what is
    //! asserted). That the plugins hear it is pinned over the CLAP test
    //! plugin in `tutti-session`'s `tests/plugin.rs`.

    use super::*;
    use crate::export::ExportInFlight;
    use bevy_app::prelude::*;
    use tutti_plugin::RenderMode;
    use tutti_session::{ExportClock, ExportRequest, ExportSource, ExportTarget};

    fn test_app() -> App {
        let mut graph = AudioGraphRes::headless(0, 2);
        let (dc, _) = graph.insert(tutti_nodes::testing::Const::mono(0.0));
        graph.set_outputs_from(dc);
        let mut app = App::new();
        app.insert_resource(graph);
        app.add_systems(Update, plugin_render_mode);
        app
    }

    fn mode(app: &App) -> RenderMode {
        app.world().resource::<AudioGraphRes>().render_mode()
    }

    /// An export started through the session, its job on the pool as
    /// `start_exports` places it.
    fn in_flight(app: &mut App) -> ExportInFlight {
        let job = app
            .world_mut()
            .resource_mut::<AudioGraphRes>()
            .session()
            .export(ExportRequest::new(
                ExportSource::Mix,
                ExportTarget::Buffers,
                tutti_export::ExportConfig::default(),
                ExportClock::frozen(),
            ))
            .expect("the export starts");
        let task = bevy_tasks::AsyncComputeTaskPool::get_or_init(bevy_tasks::TaskPool::new)
            .spawn(async move { job.run() });
        ExportInFlight::for_test(task)
    }

    /// No export: `Realtime`.
    #[test]
    fn no_export_means_realtime() {
        let mut app = test_app();
        app.update();
        assert_eq!(mode(&app), RenderMode::Realtime);
    }

    /// An export in flight holds `Offline` across frames.
    ///
    /// Mutation (run, failed): `ExportState::in_flight` ignoring the token
    /// (`false`) → the system's phase announces `Realtime` on the update.
    #[test]
    fn an_in_flight_export_announces_offline() {
        let mut app = test_app();
        let export = in_flight(&mut app);
        app.world_mut().spawn(export);
        app.update();
        assert_eq!(
            mode(&app),
            RenderMode::Offline,
            "a render in flight must put hosted plugins in offline mode"
        );
    }

    /// A **cancelled** export restores `Realtime` on the next frame.
    ///
    /// `cancel` fires no `ExportDone` and `poll_exports` never sees the
    /// entity; despawning is the harsher shape, since it destroys any
    /// component a guard might have lived on. The token went with the task.
    ///
    /// Mutation (run, failed): the system not running the phase → the mode
    /// stays `Offline`.
    #[test]
    fn a_cancelled_export_restores_realtime() {
        let mut app = test_app();
        let export = in_flight(&mut app);
        let entity = app.world_mut().spawn(export).id();
        app.update();
        assert_eq!(
            mode(&app),
            RenderMode::Offline,
            "precondition: the render must have started"
        );

        app.world_mut().entity_mut(entity).despawn();
        app.update();

        assert_eq!(
            mode(&app),
            RenderMode::Realtime,
            "a despawned export leaves no ExportDone and no poll; the mode must \
             still come back, or every plugin stays offline for the session"
        );
    }
}
