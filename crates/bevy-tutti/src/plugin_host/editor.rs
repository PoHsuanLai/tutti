//! Plugin GUI editor lifecycle: components + open/attach/idle/resize/close systems.

use bevy_ecs::prelude::*;
use bevy_log::warn;
use bevy_reflect::prelude::*;

use crate::plugin_host::native_window::attach_child_window;
use crate::plugin_host::PluginEditorMainThread;

/// Marks an entity as a loaded plugin with a control handle.
///
/// Inserted by [`plugin_load_promote`](crate::plugin_host::load::plugin_load_promote)
/// once the off-thread load resolves. The audio node is tracked separately via
/// `AudioNode`.
///
/// Not `Debug` / `Reflect`: `PluginHandle` wraps a foreign plugin-control
/// handle that implements neither.
#[derive(Component, Clone)]
pub struct PluginEmitter {
    /// The control-thread handle: parameters, editor open/close, state
    /// save/load. `Arc`-backed, so cloning it into a task is cheap.
    pub handle: tutti_plugin::handles::PluginHandle,
}

/// Present while a plugin's GUI editor is open in a separate Bevy window.
///
/// [`plugin_editor_idle_system`] calls `handle.editor_idle()` every frame for
/// entities carrying this.
///
/// Not `Reflect`: `EditorCapabilities` is a foreign type.
#[derive(Component)]
pub struct PluginEditorOpen {
    /// The Bevy `Window` entity this host spawned to hold the plugin's editor.
    pub editor_window: Entity,
    /// Editor width in logical pixels as reported by the plugin.
    pub width: u32,
    /// Editor height in logical pixels as reported by the plugin.
    pub height: u32,
    /// What the plugin says its editor supports — chiefly whether it is
    /// resizable, which decides whether the window gets resize constraints.
    pub capabilities: tutti_plugin::handles::EditorCapabilities,
    /// Last size written to either side, in logical pixels. A `WindowResized`
    /// matching this is an echo of this host's own write and is ignored.
    pub last_applied: (u32, u32),
    // The macOS AppKit live-resize observer is deliberately NOT a field here.
    // It owns a `Retained<NSView>` whose `Drop` calls `removeObserver`, and a
    // component's drop runs wherever a `Commands` queue is applied
    // (`plugin_health_poll` is not main-thread pinned) or wherever the `World`
    // is torn down — off-main AppKit is a hard crash. It lives in the
    // `NonSend` `LiveResizeRegistry` keyed by this plugin entity, so Bevy pins
    // every access and every drop to the main thread.
}

/// Present while a plugin's GUI editor is open in a window the **plugin** owns.
///
/// The floating counterpart to [`PluginEditorOpen`], and a separate component
/// rather than an `Option<Entity>` on that one. The difference is not a missing
/// field — every system keyed on `PluginEditorOpen` exists to manage a window
/// this host spawned (resize it, echo-suppress its `WindowResized`, despawn it
/// on close), and none of that applies to a window the host did not create. An
/// `Option` would make each of them carry a `None` arm for a case that is not
/// theirs; a separate component makes them simply not match.
///
/// What the two share — "an editor is open", which drives idle ticking — is
/// [`editor_is_open`].
///
/// Carries no size: the plugin owns the window, so there is no geometry here
/// for the host to apply.
#[derive(Component)]
pub struct PluginFloatingEditorOpen;

/// Whether this entity has an editor open, in either hosting mode.
///
/// The one question both components answer, named once so a caller does not
/// have to know there are two. Used by the idle pump, which must tick a
/// floating editor exactly as it ticks an embedded one — a plugin's GUI needs
/// its main-thread slice regardless of who owns the window.
pub fn editor_is_open(
    entity: Entity,
    embedded: &Query<&PluginEditorOpen>,
    floating: &Query<&PluginFloatingEditorOpen>,
) -> bool {
    embedded.get(entity).is_ok() || floating.get(entity).is_ok()
}

/// Intermediate state: a `Window` has been spawned but `open_editor` has not
/// been called yet, because the native handle does not exist until Bevy has
/// created the window.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component)]
pub struct PendingPluginEditor {
    /// The `Window` entity awaiting its native handle.
    pub window_entity: Entity,
}

