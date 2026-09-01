//! Tell hosted plugins when a bounce is rendering them, and when it has stopped.
//!
//! # The gap this closes
//!
//! A plugin may render differently without realtime pressure — a convolution
//! reverb switching to a longer partition, an oversampling saturator raising its
//! factor. Every format carries the request (VST3 `ProcessSetup::processMode`,
//! CLAP `clap.render`, AU `kAudioUnitProperty_OfflineRender`, VST2
//! `audioMasterGetCurrentProcessLevel`), and `tutti-plugin` carries it all the
//! way to `PluginHandle::set_render_mode`.
//!
//! Nothing below this module calls it. `tutti-export` renders a `Net` and has
//! never heard of a plugin; [`crate::export`] queries entities by the generic
//! `AudioNode`. Without this system a bounce renders every hosted plugin in its
//! live-quality mode and writes that into the file the user asked to be exact —
//! silently, because a plugin that is never told simply keeps doing what it was
//! doing.
//!
//! # Why a resource and not a component on the request
//!
//! The obvious shape — a marker on the export entity, removed when the render
//! finishes — cannot work, and the reason is worth stating because it is not
//! visible from the happy path.
//!
//! [`ExportInFlight::cancel`] drops the task, and despawning the request entity
//! does the same implicitly. Neither fires `ExportDone`, and neither is seen
//! by `poll_exports`. A guard component living on that entity would be
//! destroyed along with it, leaving every plugin in the session stuck in
//! `Offline` with nothing left to restore it.
//!
//! So the mode is owned by [`PluginRenderMode`], a resource, and restored by
//! observing that *no* export is in flight rather than by being told one ended.
//! That covers completion, failure, cancellation and despawn with one rule,
//! because all four have the same observable: the component is gone.
//!
//! # Ordering
//!
//! [`ExportPlugin`](crate::export::ExportPlugin) chains `poll_exports` before
//! `start_exports`. This system runs after both, so within one frame it sees the
//! settled answer: a render that finished and one that started in the same frame
//! leave `ExportInFlight` present, and the mode correctly stays `Offline`
//! instead of flapping between two back-to-back bounces.

use bevy_ecs::prelude::*;
use tutti_plugin::RenderMode;

use crate::export::ExportInFlight;
use crate::plugin_host::editor::PluginEmitter;

/// The render mode every hosted plugin was last told.
///
/// Not a copy of what each plugin thinks — that would be a second owner of
/// state the plugin already holds. This records *what this host last
/// announced*, which is the only thing that decides whether an announcement is
/// needed. A plugin that declines the mode (`set_render_mode` returning `false`)
/// does not change what was announced, so it is not re-told every frame.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PluginRenderMode(
    /// The mode last announced to hosted plugins. Written only by
    /// [`plugin_render_mode_drive`]; defaults to [`RenderMode::Realtime`].
    pub RenderMode,
);

/// Announce [`RenderMode::Offline`] while an export is in flight, and
/// [`RenderMode::Realtime`] whenever none is.
///
/// Edge-triggered: the announcement happens on the frames the answer *changes*,
/// not every frame. Three of the four formats can only take the mode while the
/// plugin is deactivated, so re-announcing an unchanged mode is not free — it is
/// a deactivate/reactivate cycle per plugin per frame.
///
/// A plugin loaded *during* a render is told on the frame it appears: the
/// per-plugin [`RenderModeAnnounced`] marker is consulted alongside the mode
/// change rather than instead of it — see `needs_announcement`. Without that,
/// a plugin that finished loading mid-bounce would render the rest of the file
/// in the wrong mode.
pub fn plugin_render_mode_drive(
    mut announced: ResMut<PluginRenderMode>,
    exporting: Query<(), With<ExportInFlight>>,
    plugins: Query<(Entity, &PluginEmitter, Has<RenderModeAnnounced>)>,
    mut commands: Commands,
) {
    let wanted = if exporting.is_empty() {
        RenderMode::Realtime
    } else {
        RenderMode::Offline
    };

    let mode_changed = announced.0 != wanted;

    for (entity, emitter, told) in plugins.iter() {
        if !needs_announcement(mode_changed, told) {
            continue;
        }
        // The return is deliberately discarded: `false` means the plugin
        // declined (a CLAP without `clap.render`), which is a statement that it
        // renders identically either way — not a failure to retry or report.
        let _ = emitter.handle.set_render_mode(wanted);
        commands.entity(entity).insert(RenderModeAnnounced);
    }

    announced.0 = wanted;
}

/// Whether a plugin needs telling, given whether the mode moved this frame and
/// whether this plugin has ever been told one.
///
/// Split out for the same reason [`super::latency::plugin_latency_poll`] splits
/// its rule: reaching the loop body needs a `PluginEmitter`, which needs a
/// `PluginHandle`, which needs a launched subprocess. Keeping the decision here
/// means it can be pinned without one — and this half is where the interesting
/// case lives.
///
/// `!told` is not redundant with `mode_changed`. A plugin that finishes loading
/// *during* a bounce arrives on a frame when the mode did not move, so a rule
/// keyed only on the change would leave it rendering the remainder of the file
/// in whatever mode it defaulted to.
fn needs_announcement(mode_changed: bool, told: bool) -> bool {
    mode_changed || !told
}

/// Marks a plugin as having been told a render mode at least once.
///
/// Distinguishes "the mode is `Realtime` and this plugin knows" from "the mode
/// is `Realtime` and this plugin has never been told anything" — identical
/// states to a comparison against [`PluginRenderMode`] alone, and only the
/// second needs an announcement.
#[derive(Component)]
pub struct RenderModeAnnounced;

