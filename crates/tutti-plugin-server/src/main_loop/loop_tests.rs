//! The event loop over a scripted host and the reference CLAP plugin: what it
//! serves while the host is quiet, and the editor on the served instance.
//!
//! The host is a script of messages and silences on the control connection, so
//! the loop runs on the test's own thread (it holds the `MainThread` token)
//! against real time: a silence is slept out in `wait_readable`, as the
//! socket's would be. The audio connection is a real one in the same sense: the
//! render thread serves it, over an in-memory pair of channels that the host's
//! end ([`HostEnd`]) sends blocks into from wherever a test wants.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tutti_clap_test_plugin::Site;
use tutti_fixtures::main_thread;
use tutti_plugin_ipc::{AudioSlab, BridgeMessage, HostMessage, ProcessAudioData};
use tutti_plugin_types::{BridgeError, BridgeResult, ContentScale, EditorSize, Load, SampleFormat};

use super::{Native, serve_every};

#[cfg(all(unix, not(target_os = "macos")))]
mod x11;
use crate::render::{RenderSide, RenderThread, ToRender};
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
    /// Does something (on the loop's thread, when the script reaches it).
    Call(Box<dyn FnOnce() + Send>),
}

/// A host that follows a script and records what it is sent.
struct Script {
    steps: VecDeque<Step>,
    heard: Vec<BridgeMessage>,
    /// When the loop last went back to waiting, and the stretches it was away
    /// from the wait for longer than [`STALL`]: its own work (a tick, a message)
    /// took that long.
    left_at: Option<Instant>,
    stalls: Vec<(Instant, Instant)>,
}

/// How long the loop must be away from its wait to be counted stalled.
const STALL: Duration = Duration::from_millis(100);

impl Script {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            heard: Vec::new(),
            left_at: None,
            stalls: Vec::new(),
        }
    }
}

impl Transport for Script {
    fn wait_readable(&mut self, timeout: Duration) -> BridgeResult<bool> {
        let now = Instant::now();
        if let Some(left) = self.left_at
            && now.duration_since(left) > STALL
        {
            self.stalls.push((left, now));
        }
        let ready = self.wait_inner(timeout);
        self.left_at = Some(Instant::now());
        ready
    }

    fn recv(&mut self) -> BridgeResult<HostMessage> {
        self.recv_inner()
    }

    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
        self.heard.push(msg.clone());
        Ok(())
    }
}

