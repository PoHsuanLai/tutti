//! The event loop over a scripted host and the reference CLAP plugin: what it
//! serves while the host is quiet, and the editor on the served instance.
//!
//! The host is a script of messages and silences, so the loop runs on the test's
//! own thread (it holds the `MainThread` token) against real time: a silence is
//! slept out in `wait_readable`, as the socket's would be.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tutti_clap_test_plugin::{
    GUI_CMD_REQUEST_RESIZE, GUI_HEIGHT, GUI_REQUESTED_RESIZE_H, GUI_REQUESTED_RESIZE_W, GUI_WIDTH,
    GuiMode, Site,
};
use tutti_fixtures::main_thread;
use tutti_plugin_ipc::{AudioSlab, BridgeMessage, HostMessage, ProcessAudioData};
use tutti_plugin_types::{BridgeError, BridgeResult, ContentScale, EditorSize, Load, SampleFormat};

use super::serve_every;
use crate::session::Session;
use crate::transport::Transport;

/// One thing the scripted host does.
#[allow(clippy::large_enum_variant)] // a test script, built once
enum Step {
    Send(HostMessage),
    /// Says nothing for this long, from the first wait on it.
    Quiet(Duration),
    /// A silence that has begun: it ends at this instant.
    QuietUntil(Instant),
}

/// A host that follows a script and records what it is sent.
struct Script {
    steps: VecDeque<Step>,
    heard: Vec<BridgeMessage>,
}

impl Script {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            heard: Vec::new(),
        }
    }
}

impl Transport for Script {
    fn wait_readable(&mut self, timeout: Duration) -> BridgeResult<bool> {
        if let Some(Step::Quiet(length)) = self.steps.front() {
            let until = Instant::now() + *length;
            self.steps[0] = Step::QuietUntil(until);
        }
        match self.steps.front() {
            Some(&Step::QuietUntil(until)) => {
                let left = until.saturating_duration_since(Instant::now());
                std::thread::sleep(timeout.min(left));
                if Instant::now() >= until {
                    self.steps.pop_front();
                }
                Ok(false)
            }
            // A message is there to read, or the script is over and `recv` says so.
            _ => Ok(true),
        }
    }

    fn recv(&mut self) -> BridgeResult<HostMessage> {
        match self.steps.pop_front() {
            Some(Step::Send(message)) => Ok(message),
            _ => Err(BridgeError::ConnectionFailed("the script is over".into())),
        }
    }

    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
        self.heard.push(msg.clone());
        Ok(())
    }
}

const TICK: Duration = Duration::from_millis(10);

fn load_message() -> HostMessage {
    HostMessage::LoadPlugin {
        path: std::path::PathBuf::from(crate::test_utils::clap_probe_path_dot_clap()),
        load: Load::new(),
        sample_rate: tutti_plugin_types::SampleRate(44100.0),
        block_size: 512,
        preferred_format: SampleFormat::Float32,
        shm_name: String::new(),
        state: None,
    }
}

/// A session with a slab, as the handshake leaves it.
fn session(name: &str) -> (Session, tutti_plugin_ipc::SlabWriter) {
    let mut session = Session::new();
    let layout = tutti_plugin_ipc::SlabLayout {
        samples_per_channel: 512,
        format: SampleFormat::Float32,
        slots: tutti_plugin_ipc::RING_SLOTS as u32,
        inputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
        outputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
    };
    let shm = format!("tutti_evloop_{name}_{}", std::process::id());
    let creator = AudioSlab::create(shm.clone(), layout.clone()).expect("slab");
    session.shm = Some(AudioSlab::open(shm, layout).expect("slab"));
    (session, creator)
}

/// Selects the gui shape for the load that follows; the default again on drop.
struct GuiShape;

impl GuiShape {
    fn embeddable() -> Self {
        let probe = crate::test_utils::clap();
        probe.set_gui_mode(GuiMode::Embeddable);
        probe.gui_reset();
        Self
    }
}

impl Drop for GuiShape {
    fn drop(&mut self) {
        let probe = crate::test_utils::clap();
        probe.gui_command(tutti_clap_test_plugin::GUI_CMD_NONE);
        probe.set_gui_mode(GuiMode::Absent);
    }
}

fn run(session: &mut Session, script: &mut Script) {
    let result = serve_every(session, script, &main_thread(), TICK);
    // A script that ends ends in `recv`'s refusal; any other end is a failure.
    assert!(
        matches!(result, Err(BridgeError::ConnectionFailed(ref m)) if m == "the script is over")
            || result.is_ok(),
        "{result:?}"
    );
}

/// **A quiet host does not stop the plugin's main-thread work.** The loop used
/// to block in `recv`, so a timer the plugin registered was served only at the
/// next message: after one block, a quiet host saw it tick at most once. The
/// plugin registers a period-0 timer (due at every service pass) in the
/// callback its first block asks for, and the loop's idle ticks must keep
/// serving it while the host says nothing at all.
#[test]
fn a_quiet_host_does_not_stop_the_plugins_timers() {
    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_TIMER);
    let (mut session, _slab) = session("timers");
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Send(HostMessage::ProcessAudio(ProcessAudioData {
            seq: 1,
            num_samples: 64,
            ..Default::default()
        })),
        Step::Quiet(Duration::from_millis(300)),
    ]);

    run(&mut session, &mut script);

    let ticks = probe.thread_capture().sites[Site::OnTimer as usize].visits;
    assert!(
        ticks >= 5,
        "300 ms of a quiet host at a 10 ms tick served the timer {ticks} times"
    );
}