/// Ask for a plugin's native GUI editor to be shown or hidden.
///
/// The one way to drive editor visibility:
///
/// ```ignore
/// commands.trigger(SetEditorVisible::show(entity));
/// commands.trigger(SetEditorVisible::hide(entity));
/// commands.trigger(SetEditorVisible::toggle(entity));   // menu item / double-click
/// ```
///
/// # Why one event and not a caller-side "am I open?" check
///
/// A caller cannot answer that question from outside: a
/// `Query<&PluginEditorOpen>` reads the previous frame, so a fast double-click
/// could open twice or close a window already gone. Resolving
/// [`Visibility::Toggle`] inside the observer, where `PluginEditorOpen` is
/// authoritative, makes that unrepresentable.
///
/// This event does not say whether an editor *is* open — that is
/// [`PluginEditorOpen`], and [`editor_is_open`] reads it.
#[derive(EntityEvent, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetEditorVisible {
    /// The plugin entity — the one carrying [`PluginEmitter`], not its window.
    pub entity: Entity,
    /// What to do with its editor.
    pub visibility: Visibility,
}

/// What [`SetEditorVisible`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Open the editor. A no-op if one is already open or pending.
    Show,
    /// Close the editor. A no-op if none is open.
    Hide,
    /// Whichever the editor is not right now, resolved inside the observer.
    Toggle,
}

impl SetEditorVisible {
    /// Ask for `entity`'s editor to be shown.
    pub fn show(entity: Entity) -> Self {
        Self {
            entity,
            visibility: Visibility::Show,
        }
    }

    /// Ask for `entity`'s editor to be closed.
    pub fn hide(entity: Entity) -> Self {
        Self {
            entity,
            visibility: Visibility::Hide,
        }
    }

    /// Flip `entity`'s editor between open and closed.
    pub fn toggle(entity: Entity) -> Self {
        Self {
            entity,
            visibility: Visibility::Toggle,
        }
    }
}

/// Ticks `editor_idle()` on every plugin with an editor open, so its GUI can
/// redraw and process events.
///
/// Runs in Bevy's `Update` schedule, and **main-thread pinned** by the
/// [`PluginEditorMainThread`] non-send param: plugin GUI frameworks assume it.
///
/// Ticked for **both** hosting modes — a plugin's GUI needs its main-thread
/// slice whether or not this host owns the window it draws into. Querying only
/// [`PluginEditorOpen`] would leave a floating editor unpumped, which presents
/// as a frozen UI rather than as a missing one.
///
/// One [`EditorOpenFilter`] rather than two systems, so the tick happens once
/// per entity even for a plugin that somehow carried both markers.
pub fn plugin_editor_idle_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    query: Query<&PluginEmitter, EditorOpenFilter>,
) {
    for emitter in query.iter() {
        emitter.handle.editor_idle();
    }
}

/// "Has an editor open, in either hosting mode", as a query filter.
///
/// Named rather than written inline at its one call site, because that call
/// site cannot be tested: it needs a `PluginEmitter`, which needs a launched
/// subprocess. The filter alone can be, and it is the half that carries the bug
/// — see the tests.
pub type EditorOpenFilter = Or<(With<PluginEditorOpen>, With<PluginFloatingEditorOpen>)>;

