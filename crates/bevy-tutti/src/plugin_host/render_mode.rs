//! Tell hosted plugins when a bounce is rendering them, and when it has
//! stopped: the session's `Phase::RenderMode`, each frame.
//!
//! A plugin may render differently without realtime pressure (a convolution
//! reverb switching to a longer partition, an oversampling saturator raising
//! its factor), and every format carries the request to
//! `PluginHandle::set_render_mode`. The session owns the answer (design 015
//! §4.3): `Offline` while any export's token lives, `Realtime` otherwise,
//! announced by `Session::export` before it returns and by
//! `Session::finish_export` after it drops the token. Bevy finishes with
//! `Session::finish_export_unannounced` instead and leaves the Realtime to
//! this system, after `start_exports`: a frame that finishes one export
//! and starts the next keeps its plugins Offline throughout.
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

/// Runs the session's `Phase::RenderMode`. Added by
/// [`ExportPlugin`](crate::export::ExportPlugin) (with `plugin`) after both
/// export systems, and ungated on the engine: a render in flight when audio
/// stops still has to put its plugins back. `poll_exports` finishes without
/// announcing, so this is what tells the plugins `Realtime` when the last
/// export of a queue ends.
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

    /// What the probe saw: the session's announced mode, read between
    /// `poll_exports` and `start_exports` each frame.
    #[derive(Resource, Default)]
    struct SeenAtHandOff(Vec<RenderMode>);

    fn probe(graph: Res<AudioGraphRes>, mut seen: ResMut<SeenAtHandOff>) {
        seen.0.push(graph.render_mode());
    }

    /// **Two back-to-back exports keep the plugins Offline across the
    /// hand-off.** `poll_exports` and `start_exports` run in one frame, so a
    /// frame can finish one export and start the next; `poll_exports`
    /// finishes without announcing (`Session::finish_export_unannounced`),
    /// and the probe between the two systems never reads Realtime while the
    /// queue drains. Two requests queued at once, over real session jobs,
    /// with `ExportPlugin`'s own scheduling (which adds this module's
    /// system after both).
    ///
    /// Mutation (run, failed): `poll_exports` finishing with the announcing
    /// `Session::finish_export` → the probe reads Realtime on the frame the
    /// first export finishes, before the second starts, and again when the
    /// second does (`[Realtime; 4]`: no Offline run at the hand-off).
    #[test]
    fn back_to_back_renders_stay_offline() {
        use crate::export::{ExportDone, ExportPlugin, ExportRequest as BevyRequest};
        use crate::graph::AudioConfig;
        use crate::AudioEngineState;

        let mut graph = AudioGraphRes::headless(0, 2);
        let (dc, _) = graph.insert(tutti_nodes::testing::Const::mono(0.0));
        graph.set_outputs_from(dc);
        let mut app = App::new();
        app.add_plugins(bevy_app::TaskPoolPlugin::default());
        app.insert_resource(graph);
        app.insert_resource(AudioConfig {
            sample_rate: tutti_core::SampleRate(48_000.0),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(ExportPlugin);
        app.init_resource::<SeenAtHandOff>();
        app.add_systems(
            Update,
            probe
                .after(crate::export::poll_exports)
                .before(crate::export::start_exports),
        );

        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..2 {
            let done = std::sync::Arc::clone(&done);
            app.world_mut()
                .spawn(BevyRequest::new(
                    crate::export::ExportSource::Master,
                    crate::export::ExportTarget::Buffers,
                    tutti_export::ExportConfig {
                        render: tutti_export::RenderConfig {
                            sample_rate: tutti_core::SampleRate(48_000.0),
                            duration_seconds: 0.01,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    crate::export::ExportClock::frozen(),
                ))
                .observe(move |d: On<ExportDone>| {
                    assert!(d.result.is_ok(), "the export failed: {:?}", d.result);
                    done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                });
        }
        for _ in 0..4000 {
            app.update();
            if done.load(std::sync::atomic::Ordering::SeqCst) == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(done.load(std::sync::atomic::Ordering::SeqCst), 2, "both export");
        app.update();
        assert_eq!(mode(&app), RenderMode::Realtime, "Realtime once both end");

        let seen = &app.world().resource::<SeenAtHandOff>().0;
        let offline_runs = seen
            .windows(2)
            .filter(|w| w[0] != RenderMode::Offline && w[1] == RenderMode::Offline)
            .count()
            + usize::from(seen.first() == Some(&RenderMode::Offline));
        assert_eq!(
            offline_runs, 1,
            "the mode must not flap between two consecutive bounces: {seen:?}"
        );
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
