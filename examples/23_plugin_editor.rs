//! # 23 - Plugin Editor GUI
//!
//! Open a plugin's native editor GUI via the engine's plugin API.
//!
//! Tries enabled plugin formats in order (CLAP, VST3, VST2). Uses feature flags
//! to select which formats are available.
//!
//! Logs parameter changes to stdout when you interact with the plugin GUI.
//!
//! ```bash
//! # CLAP
//! cargo run --example 23_plugin_editor --features "clap,midi"
//!
//! # VST3
//! cargo run --example 23_plugin_editor --features "vst3,midi"
//!
//! # VST2 (audio + native editor; runs in-process via `vst2-host`)
//! cargo run --example 23_plugin_editor --features "vst2,midi"
//!
//! # AU (macOS only)
//! cargo run --example 23_plugin_editor --features "au,midi"
//!
//! # All formats (tries CLAP first, then VST3, then VST2, then AU)
//! cargo run --example 23_plugin_editor --features "clap,vst3,vst2,au,midi"
//! ```
//!
//! ## Setup
//!
//! Install a free plugin with a GUI:
//! - [TAL-NoiseMaker](https://tal-software.com/products/tal-noisemaker) (CLAP + VST3 + VST2)
//! - [Surge XT](https://surge-synthesizer.github.io/) (CLAP)

fn main() -> tutti::Result<()> {
    plugin_editor::run()
}

mod plugin_editor {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::window::{Window, WindowId};

    use tutti::plugin::handles::{EditorSize, PluginHandle};
    use tutti::prelude::*;
    use tutti_midi_runtime::MidiSender;

    /// Try loading a plugin from known paths using the engine API.
    /// Searches CLAP, VST3, and VST2 formats in order (based on enabled features).
    fn try_load_plugin(sample_rate: f64) -> Option<(Box<dyn tutti::AudioUnit>, PluginHandle)> {
        // CLAP plugins
        #[cfg(feature = "clap")]
        {
            let clap_paths = [
                "/Library/Audio/Plug-Ins/CLAP/Surge XT.clap",
                "/Library/Audio/Plug-Ins/CLAP/TAL-NoiseMaker.clap",
            ];
            for path in &clap_paths {
                if Path::new(path).exists() {
                    println!("  Trying CLAP: {}", path);
                    match tutti::plugin::clap(sample_rate, path).build() {
                        Ok((unit, handle)) => return Some((unit, handle)),
                        Err(e) => eprintln!("    Failed: {:?}", e),
                    }
                }
            }
        }

        // VST3 plugins
        #[cfg(feature = "vst3")]
        {
            let vst3_paths = [
                // Local fixture plugins (Voxengo — free, support f64)
                "tests/fixtures/plugins/Boogex.vst3",
                "tests/fixtures/plugins/SPAN.vst3",
                // System-installed plugins
                "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3",
                "/Library/Audio/Plug-Ins/VST3/Surge XT.vst3",
            ];
            for path in &vst3_paths {
                if Path::new(path).exists() {
                    println!("  Trying VST3: {}", path);
                    match tutti::plugin::vst3(sample_rate, path).build() {
                        Ok((unit, handle)) => return Some((unit, handle)),
                        Err(e) => eprintln!("    Failed: {:?}", e),
                    }
                }
            }
        }

        // VST2 plugins
        #[cfg(feature = "vst2")]
        {
            let vst2_paths = ["/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst"];
            for path in &vst2_paths {
                if Path::new(path).exists() {
                    println!("  Trying VST2: {}", path);
                    match tutti::plugin::vst2(sample_rate, path).build() {
                        Ok((unit, handle)) => return Some((unit, handle)),
                        Err(e) => eprintln!("    Failed: {:?}", e),
                    }
                }
            }
        }

        // AU plugins (macOS only)
        #[cfg(all(feature = "au", target_os = "macos"))]
        {
            let au_paths = ["/Library/Audio/Plug-Ins/Components/TAL-NoiseMaker.component"];
            for path in &au_paths {
                if Path::new(path).exists() {
                    println!("  Trying AU: {}", path);
                    match tutti::plugin::au(sample_rate, path).build() {
                        Ok((unit, handle)) => return Some((unit, handle)),
                        Err(e) => eprintln!("    Failed: {:?}", e),
                    }
                }
            }
        }

        let _ = sample_rate;
        None
    }