/// Observer: the one entry point for editor visibility.
///
/// Showing is phase 1 of two — it spawns the host `Window` and leaves
/// [`PendingPluginEditor`]; the native handle does not exist until Bevy has
/// created the window, so `plugin_editor_attach_system` finishes the job on a
/// later frame. Hiding closes the plugin's editor and despawns that window.
///
/// Both are no-ops when already in the requested state, which is what lets a
/// host fire `show` without first checking, and what makes a double `hide`
/// harmless.
pub fn set_editor_visible_observer(
    request: On<SetEditorVisible>,
    _main_thread: NonSend<PluginEditorMainThread>,
    // Main-thread-pinned: dropping the AppKit observer calls `removeObserver`,
    // which must not run off-main.
    #[cfg(target_os = "macos")] mut live_resize_registry: NonSendMut<
        crate::plugin_host::live_resize::LiveResizeRegistry,
    >,
    mut commands: Commands,
    plugins: Query<&PluginEmitter>,
    open: Query<&PluginEditorOpen>,
    pending: Query<&PendingPluginEditor>,
    floating: Query<&PluginFloatingEditorOpen>,
) {
    use bevy_window::{Window, WindowResolution};

    let entity = request.event_target();
    // Not a plugin, or one whose load failed. A host aiming a toggle at the
    // wrong entity is a mistake a log line per click would not help with.
    let Ok(emitter) = plugins.get(entity) else {
        return;
    };

    let showing =
        open.get(entity).is_ok() || pending.get(entity).is_ok() || floating.get(entity).is_ok();
    let want_visible = match request.visibility {
        Visibility::Show => true,
        Visibility::Hide => false,
        // Resolved here rather than by the caller: `PluginEditorOpen` is
        // current in an observer and a frame stale in a query.
        Visibility::Toggle => !showing,
    };

    if want_visible == showing {
        return;
    }

    if want_visible {
        // A plugin that owns its own window needs none from this host, and
        // there is no native handle to wait for — so this opens now rather
        // than going through `PendingPluginEditor`'s two-phase dance.
        //
        // Read from `Features` rather than from `EditorCapabilities`: this
        // decision happens *before* the editor exists, and capabilities are not
        // readable until after `open_editor` returns.
        if emitter
            .handle
            .loaded()
            .features
            .contains(tutti_plugin::Features::EDITOR_FLOATING)
        {
            match emitter.handle.open_floating_editor() {
                Ok(()) => {
                    commands.entity(entity).insert(PluginFloatingEditorOpen);
                    bevy_log::info!(
                        "Plugin '{}' floating editor opened (entity {entity:?})",
                        emitter.handle.name()
                    );
                }
                Err(e) => {
                    // No window was spawned, so there is nothing to clean up —
                    // unlike the embedded path, whose failure has to despawn the
                    // window it created.
                    bevy_log::warn!(
                        "Plugin '{}' floating editor failed to open: {e}",
                        emitter.handle.name()
                    );
                }
            }
            return;
        }

        // Spawned hidden: the plugin reports its real size when the editor
        // attaches, and showing it at 800x600 first would flash the wrong size.
        let window_entity = commands
            .spawn(Window {
                title: emitter.handle.name().to_string(),
                resolution: WindowResolution::new(800, 600),
                decorations: true,
                visible: false,
                ..Default::default()
            })
            .id();

        bevy_log::info!(
            "Spawning editor window for '{}' (window={window_entity:?})",
            emitter.handle.name(),
        );
        commands
            .entity(entity)
            .insert(PendingPluginEditor { window_entity });
        return;
    }

    // Hiding. A pending editor has a window but no native editor yet, so there
    // is nothing to close — just drop the window and the marker.
    if let Ok(pend) = pending.get(entity) {
        commands.entity(pend.window_entity).try_despawn();
        commands.entity(entity).remove::<PendingPluginEditor>();
        return;
    }

    // A floating editor: close the plugin's window, despawn nothing. There is
    // no `live_resize_registry` entry either — that observer is installed on a
    // window this host created, and no such window exists here.
    if floating.get(entity).is_ok() {
        emitter.handle.close_editor();
        commands.entity(entity).remove::<PluginFloatingEditorOpen>();
        bevy_log::info!(
            "Plugin '{}' floating editor closed (entity {entity:?})",
            emitter.handle.name()
        );
        return;
    }

    let Ok(editor) = open.get(entity) else {
        return;
    };
    // Tear the observer down here — on the main thread — rather than in a
    // component `Drop` that runs wherever the command queue is applied.
    #[cfg(target_os = "macos")]
    live_resize_registry.remove(entity);
    emitter.handle.close_editor();
    commands.entity(editor.editor_window).try_despawn();
    commands.entity(entity).remove::<PluginEditorOpen>();
    bevy_log::info!(
        "Plugin '{}' editor closed (entity {entity:?})",
        emitter.handle.name()
    );
}