impl Script {
    fn wait_inner(&mut self, timeout: Duration) -> BridgeResult<bool> {
        if let Some(Step::Quiet(length)) = self.steps.front() {
            let until = Instant::now() + *length;
            self.steps[0] = Step::QuietUntil(until);
        }
        match self.steps.front() {
            Some(Step::Call(_)) => {
                let Some(Step::Call(call)) = self.steps.pop_front() else {
                    unreachable!("matched a call")
                };
                call();
                Ok(false)
            }
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

    fn recv_inner(&mut self) -> BridgeResult<HostMessage> {
        match self.steps.pop_front() {
            Some(Step::Send(message)) => Ok(message),
            _ => Err(BridgeError::ConnectionFailed("the script is over".into())),
        }
    }
}

/// The server's end of the audio connection, in memory.
struct AudioEnd {
    requests: mpsc::Receiver<HostMessage>,
    replies: mpsc::Sender<BridgeMessage>,
    peeked: Option<HostMessage>,
}

impl Transport for AudioEnd {
    fn wait_readable(&mut self, timeout: Duration) -> BridgeResult<bool> {
        if self.peeked.is_none() {
            match self.requests.recv_timeout(timeout) {
                Ok(msg) => self.peeked = Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
                // `recv` reports the host gone.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(true),
            }
        }
        Ok(true)
    }

    fn recv(&mut self) -> BridgeResult<HostMessage> {
        match self.peeked.take() {
            Some(msg) => Ok(msg),
            None => self
                .requests
                .recv()
                .map_err(|_| BridgeError::ConnectionFailed("the host went".into())),
        }
    }

    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
        self.replies
            .send(msg.clone())
            .map_err(|_| BridgeError::ConnectionFailed("the host went".into()))
    }
}

/// The host's end of the audio connection.
struct HostEnd {
    requests: mpsc::Sender<HostMessage>,
    replies: mpsc::Receiver<BridgeMessage>,
}

impl HostEnd {
    /// Sends block `seq` of `frames` and waits up to `within` for its answer:
    /// `None` when none came.
    fn block(&self, seq: u64, frames: usize, within: Duration) -> Option<BridgeMessage> {
        self.requests
            .send(HostMessage::ProcessAudio(ProcessAudioData {
                seq,
                num_samples: frames,
                ..Default::default()
            }))
            .ok()?;
        self.replies.recv_timeout(within).ok()
    }
}

/// A session, its render side and the host's ends of both connections.
struct Fixture {
    session: Session,
    side: Option<RenderSide>,
    audio: Option<AudioEnd>,
    host: Option<HostEnd>,
}

impl Fixture {
    /// The host's end of the audio connection, taken once.
    fn host(&mut self) -> HostEnd {
        self.host.take().expect("the host's end is taken once")
    }
}

const TICK: Duration = Duration::from_millis(10);

fn load_message() -> HostMessage {
    HostMessage::LoadPlugin {
        path: std::path::PathBuf::from(crate::test_utils::clap_probe_path_dot_clap()),
        load: Load::new(),
        sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
        block_size: 512,
        preferred_format: SampleFormat::Float32,
        shm_name: String::new(),
        state: None,
    }
}

/// A session with a slab, as the handshake leaves it.
fn session(name: &str) -> (Fixture, tutti_plugin_ipc::SlabWriter) {
    let (mut session, side) = Session::new();
    let layout = tutti_plugin_ipc::SlabLayout {
        samples_per_channel: 512,
        format: SampleFormat::Float32,
        slots: tutti_plugin_ipc::RING_SLOTS as u32,
        inputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
        outputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
    };
    let shm = format!("tutti_evloop_{name}_{}", std::process::id());
    let creator = AudioSlab::create(shm.clone(), layout.clone()).expect("slab");
    session
        .render
        .send(ToRender::Slab(AudioSlab::open(shm, layout).expect("slab")));
    let (requests, host_requests) = mpsc::channel();
    let (replies, host_replies) = mpsc::channel();
    (
        Fixture {
            session,
            side: Some(side),
            audio: Some(AudioEnd {
                requests: host_requests,
                replies,
                peeked: None,
            }),
            host: Some(HostEnd {
                requests,
                replies: host_replies,
            }),
        },
        creator,
    )
}

fn run(fixture: &mut Fixture, script: &mut Script) {
    let main = main_thread();
    let render = RenderThread::spawn(
        fixture.side.take().expect("run once"),
        Box::new(fixture.audio.take().expect("run once")),
    )
    .expect("the render thread");
    let result = serve_every(
        &mut fixture.session,
        script,
        &mut Native,
        &main,
        &render,
        TICK,
    );
    // As the server ends: the plugin goes home, then the render thread stops.
    fixture.session.drop_plugin(&main);
    drop(render.stop());
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
    let host = session.host();
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Call(Box::new(move || {
            assert!(
                host.block(1, 64, Duration::from_secs(5)).is_some(),
                "the render thread answers the block"
            );
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

/// Makes every plugin timer take this long, as a slow editor paint does; none
/// again on drop.
struct SlowPaint;

impl SlowPaint {
    fn of(millis: u32) -> Self {
        crate::test_utils::clap().set(tutti_clap_test_plugin::ClapSwitch::TimerStall, millis);
        Self
    }
}

impl Drop for SlowPaint {
    fn drop(&mut self) {
        crate::test_utils::clap().set(tutti_clap_test_plugin::ClapSwitch::TimerStall, 0);
    }
}

/// **A slow editor paint on the main loop delays no block** (doc 029 section
/// 6.5). The plugin's timer, served by the loop's tick, takes 250 ms each time,
/// which is what a slow `editor_idle` does to the loop. The host sends a block
/// every 5 ms through all of it, and each must be answered well inside a block
/// deadline, including the blocks sent and answered entirely while the loop
/// was away in the paint. With the render on the main loop (as it was) a block
/// that arrives in a paint waits for it to end, a quarter of a second.
#[test]
fn a_slow_paint_on_the_main_loop_delays_no_block() {
    /// The deadline a block's answer must make: generous for a loaded
    /// machine, and well under the paint.
    const DEADLINE: Duration = Duration::from_millis(100);
    const PAINT_MS: u32 = 250;

    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_TIMER);
    let _paint = SlowPaint::of(PAINT_MS);
    let (mut session, _slab) = session("paint");
    let host = session.host();
    // (sent, answered) of every block the host sent after the first.
    let sent: std::sync::Arc<std::sync::Mutex<Vec<(Instant, Option<Instant>)>>> =
        std::sync::Arc::default();
    let sender = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Call(Box::new({
            let (sent, sender) = (sent.clone(), sender.clone());
            move || {
                // The first block asks the plugin for its callback, which
                // registers the timer the paint runs in.
                assert!(host.block(1, 64, Duration::from_secs(5)).is_some());
                *sender.lock().unwrap() = Some(std::thread::spawn(move || {
                    let until = Instant::now() + Duration::from_millis(900);
                    for seq in 2.. {
                        if Instant::now() >= until {
                            break;
                        }
                        let at = Instant::now();
                        let answered = host
                            .block(seq, 64, Duration::from_secs(2))
                            .map(|_| Instant::now());
                        sent.lock().unwrap().push((at, answered));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }));
            }
        })),
        Step::Quiet(Duration::from_millis(1000)),
    ]);

    run(&mut session, &mut script);
    sender
        .lock()
        .unwrap()
        .take()
        .expect("the host ran")
        .join()
        .unwrap();

    assert!(
        script
            .stalls
            .iter()
            .any(|(a, b)| b.duration_since(*a) >= DEADLINE * 2),
        "the paint stalled the loop: {:?}",
        script.stalls
    );
    let sent = sent.lock().unwrap();
    let late: Vec<_> = sent
        .iter()
        .filter(|(at, answered)| answered.is_none_or(|a| a.duration_since(*at) > DEADLINE))
        .collect();
    assert!(late.is_empty(), "blocks answered late or never: {late:?}");
    let during = sent
        .iter()
        .filter(|(at, answered)| {
            let answered = answered.expect("none are late");
            script.stalls.iter().any(|(a, b)| at >= a && answered <= *b)
        })
        .count();
    assert!(
        during >= 10,
        "{during} blocks were served while the loop was away in a paint"
    );
}