#[cfg(test)]
mod tests {
    //! A `PluginEmitter` needs a `PluginHandle`, which needs a launched
    //! subprocess, so no test here can put a plugin in the world. These cover
    //! the two halves that do not need one: which mode a frame resolves to, and
    //! `needs_announcement`'s rule about who gets told.
    //!
    //! **Not covered here:** a `set_render_mode` call actually reaching a
    //! plugin. Deleting the call in the loop body leaves all seven of these
    //! green — they observe the decision, not the delivery. That half is covered
    //! one crate down, in `tutti-plugin`'s
    //! `tests/vst2_in_process_render_mode.rs`, which loads the reference probe
    //! and reads back the process level the plugin actually saw. Stated rather
    //! than implied, because a reader counting seven passing tests would
    //! otherwise assume this file covers the wire.

    use super::*;
    use bevy_app::prelude::*;

    /// An app running only [`plugin_render_mode_drive`], with the mode resource
    /// at its `Realtime` default.
    fn test_app() -> App {
        let mut app = App::new();
        app.init_resource::<PluginRenderMode>();
        app.add_systems(Update, plugin_render_mode_drive);
        app
    }

    /// With no export running the announced mode is `Realtime`.
    ///
    /// The default, and the state a session spends all its time in. Pinned
    /// because the whole feature is a departure from it.
    #[test]
    fn no_export_means_realtime() {
        let mut app = test_app();
        app.update();

        assert_eq!(
            app.world().resource::<PluginRenderMode>().0,
            RenderMode::Realtime
        );
    }

    /// An in-flight export moves the announced mode to `Offline`.
    #[test]
    fn an_in_flight_export_announces_offline() {
        let mut app = test_app();
        app.world_mut().spawn(dummy_in_flight());
        app.update();

        assert_eq!(
            app.world().resource::<PluginRenderMode>().0,
            RenderMode::Offline,
            "a render in flight must put hosted plugins in offline mode"
        );
    }

    /// A **cancelled** export restores `Realtime`.
    ///
    /// The case a component-on-the-request design gets wrong: `cancel` fires no
    /// `ExportDone` and `poll_exports` never sees the entity, so anything
    /// waiting to be *told* the render ended waits forever. Despawning has the
    /// same shape, which is why this asserts on despawn — the harsher of the
    /// two, since it destroys any component a guard might have lived on.
    #[test]
    fn a_cancelled_export_restores_realtime() {
        let mut app = test_app();
        let entity = app.world_mut().spawn(dummy_in_flight()).id();
        app.update();
        assert_eq!(
            app.world().resource::<PluginRenderMode>().0,
            RenderMode::Offline,
            "precondition: the render must have started"
        );

        app.world_mut().entity_mut(entity).despawn();
        app.update();

        assert_eq!(
            app.world().resource::<PluginRenderMode>().0,
            RenderMode::Realtime,
            "a despawned export leaves no ExportDone and no poll; the mode must \
             still come back, or every plugin stays offline for the session"
        );
    }

    /// Two back-to-back renders do not drop to `Realtime` between them.
    ///
    /// `poll_exports` is chained before `start_exports`, so a frame can contain
    /// both the end of one render and the start of the next. Keying on the
    /// presence of *any* `ExportInFlight` rather than on an end-of-render signal
    /// is what makes that frame read `Offline` throughout.
    #[test]
    fn back_to_back_renders_stay_offline() {
        let mut app = test_app();
        let first = app.world_mut().spawn(dummy_in_flight()).id();
        app.update();

        // The frame where one ends and the next begins.
        app.world_mut().entity_mut(first).despawn();
        app.world_mut().spawn(dummy_in_flight());
        app.update();

        assert_eq!(
            app.world().resource::<PluginRenderMode>().0,
            RenderMode::Offline,
            "the mode must not flap between two consecutive bounces"
        );
    }

    /// A plugin already told the current mode is not told again.
    ///
    /// The case that keeps this from re-announcing every frame: three of the
    /// four formats can only take the mode while deactivated, so a system
    /// without this check would run a deactivate/reactivate cycle per plugin per
    /// frame for the life of the session.
    #[test]
    fn an_unchanged_mode_is_not_re_announced() {
        assert!(!needs_announcement(false, true));
    }

    /// A plugin that has never been told is told, even on a frame when the mode
    /// did not move.
    ///
    /// This is the mid-render load: a plugin finishing its subprocess launch
    /// during a bounce arrives when `mode_changed` is already `false`. Keyed on
    /// the change alone, it would render the rest of the file in the wrong mode
    /// — and nothing downstream would report it, because a plugin that is never
    /// told simply keeps doing what it was doing.
    #[test]
    fn a_plugin_never_told_is_announced_to_even_without_a_change() {
        assert!(
            needs_announcement(false, false),
            "a plugin loaded mid-render must still be told the current mode"
        );
    }

    /// A mode change reaches every plugin, told or not.
    #[test]
    fn a_changed_mode_reaches_every_plugin() {
        assert!(needs_announcement(true, true), "already told, mode moved");
        assert!(needs_announcement(true, false), "never told, mode moved");
    }

    /// An `ExportInFlight` carrying a task that is already finished still counts.
    ///
    /// The component's *presence* is the signal, not the task's state — the
    /// export module removes it when reporting, and until then the render is
    /// still the thing being rendered.
    fn dummy_in_flight() -> ExportInFlight {
        ExportInFlight::new(
            bevy_tasks::AsyncComputeTaskPool::get_or_init(bevy_tasks::TaskPool::new)
                .spawn(async { Err(tutti_export::Error::InvalidConfig("test fixture".into())) }),
        )
    }
}
