//! Plugin GUI editor lifecycle: components + open/attach/idle/resize/close systems.

use bevy_ecs::prelude::*;
use bevy_log::warn;
use bevy_reflect::prelude::*;

use crate::native_window::attach_child_window;
use crate::resources::PluginEditorMainThread;

/// Marks an entity as a loaded plugin with a control handle.
///
/// Added automatically by `plugin_load_system`. Use the `handle` to
/// control parameters, open/close the editor, save/load state, etc.
///
/// The audio node is tracked separately via `AudioEmitter`.
///
/// Not `Debug` / `Reflect`: `PluginHandle` wraps a foreign plugin-control
/// handle that doesn't implement `Debug` and isn't reflected.
#[derive(Component, Clone)]
pub struct PluginEmitter {
    pub handle: tutti::plugin::handles::PluginHandle,
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
    pub capabilities: tutti::plugin::handles::EditorCapabilities,
    /// Last size written to either side. A `WindowResized` matching
    /// this is an echo of our own write and is ignored.
    pub last_applied: (u32, u32),
    /// macOS only: AppKit notification observer that drives
    /// `set_editor_size` during live drag for plugins that don't
    /// follow the autoresize mask. Dropped with the editor.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    pub(crate) live_resize: Option<crate::live_resize::LiveResizeHandle>,
}

/// Intermediate state: a Window has been spawned but `open_editor` hasn't
/// been called yet (waiting for the native handle to become available).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component)]
pub struct PendingPluginEditor {
    pub window_entity: Entity,
}

/// Trigger component: insert on an entity with `PluginEmitter` to open
/// the plugin's native GUI editor. Automatically removed after processing.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct OpenPluginEditor;

/// Trigger component: insert on an entity with `PluginEmitter` +
/// `PluginEditorOpen` to close the plugin's native GUI editor.
/// Automatically removed after processing.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct ClosePluginEditor;

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

/// Phase 1 of plugin editor opening: spawn a Bevy Window for the editor.
///
/// The native handle won't be available until the next frame, so we insert
/// `PendingPluginEditor` and let `plugin_editor_attach_system` finish the job.
pub fn plugin_editor_open_system(
    mut commands: Commands,
    query: Query<(Entity, &PluginEmitter), Added<OpenPluginEditor>>,
) {
    use bevy_window::{Window, WindowResolution};

    for (entity, emitter) in query.iter() {
        commands.entity(entity).remove::<OpenPluginEditor>();

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
    }
}

/// Phase 2: once the native handle is available, call `open_editor` on the plugin.
pub fn plugin_editor_attach_system(
    _main_thread: NonSend<PluginEditorMainThread>,
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
                    capabilities.resizable,
                );

                if let Ok(mut win) = windows.get_mut(pend.window_entity) {
                    win.resolution.set(w as f32, h as f32);
                    if capabilities.resizable {
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
                if let Ok(parent_handle) = primary.single() {
                    attach_child_window(raw_handle, parent_handle);
                }

                // macOS: drive smooth live resize. AppKit-friendly
                // formats (VST3/JUCE) get the autoresize mask; the
                // others (CLAP, AU) get an NSNotificationCenter
                // observer that calls `set_editor_size` from inside
                // AppKit's tracking loop.
                #[cfg(target_os = "macos")]
                let live_resize = if capabilities.resizable {
                    if capabilities.appkit_autoresize_friendly {
                        crate::native_window::enable_subview_autoresize(raw_handle);
                        None
                    } else {
                        let handle = emitter.handle.clone();
                        let cb: crate::live_resize::ResizeCallback =
                            std::sync::Arc::new(move |w, h| {
                                let _ = handle.set_editor_size(
                                    tutti::plugin::handles::EditorSize {
                                        width: w,
                                        height: h,
                                    },
                                );
                            });
                        // SAFETY: main-thread context.
                        unsafe {
                            crate::live_resize::LiveResizeHandle::install(raw_handle, cb)
                        }
                    }
                } else {
                    None
                };

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
                        #[cfg(target_os = "macos")]
                        live_resize,
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
            if !editor.capabilities.resizable {
                continue;
            }
            if event_size == editor.last_applied {
                continue;
            }

            let requested = tutti::plugin::handles::EditorSize {
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
                        win.resolution.set(
                            editor.last_applied.0 as f32,
                            editor.last_applied.1 as f32,
                        );
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
            if !editor.capabilities.resizable {
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

/// Closes plugin editors for entities with `ClosePluginEditor` trigger.
pub fn plugin_editor_close_system(
    _main_thread: NonSend<PluginEditorMainThread>,
    mut commands: Commands,
    query: Query<(Entity, &PluginEmitter, &PluginEditorOpen), Added<ClosePluginEditor>>,
) {
    for (entity, emitter, editor) in query.iter() {
        emitter.handle.close_editor();
        commands.entity(editor.editor_window).try_despawn();
        bevy_log::info!(
            "Plugin '{}' editor closed (entity {entity:?})",
            emitter.handle.name()
        );
        commands
            .entity(entity)
            .remove::<ClosePluginEditor>()
            .remove::<PluginEditorOpen>();
    }
}

/// Handles the OS close button on plugin editor windows.
///
/// When a plugin editor window receives a `WindowCloseRequested`, this routes
/// the close through `ClosePluginEditor` on the plugin entity (so the native
/// `close_editor()` call runs before the window despawns) and removes the
/// `ClosingWindow` marker so Bevy's default `close_when_requested` doesn't
/// despawn the window out from under us.
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
                commands.entity(entity).insert(ClosePluginEditor);
            }
        }
    }
}
