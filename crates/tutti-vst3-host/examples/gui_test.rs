//! GUI test example for VST3 plugin hosting.
//!
//! This example opens a window and embeds a VST3 plugin's GUI into it.
//!
//! Run with:
//! ```bash
//! cargo run -p vst3-host --example gui_test
//! ```

use std::path::Path;
use tutti_fixtures::main_thread;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use tutti_plugin_types::EditorWindow;
use tutti_vst3_host::{ContentScale, LoadOptions, Vst3Loaded, WindowHandle};

const PLUGIN_PATH: &str =
    "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3/Contents/MacOS/TAL-NoiseMaker";

struct App {
    window: Option<Window>,
    plugin: Option<Vst3Loaded>,
    editor_open: bool,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            plugin: None,
            editor_open: false,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // Load plugin first to get editor size
        println!("Loading plugin: {}", PLUGIN_PATH);
        let plugin =
            match Vst3Loaded::load(&main_thread(), Path::new(PLUGIN_PATH), LoadOptions::new()) {
                Ok(p) => {
                    println!(
                        "Loaded: {} by {}",
                        p.plugin_info().name,
                        p.plugin_info().vendor
                    );
                    p
                }
                Err(e) => {
                    eprintln!("Failed to load plugin: {:?}", e);
                    event_loop.exit();
                    return;
                }
            };

        if !plugin.has_editor(&main_thread()) {
            eprintln!("Plugin has no editor");
            event_loop.exit();
            return;
        }

        // Create window
        let window_attrs = Window::default_attributes()
            .with_title(format!("{} - VST3 GUI Test", plugin.plugin_info().name))
            .with_inner_size(winit::dpi::LogicalSize::new(800, 600));

        let window = match event_loop.create_window(window_attrs) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("Failed to create window: {:?}", e);
                event_loop.exit();
                return;
            }
        };

        self.window = Some(window);
        self.plugin = Some(plugin);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                println!("Closing...");
                // Close editor before exiting
                if let Some(plugin) = &mut self.plugin {
                    plugin.close_editor(&main_thread());
                }
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                // Open editor on first redraw if not already open
                #[allow(
                    clippy::collapsible_match,
                    reason = "hoisting `!self.editor_open` into a match guard would \
                              send already-open redraws to the catch-all arm instead \
                              of this deliberate no-op"
                )]
                if !self.editor_open
                    && let (Some(window), Some(plugin)) = (&self.window, &mut self.plugin)
                {
                    let handle = match WindowHandle::from_window(window) {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("No embeddable window: {e}");
                            return;
                        }
                    };
                    println!("Opening plugin editor...");
                    let scale = ContentScale::new(window.scale_factor()).unwrap_or_default();
                    match plugin.open_editor(&main_thread(), handle, scale) {
                        Ok(size) => {
                            println!("Editor opened: {}x{}", size.width, size.height);
                            // Resize window to fit editor
                            let _ = window.request_inner_size(winit::dpi::LogicalSize::new(
                                size.width,
                                size.height,
                            ));
                            self.editor_open = true;
                        }
                        Err(e) => {
                            eprintln!("Failed to open editor: {:?}", e);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn main() {
    println!("VST3 GUI Test");
    println!("=============");

    if !Path::new(PLUGIN_PATH).exists() {
        eprintln!("Plugin not found: {}", PLUGIN_PATH);
        eprintln!("Please install TAL-NoiseMaker or modify PLUGIN_PATH");
        return;
    }

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new();
    event_loop.run_app(&mut app).expect("Event loop error");
}