/// Phase 2 of showing an embedded editor: once Bevy has created the window and
/// its native handle exists, call `open_editor` on the plugin, size the window
/// to what the plugin reports, and parent it to the primary window.
///
/// Main-thread pinned. On failure the window this host spawned is despawned and
/// [`PendingPluginEditor`] removed, so a refused editor leaves no orphan.
pub fn plugin_editor_attach_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    // `NonSendMut` pins this system — and therefore every observer install and
    // every observer drop — to the main thread, which AppKit requires.
    #[cfg(target_os = "macos")] mut live_resize_registry: NonSendMut<
        crate::plugin_host::live_resize::LiveResizeRegistry,
    >,
    mut commands: Commands,
    pending: Query<(Entity, &PluginEmitter, &PendingPluginEditor)>,
    mut windows: Query<&mut bevy_window::Window>,
    handles: Query<&bevy_window::RawHandleWrapper>,
    primary: Query<&bevy_window::RawHandleWrapper, With<bevy_window::PrimaryWindow>>,
) {
    for (entity, emitter, pend) in pending.iter() {
        let Ok(raw_handle) = handles.get(pend.window_entity) else {
            continue; // handle not ready yet
        };
        // SAFETY: plugin editor systems are pinned to the main thread via
        // `PluginEditorMainThread` non-send marker; `get_handle` is safe to
        // call on the main thread.
        let thread_locked = unsafe { raw_handle.get_handle() };

        match emitter.handle.open_editor(&thread_locked) {
            Ok(size) => {
                let w = size.width;
                let h = size.height;
                // `open_editor` just returned `Ok`, so the editor capability is
                // present — but it is an `Option` on the handle, and defaulting
                // a missing one would report a non-resizable editor for a plugin
                // that is resizable. Skipping instead keeps the window
                // unconfigured rather than misconfigured.
                let Some(capabilities) = emitter.handle.editor().map(|e| e.editor_capabilities())
                else {
                    continue;
                };
                bevy_log::info!(
                    "Plugin '{}' editor opened ({w}x{h}, resizable={})",
                    emitter.handle.name(),
                    capabilities.resize.resizable,
                );

                if let Ok(mut win) = windows.get_mut(pend.window_entity) {
                    win.resolution.set(w as f32, h as f32);
                    if capabilities.resize.resizable {
                        win.resize_constraints = bevy_window::WindowResizeConstraints {
                            min_width: 64.0,
                            min_height: 64.0,
                            max_width: f32::INFINITY,
                            max_height: f32::INFINITY,
                        };
                    } else {
                        win.resize_constraints = bevy_window::WindowResizeConstraints {
                            min_width: w as f32,
                            min_height: h as f32,
                            max_width: w as f32,
                            max_height: h as f32,
                        };
                    }
                    win.visible = true;
                }

                // Attach as child of primary window so they move together.
                // Unwrap Bevy's RawHandleWrapper into a platform-neutral
                // RawWindowHandle at the boundary; native_window is Bevy-free.
                if let Ok(parent_handle) = primary.single() {
                    attach_child_window(
                        raw_handle.get_window_handle(),
                        parent_handle.get_window_handle(),
                    );
                }

                // macOS: drive smooth live resize. AppKit-friendly
                // formats (VST3/JUCE) get the autoresize mask; the
                // others (CLAP, AU) get an NSNotificationCenter
                // observer that calls `set_editor_size` from inside
                // AppKit's tracking loop.
                #[cfg(target_os = "macos")]
                if capabilities.resize.resizable {
                    if capabilities.appkit_autoresize_friendly {
                        crate::plugin_host::native_window::enable_subview_autoresize(
                            raw_handle.get_window_handle(),
                        );
                    } else {
                        let handle = emitter.handle.clone();
                        let cb: crate::plugin_host::live_resize::ResizeCallback =
                            std::sync::Arc::new(move |w, h| {
                                let _ = handle.set_editor_size(tutti_plugin::handles::EditorSize {
                                    width: w,
                                    height: h,
                                });
                            });
                        // SAFETY: main-thread context — this system takes
                        // `NonSend` params, so Bevy runs it on the main thread.
                        let installed = unsafe {
                            crate::plugin_host::live_resize::LiveResizeHandle::install(
                                raw_handle.get_window_handle(),
                                cb,
                            )
                        };
                        // The observer is owned by the main-thread-only
                        // registry, never by a (Send + Sync) component.
                        if let Some(installed) = installed {
                            live_resize_registry.insert(entity, installed);
                        }
                    }
                }

                // Remove RawHandleWrapper so Bevy's renderer doesn't create a
                // wgpu surface on this window (the plugin owns the rendering).
                commands
                    .entity(pend.window_entity)
                    .remove::<bevy_window::RawHandleWrapper>();

                commands
                    .entity(entity)
                    .remove::<PendingPluginEditor>()
                    .insert(PluginEditorOpen {
                        editor_window: pend.window_entity,
                        width: w,
                        height: h,
                        capabilities,
                        last_applied: (w, h),
                    });
            }
            Err(e) => {
                warn!(
                    "Plugin '{}' editor failed to open: {}",
                    emitter.handle.name(),
                    e,
                );
                commands.entity(pend.window_entity).despawn();
                commands.entity(entity).remove::<PendingPluginEditor>();
            }
        }
    }
}

