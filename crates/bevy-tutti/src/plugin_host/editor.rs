//! Plugin GUI editor lifecycle: components + open/attach/idle/resize/close systems.

use bevy_ecs::prelude::*;
use bevy_log::warn;
use bevy_reflect::prelude::*;

use crate::plugin_host::native_window::attach_child_window;
use crate::plugin_host::PluginEditorMainThread;

/// Marks an entity as a loaded plugin with a control handle.
///
/// Inserted by [`plugin_load_promote`](crate::plugin_host::load::plugin_load_promote)
/// once the off-thread load resolves. Use the `handle` to control parameters,
/// open/close the editor, save/load state, etc.
///
/// The audio node is tracked separately via `AudioNode`.
///
/// Not `Debug` / `Reflect`: `PluginHandle` wraps a foreign plugin-control
/// handle that doesn't implement `Debug` and isn't reflected.
#[derive(Component, Clone)]
pub struct PluginEmitter {
    pub handle: tutti_plugin::handles::PluginHandle,
}

/// Present while a plugin's GUI editor is open in a separate Bevy window.
///
/// `plugin_editor_idle_system` calls `handle.editor_idle()` every frame
/// for entities that have this component.
///
/// Not `Reflect`: `EditorCapabilities` and the macOS live-resize observer
/// are foreign types.
#[derive(Component)]
pub struct PluginEditorOpen {
    /// The Bevy Window entity hosting the plugin editor.
    pub editor_window: Entity,
    /// Editor width in logical pixels as reported by the plugin.
    pub width: u32,
    /// Editor height in logical pixels as reported by the plugin.
    pub height: u32,
    pub capabilities: tutti_plugin::handles::EditorCapabilities,
    /// Last size written to either side. A `WindowResized` matching
    /// this is an echo of our own write and is ignored.
    pub last_applied: (u32, u32),
    // NOTE (macOS): the AppKit live-resize observer used to be a field here,
    // which forced an `unsafe impl Send + Sync` over a `Retained<NSView>`
    // solely to satisfy `Component: Send + Sync`. That placed an AppKit
    // `removeObserver` inside a `Drop` that runs wherever a `Commands` queue
    // is applied (`plugin_health_poll` is not main-thread pinned) or
    // wherever the `World` is torn down — off-main AppKit is a hard crash on
    // macOS. The observer now lives in the `NonSend` `LiveResizeRegistry`,
    // keyed by this plugin entity, so Bevy pins every access and every drop
    // to the main thread.
}

/// Intermediate state: a Window has been spawned but `open_editor` hasn't
/// been called yet (waiting for the native handle to become available).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component)]
pub struct PendingPluginEditor {
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
/// # Why one event rather than an open-component and a close-event
///
/// This replaced a trigger *component* (`OpenPluginEditor`, inserted and then
/// removed by the system that saw it) beside an *event* (`CloseEditor`) — two
/// shapes for one concern, and neither could express "toggle" without the caller
/// first asking whether the editor was open. That question has no good answer
/// from outside: a `Query<&PluginEditorOpen>` reads the previous frame, so a
/// fast double-click could open twice or close a window already gone. Resolving
/// [`Visibility::Toggle`] inside the observer, where `PluginEditorOpen` is
/// authoritative, makes that unrepresentable.
///
/// A component also implied a state it did not have: `OpenPluginEditor` was
/// present for exactly one frame, so "is this plugin's editor open" was never
/// answerable from it — that is `PluginEditorOpen`, which this event does not
/// duplicate.
#[derive(EntityEvent, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetEditorVisible {
    pub entity: Entity,
    pub visibility: Visibility,
}

/// What [`SetEditorVisible`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    Show,
    Hide,
    /// Whichever the editor is not right now.
    Toggle,
}

impl SetEditorVisible {
    pub fn show(entity: Entity) -> Self {
        Self {
            entity,
            visibility: Visibility::Show,
        }
    }

    pub fn hide(entity: Entity) -> Self {
        Self {
            entity,
            visibility: Visibility::Hide,
        }
    }

    pub fn toggle(entity: Entity) -> Self {
        Self {
            entity,
            visibility: Visibility::Toggle,
        }
    }
}

/// Ticks `editor_idle()` on all plugins that have `PluginEditorOpen`.
///
/// Call this in Bevy's `Update` schedule. Plugin GUIs require periodic
/// idle ticks to handle redraws and event processing.
pub fn plugin_editor_idle_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    query: Query<(&PluginEmitter, &PluginEditorOpen)>,
) {
    for (emitter, _) in query.iter() {
        emitter.handle.editor_idle();
    }
}

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
) {
    use bevy_window::{Window, WindowResolution};

    let entity = request.event_target();
    // Not a plugin, or one whose load failed. A host aiming a toggle at the
    // wrong entity is a mistake a log line per click would not help with.
    let Ok(emitter) = plugins.get(entity) else {
        return;
    };

    let showing = open.get(entity).is_ok() || pending.get(entity).is_ok();
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

/// Phase 2: once the native handle is available, call `open_editor` on the plugin.
pub fn plugin_editor_attach_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    // `NonSendMut` pins this system (and therefore every observer install and
    // every observer drop) to the main thread — the AppKit requirement that
    // the old `unsafe impl Send + Sync` was papering over.
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
                let capabilities = emitter.handle.editor_capabilities();
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
                        // registry, never by the (Send + Sync) component.
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

/// Forwards OS-driven editor-window resizes to the plugin and writes
/// the plugin's snapped reply back to the window.
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

        // Drive onSize so the plugin lays out at the new bounds. If
        // the plugin snaps further, last_applied gets a follow-up
        // update — but we don't loop here.
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
/// `ClosingWindow` marker so Bevy's default `close_when_requested` doesn't
/// despawn the window out from under us.
///
/// The user closing the window and a host calling `hide` are the same operation,
/// so they share the one path rather than each tearing the editor down their own
/// way.
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