    struct App {
        engine: TuttiEngine,
        handle: Option<PluginHandle>,
        node_id: Option<tutti::NodeId>,
        window: Option<Window>,
        editor_open: bool,
        notes_sent: bool,
        is_effect: bool,
        param_snapshot: Vec<(u32, String, f32)>,
        last_poll: Instant,
        midi_sender: Option<MidiSender>,
    }

    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    impl App {
        fn new(engine: TuttiEngine) -> Self {
            Self {
                engine,
                handle: None,
                node_id: None,
                window: None,
                editor_open: false,
                notes_sent: false,
                is_effect: false,
                param_snapshot: Vec::new(),
                last_poll: Instant::now(),
                midi_sender: None,
            }
        }

        fn snapshot_params(handle: &PluginHandle) -> Vec<(u32, String, f32)> {
            let params = handle.parameters().unwrap_or_default();
            params
                .iter()
                .map(|info| {
                    let value = handle.parameter(info.id).unwrap_or(0.0);
                    (info.id, info.name.clone(), value)
                })
                .collect()
        }

        fn poll_params(&mut self) {
            let Some(handle) = &self.handle else { return };
            let new_snapshot = Self::snapshot_params(handle);

            for (id, name, new_value) in &new_snapshot {
                if let Some((_, _, old_value)) =
                    self.param_snapshot.iter().find(|(pid, _, _)| pid == id)
                {
                    if (old_value - new_value).abs() > 1e-6 {
                        println!(
                            "  [param] {} (id={}) : {:.4} -> {:.4}",
                            name, id, old_value, new_value
                        );
                    }
                }
            }

            self.param_snapshot = new_snapshot;
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }

            println!("Searching for plugins...");
            let (unit, handle) = match try_load_plugin(self.engine.sample_rate) {
                Some(result) => result,
                None => {
                    eprintln!(
                        "No plugin found. Install TAL-NoiseMaker or Surge XT (CLAP/VST3/VST2)."
                    );
                    event_loop.exit();
                    return;
                }
            };

            let meta = handle.metadata().clone();
            let plugin_name = meta.name.clone();
            println!(
                "Loaded: {} (has_editor: {}, supports_f64: {}, audio_io: {}in/{}out, midi: {})",
                plugin_name,
                meta.has_editor,
                meta.supports_f64,
                meta.audio_io.inputs,
                meta.audio_io.outputs,
                meta.receives_midi,
            );

            if !handle.has_editor() {
                eprintln!("Plugin '{}' has no editor GUI.", plugin_name);
                event_loop.exit();
                return;
            }

            let is_effect = meta.audio_io.inputs > 0;
            let node_id = if is_effect {
                println!("Plugin is an effect — feeding stereo saw wave as input.");
                let src = self.engine.graph.add(saw_hz(220.0) * 0.3f32);
                let fx = self.engine.graph.add_boxed(unit);
                self.engine.graph.pipe_all(src, fx);
                self.engine.graph.pipe_output(fx);
                self.engine.graph.commit();
                fx
            } else {
                println!("Plugin is a synth — will send MIDI notes.");
                let id = self.engine.graph.add_boxed(unit);
                self.engine.graph.pipe_output(id);
                self.engine.graph.commit();
                id
            };
            self.node_id = Some(node_id);
            self.is_effect = is_effect;
            self.midi_sender = Some(handle.midi_sender());
            self.engine.transport.play();

            let window_attrs = Window::default_attributes()
                .with_title(format!("{} - Plugin Editor", plugin_name))
                .with_inner_size(winit::dpi::LogicalSize::new(800u32, 600u32));

            let window = match event_loop.create_window(window_attrs) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("Failed to create window: {:?}", e);
                    event_loop.exit();
                    return;
                }
            };

            self.window = Some(window);
            self.handle = Some(handle);
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::CloseRequested => {
                    println!("Closing editor...");
                    if self.notes_sent {
                        if let Some(sender) = &self.midi_sender {
                            sender.note_off(0, 60);
                            sender.note_off(0, 64);
                            sender.note_off(0, 67);
                        }
                    }
                    if let Some(handle) = &self.handle {
                        handle.close_editor();
                    }
                    event_loop.exit();
                }
                WindowEvent::RedrawRequested => {
                    if !self.editor_open {
                        let (Some(window), Some(handle)) = (&self.window, &self.handle) else {
                            return;
                        };

                        println!("Opening plugin editor...");
                        match handle.open_editor(window) {
                            Ok(EditorSize { width, height }) => {
                                println!("Editor opened: {}x{}", width, height);
                                let _ = window.request_inner_size(winit::dpi::LogicalSize::new(
                                    width, height,
                                ));
                                self.editor_open = true;

                                self.param_snapshot = Self::snapshot_params(handle);
                                println!(
                                    "Tracking {} parameters. Tweak knobs to see changes.",
                                    self.param_snapshot.len()
                                );

                                if !self.is_effect {
                                    if let Some(sender) = &self.midi_sender {
                                        println!("Sending MIDI chord (C4 E4 G4)...");
                                        sender.note_on(0, 60, 100);
                                        sender.note_on(0, 64, 100);
                                        sender.note_on(0, 67, 100);
                                        self.notes_sent = true;
                                    }
                                }

                                event_loop.set_control_flow(ControlFlow::WaitUntil(
                                    Instant::now() + POLL_INTERVAL,
                                ));
                            }
                            Err(e) => {
                                eprintln!("Failed to open editor: {:?}", e);
                            }
                        }
                    }
                }
                _ => {}
            }

            if self.editor_open && self.last_poll.elapsed() >= POLL_INTERVAL {
                if let Some(handle) = &self.handle {
                    handle.editor_idle();
                }
                self.poll_params();
                self.last_poll = Instant::now();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.editor_open {
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
            }
        }
    }

    pub fn run() -> tutti::Result<()> {
        tracing_subscriber::fmt::init();

        println!("Plugin Editor Example");
        println!("=====================");
        println!();

        let engine = TuttiEngine::builder().midi().build()?;

        let event_loop = EventLoop::new().expect("Failed to create event loop");
        event_loop.set_control_flow(ControlFlow::Wait);

        let mut app = App::new(engine);
        event_loop.run_app(&mut app).expect("Event loop error");

        Ok(())
    }
} // mod plugin_editor