/// Forwards OS-driven editor-window resizes to the plugin, and writes the
/// plugin's snapped reply back to the window.
///
/// A `WindowResized` matching `last_applied` is an echo of this host's own
/// write and is skipped, which is what stops the two sides trading resizes. A
/// plugin that refuses outright has the window put back to the last size it
/// accepted.
pub fn plugin_editor_window_resize_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    mut events: bevy_ecs::message::MessageReader<bevy_window::WindowResized>,
    mut editors: Query<(&PluginEmitter, &mut PluginEditorOpen)>,
    mut windows: Query<&mut bevy_window::Window>,
) {
    for ev in events.read() {
        let event_size = (ev.width.round() as u32, ev.height.round() as u32);
        for (emitter, mut editor) in editors.iter_mut() {
            if editor.editor_window != ev.window {
                continue;
            }
            if !editor.capabilities.resize.resizable {
                continue;
            }
            if event_size == editor.last_applied {
                continue;
            }

            let requested = tutti_plugin::handles::EditorSize {
                width: event_size.0,
                height: event_size.1,
            };
            match emitter.handle.set_editor_size(requested) {
                Ok(snapped) => {
                    editor.last_applied = (snapped.width, snapped.height);
                    editor.width = snapped.width;
                    editor.height = snapped.height;
                    if (snapped.width, snapped.height) != event_size {
                        if let Ok(mut win) = windows.get_mut(ev.window) {
                            win.resolution
                                .set(snapped.width as f32, snapped.height as f32);
                        }
                    }
                }
                Err(e) => {
                    bevy_log::warn!(
                        "Plugin '{}' refused resize to {}x{}: {}",
                        emitter.handle.name(),
                        event_size.0,
                        event_size.1,
                        e
                    );
                    if let Ok(mut win) = windows.get_mut(ev.window) {
                        win.resolution
                            .set(editor.last_applied.0 as f32, editor.last_applied.1 as f32);
                    }
                }
            }
        }
    }
}

/// Polls each open editor for plugin-initiated resize requests, resizes
/// the host window, then calls back into the plugin via
/// `set_editor_size` so it lays out at the new bounds (per Steinberg's
/// `IPlugFrame::resizeView` contract).
pub fn plugin_editor_resize_request_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    mut editors: Query<(&PluginEmitter, &mut PluginEditorOpen)>,
    mut windows: Query<&mut bevy_window::Window>,
) {
    for (emitter, mut editor) in editors.iter_mut() {
        // No editor capability means no editor to resize — the same "nothing to
        // do" as a present editor with no pending request, so both collapse
        // into one `None` inside the handle method rather than at each call
        // site.
        let Some(req) = emitter.handle.poll_editor_resize_request() else {
            continue;
        };
        if (req.width, req.height) == editor.last_applied {
            continue;
        }

        editor.last_applied = (req.width, req.height);
        editor.width = req.width;
        editor.height = req.height;

        if let Ok(mut win) = windows.get_mut(editor.editor_window) {
            win.resolution.set(req.width as f32, req.height as f32);
            if !editor.capabilities.resize.resizable {
                win.resize_constraints = bevy_window::WindowResizeConstraints {
                    min_width: req.width as f32,
                    min_height: req.height as f32,
                    max_width: req.width as f32,
                    max_height: req.height as f32,
                };
            }
        }

        // Drive onSize so the plugin lays out at the new bounds. A further
        // snap gets one follow-up update; this deliberately does not loop.
        if let Ok(snapped) = emitter.handle.set_editor_size(req) {
            if (snapped.width, snapped.height) != (req.width, req.height) {
                editor.last_applied = (snapped.width, snapped.height);
                editor.width = snapped.width;
                editor.height = snapped.height;
                if let Ok(mut win) = windows.get_mut(editor.editor_window) {
                    win.resolution
                        .set(snapped.width as f32, snapped.height as f32);
                }
            }
        }
    }
}