/// **A request is answered while the loop is ticking**: the idle wait ends the
/// instant the host speaks, so a message after silence is served and replied
/// to, not left for the next tick.
#[test]
fn a_message_after_silence_is_answered() {
    let _lock = crate::test_utils::plugin_load_lock();
    let (mut session, _slab) = session("answers");
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Quiet(Duration::from_millis(60)),
        Step::Send(HostMessage::GetParameterList),
    ]);

    run(&mut session, &mut script);

    assert!(
        matches!(
            script.heard.last(),
            Some(BridgeMessage::ParameterList { parameters }) if !parameters.is_empty()
        ),
        "{:?}",
        script.heard
    );
}

/// The editor opens on the served instance, a resize the plugin asks for
/// reaches the host with no message to carry it, the host's size is taken, and
/// a close destroys the plugin's editor. Needs a window number that means the
/// same here as in the host (an X11 window id).
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn the_editor_opens_resizes_and_closes_on_the_served_instance() {
    let _lock = crate::test_utils::plugin_load_lock();
    let _shape = GuiShape::embeddable();
    let probe = crate::test_utils::clap();
    probe.gui_command(GUI_CMD_REQUEST_RESIZE);
    let (mut session, _slab) = session("editor");
    let wanted = EditorSize {
        width: 801,
        height: 601,
    };
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Send(HostMessage::OpenEditor {
            parent: Some(0x40_0001),
            scale: ContentScale::IDENTITY,
        }),
        // The plugin asked for a size from `show`; only an idle tick can say so.
        Step::Quiet(Duration::from_millis(60)),
        Step::Send(HostMessage::SetEditorSize { size: wanted }),
        Step::Send(HostMessage::CloseEditor),
        Step::Quiet(Duration::from_millis(30)),
    ]);

    run(&mut session, &mut script);

    let opened = script
        .heard
        .iter()
        .find_map(|m| match m {
            BridgeMessage::EditorOpened { result } => Some(result.clone()),
            _ => None,
        })
        .expect("the open is answered");
    assert_eq!(
        opened,
        Ok(Some(EditorSize {
            width: GUI_WIDTH,
            height: GUI_HEIGHT
        })),
        "the plugin's own size"
    );
    assert!(
        script.heard.iter().any(|m| matches!(
            m,
            BridgeMessage::EditorWantsSize { size }
                if *size == EditorSize { width: GUI_REQUESTED_RESIZE_W, height: GUI_REQUESTED_RESIZE_H }
        )),
        "the plugin's resize request reached the host: {:?}",
        script.heard
    );
    let sized = script
        .heard
        .iter()
        .find_map(|m| match m {
            BridgeMessage::EditorSized { result } => Some(result.clone()),
            _ => None,
        })
        .expect("the resize is answered");
    let took = sized.expect("the plugin takes a size");
    let cap = probe.gui_capture();
    assert_eq!(
        (cap.last_set_size_w, cap.last_set_size_h),
        (took.width, took.height),
        "the size the server answered is the one the plugin was set to"
    );
    assert!(
        cap.set_parent_window_non_null,
        "embedded in the host's window"
    );
    assert!(cap.destroyed, "a close reaches the plugin");
    assert_eq!(cap.create_balance, 0, "every create is destroyed");
}

/// **A plugin that goes while its editor is open takes the editor with it, and
/// the host is told**: the server closed it on its own account.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn unloading_a_plugin_closes_its_editor_and_says_so() {
    let _lock = crate::test_utils::plugin_load_lock();
    let _shape = GuiShape::embeddable();
    let probe = crate::test_utils::clap();
    let (mut session, _slab) = session("unload");
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Send(HostMessage::OpenEditor {
            parent: Some(0x40_0001),
            scale: ContentScale::IDENTITY,
        }),
        Step::Send(HostMessage::UnloadPlugin),
        Step::Quiet(Duration::from_millis(40)),
    ]);

    run(&mut session, &mut script);

    assert!(
        script
            .heard
            .iter()
            .any(|m| matches!(m, BridgeMessage::EditorClosed)),
        "{:?}",
        script.heard
    );
    assert_eq!(
        probe.gui_capture().create_balance,
        0,
        "the editor was destroyed"
    );
}

/// **With nothing loaded, an editor request is answered, never dropped**: the
/// host blocks on the open and the resize.
#[test]
fn an_editor_request_with_no_plugin_is_refused_not_ignored() {
    let (mut session, _slab) = session("none");
    let mut script = Script::new([
        Step::Send(HostMessage::OpenEditor {
            parent: None,
            scale: ContentScale::IDENTITY,
        }),
        Step::Send(HostMessage::SetEditorSize {
            size: EditorSize {
                width: 10,
                height: 10,
            },
        }),
        Step::Send(HostMessage::CloseEditor),
    ]);

    run(&mut session, &mut script);

    assert!(
        matches!(
            script.heard.as_slice(),
            [
                BridgeMessage::EditorOpened { result: Err(open) },
                BridgeMessage::EditorSized { result: Err(size) },
            ] if open.contains("no plugin") && size.contains("not open")
        ),
        "{:?}",
        script.heard
    );
}