/// Handles the OS close button on plugin editor windows.
///
/// Routes the close through [`SetEditorVisible::hide`] so the native
/// `close_editor()` runs before the window despawns, and removes the
/// `ClosingWindow` marker so Bevy's default `close_when_requested` does not
/// despawn the window out from under it.
///
/// The user closing the window and a host calling `hide` are the same
/// operation, so they share one path rather than each tearing the editor down
/// their own way.
pub fn plugin_editor_window_close_system(
    mut commands: Commands,
    mut close_events: bevy_ecs::message::MessageReader<bevy_window::WindowCloseRequested>,
    editors: Query<(Entity, &PluginEditorOpen)>,
) {
    for event in close_events.read() {
        for (entity, editor) in editors.iter() {
            if editor.editor_window == event.window {
                commands
                    .entity(event.window)
                    .remove::<bevy_window::ClosingWindow>();
                commands.trigger(SetEditorVisible::hide(entity));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! A `PluginEmitter` needs a `PluginHandle`, which needs a launched
    //! subprocess, so nothing here can put a real plugin in the world. What is
    //! covered is the part that does not need one: `editor_is_open`, the
    //! predicate every "is this plugin showing a UI" decision goes through.
    //!
    //! **Not covered here:** that a floating-capable plugin actually takes the
    //! floating branch of `set_editor_visible_observer`. That needs a loaded
    //! binary reporting `EDITOR_FLOATING`, and no floating-only CLAP plugin is
    //! installed to be one. The branch is driven by a single
    //! `Features::EDITOR_FLOATING` check, whose two arms are covered one crate
    //! down by `editor_bits` in `tutti-plugin-server`.
    //!
    //! `editor_is_open` takes `Query`s, so it is exercised through a system
    //! rather than called directly — a test that re-implemented the `||` would
    //! pass whatever the function did.

    use super::*;
    use bevy_app::prelude::*;

    /// What `record_open` saw: one `(entity, is_open)` pair per entity.
    #[derive(Resource, Default)]
    struct Observed(Vec<(Entity, bool)>);

    fn record_open(
        mut out: ResMut<Observed>,
        all: Query<Entity>,
        embedded: Query<&PluginEditorOpen>,
        floating: Query<&PluginFloatingEditorOpen>,
    ) {
        out.0 = all
            .iter()
            .map(|e| (e, editor_is_open(e, &embedded, &floating)))
            .collect();
    }

    fn run(spawn: impl FnOnce(&mut World) -> Entity) -> bool {
        let mut app = App::new();
        app.init_resource::<Observed>();
        app.add_systems(Update, record_open);
        let entity = spawn(app.world_mut());
        app.update();
        app.world()
            .resource::<Observed>()
            .0
            .iter()
            .find(|(e, _)| *e == entity)
            .map(|(_, open)| *open)
            .expect("the spawned entity must have been visited")
    }

    /// An embedded editor counts as open.
    #[test]
    fn an_embedded_editor_is_open() {
        assert!(run(|w| {
            let window = w.spawn_empty().id();
            w.spawn(PluginEditorOpen {
                editor_window: window,
                width: 100,
                height: 100,
                capabilities: Default::default(),
                last_applied: (100, 100),
            })
            .id()
        }));
    }

    /// A floating editor counts as open too — the case a check written against
    /// `PluginEditorOpen` alone gets wrong.
    ///
    /// This is what the idle pump keys on. Miss it and a floating editor is
    /// never ticked, which presents as a frozen plugin UI rather than an absent
    /// one — the harder bug to attribute.
    #[test]
    fn a_floating_editor_is_open() {
        assert!(
            run(|w| w.spawn(PluginFloatingEditorOpen).id()),
            "a floating editor is open, even though this host owns no window \
             for it"
        );
    }

    /// A plugin with neither component has no editor open.
    ///
    /// The negative half: without it, a predicate hardcoded to `true` would
    /// satisfy both tests above.
    #[test]
    fn a_plugin_with_no_editor_component_is_not_open() {
        assert!(
            !run(|w| w.spawn_empty().id()),
            "a bare entity has no editor in either hosting mode"
        );
    }

    /// The idle pump's query filter matches both hosting modes.
    ///
    /// Separate from [`editor_is_open`] and not redundant with it: the pump does
    /// not call that function, it uses [`EditorOpenFilter`], so a filter that
    /// dropped the floating arm would leave these two in disagreement. That is
    /// the frozen-UI bug, and it is invisible from the predicate's tests.
    ///
    /// Counted rather than asserted per-entity, so a filter that matched
    /// *everything* fails too.
    #[test]
    fn the_idle_filter_matches_both_hosting_modes() {
        let mut app = App::new();
        let world = app.world_mut();
        let window = world.spawn_empty().id();
        world.spawn(PluginEditorOpen {
            editor_window: window,
            width: 10,
            height: 10,
            capabilities: Default::default(),
            last_applied: (10, 10),
        });
        world.spawn(PluginFloatingEditorOpen);
        // A plugin with no editor open, plus the bare window entity above:
        // neither may match.
        world.spawn_empty();

        let mut q = world.query_filtered::<Entity, EditorOpenFilter>();
        assert_eq!(
            q.iter(world).count(),
            2,
            "the pump must tick exactly the two open editors — one embedded, \
             one floating — and nothing else"
        );
    }
}
