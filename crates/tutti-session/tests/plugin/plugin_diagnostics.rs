//! A hosted plugin's diagnostics and buses (design 029 step 4): the counts and
//! figures a host shows, whichever hosting runs the plugin, and the graph's bus
//! handles for wiring it.
//!
//! CLAP over the reference plugin in a real `plugin-server` (an aux bus needs
//! its aux-bus port layout; the server's pid and its termination); VST2 over
//! the reference plugin in process (a count that is the same figure in both
//! hostings: lost MIDI).

use tutti_plugin::catalog::PluginRef;
use tutti_plugin_types::MainThread;
use tutti_session::{PluginLoadRequest, Root, Session};
use tutti_types::{ChannelLayout, InterleavedMut, MaxBlock, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 512;

fn main_thread() -> MainThread {
    MainThread::claim().expect("one test, one process")
}

fn offline() -> Session {
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(BLOCK)))
        .expect("an offline session builds")
}

fn render_block(s: &Session) {
    let mut out = vec![0.0f32; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
}

#[cfg(feature = "clap")]
mod clap {
    use super::*;
    use crate::support::{clap_probe, plugin_server};
    use tutti_graph::{BusError, ForkByClone};
    use tutti_nodes::testing::Const;
    use tutti_session::ConnectError;
    use tutti_session::{Key, PluginNode};

    fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: nextest runs each test in its own process, and every variable
        // is set before the first server is spawned, from the test's own thread.
        unsafe { std::env::set_var(key, value) };
    }

    fn loaded(layout: Option<&str>) -> (Session, Key<PluginNode>) {
        set_env("TUTTI_PLUGIN_SERVER", plugin_server());
        if let Some(layout) = layout {
            set_env("TUTTI_CLAP_PROBE_PORT_LAYOUT", layout);
        }
        let mut s = offline();
        let node = s
            .add_plugin_blocking(
                &main_thread(),
                PluginLoadRequest::new(PluginRef::file(clap_probe()).expect("a plugin file")),
            )
            .expect("the reference CLAP plugin loads");
        (s, node)
    }

    /// **A plugin's buses are the graph's bus handles.** The probe with a mono aux
    /// input beside its stereo main: `main_input` is bus 0 and the aux input is
    /// bus 1 through `input(1)`,
    /// the same handle `key.bus_in(1)` names; the output buses likewise; a
    /// source's bus connects into it and the graph says what feeds it; a bus
    /// the plugin lacks is not a handle and the graph refuses it as a bus the
    /// node does not declare.
    #[test]
    fn an_aux_input_wires_through_the_buses_handle() {
        let (mut s, node) = loaded(Some("1"));
        let key = node;
        let buses = s.plugin(node).unwrap().buses();
        assert_eq!(buses.main_input(), Some(key.bus_in(0)));
        assert_eq!(buses.input(1), Some(key.bus_in(1)));
        assert_eq!(buses.output(0), Some(key.bus_out(0)));
        assert_eq!(buses.output(1), Some(key.bus_out(1)));
        assert_eq!((buses.input_count(), buses.output_count()), (2, 2));
        assert_eq!(buses.output(2), None);
        assert_eq!(buses.input(2), None);

        let aux = buses.input(1).expect("the probe has an aux input");
        let (kick, _) = s.graph().add(ForkByClone(Const::mono(0.5)));
        s.graph()
            .connect(kick.bus_out(0), aux)
            .expect("a mono source feeds the mono aux input");
        assert_eq!(s.graph().bus_sources(aux).unwrap(), [kick.bus_out(0)]);
        let _ = s.update();

        let refused = s.graph().connect(kick.bus_out(0), key.bus_in(2));
        assert!(
            matches!(refused, Err(ConnectError::Bus(BusError::NoSuchBus { .. }))),
            "a bus past the plugin's own is no input: {:?}",
            refused.map(|_| ())
        );
    }

    /// A plugin with one bus per side has no second input, and its main bus is 0.
    #[test]
    fn a_plugin_with_one_input_bus_has_no_second_input() {
        let (s, node) = loaded(None);
        let buses = s.plugin(node).unwrap().buses();
        assert!(buses.input(1).is_none());
        assert!(buses.main_input().is_some());
        assert_eq!(buses.input_count(), 1);
    }

    /// **The server is named, its termination is the plugin's death, and its
    /// process ends.** `server_pid` is a live process; `terminate(Hung { .. })` ends it,
    /// and the view reads `Died` with the cause `Terminated { reason: Hung }`.
    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_ends_the_server_and_the_plugin_reads_died() {
        use tutti_plugin_types::{PluginFailure, TerminationReason};
        use tutti_session::PluginHealth;

        let (mut s, node) = loaded(None);
        s.graph().connect(node, Root).unwrap();
        let _ = s.update();
        let pid = s
            .plugin(node)
            .unwrap()
            .diagnostics()
            .server_pid()
            .expect("an out-of-process plugin has a server");
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the server is a live process"
        );

        s.plugin(node)
            .unwrap()
            .diagnostics()
            .terminate(TerminationReason::Hung {
                blocks: 16,
                held: std::time::Duration::from_secs(5),
            });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let cause = loop {
            if let PluginHealth::Died { cause } = s.plugin(node).unwrap().health() {
                break cause;
            }
            assert!(std::time::Instant::now() < deadline, "it never read Died");
            render_block(&s);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(
            matches!(
                cause,
                PluginFailure::Terminated {
                    reason: TerminationReason::Hung { .. }
                }
            ),
            "{cause:?}"
        );
    }

    /// **The latency is the one the graph compensates: the plugin's report plus
    /// the pipeline's chunk.** `reported_latency` is the probe's own figure and
    /// `latency` adds one block; the tail is the probe's `clap.tail`. (A report
    /// that moves is `plugin_latency.rs`'s.)
    #[test]
    fn the_diagnostics_latency_is_the_report_plus_the_pipeline() {
        let (mut s, node) = loaded(None);
        s.graph().connect(node, Root).unwrap();
        let _ = s.update();
        let diag = s.plugin(node).unwrap().diagnostics();
        let own = tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize;
        assert_eq!(diag.reported_latency(), Samples(own));
        assert_eq!(diag.latency(), Samples(own + BLOCK));
        assert_eq!(
            diag.tail(),
            tutti_types::Tail::Finite(Samples(8191)),
            "the tail the probe's `clap.tail` reports, as the graph holds it"
        );
    }
}

#[cfg(feature = "vst2")]
mod vst2 {
    use super::*;

    use tutti_fixtures::midi;
    use tutti_graph::EventCapacity;
    use tutti_session::Losses;
    use tutti_ump::MidiChannel;

    /// **Lost MIDI is counted as the MIDI inlets count theirs.** A block with 300
    /// notes holds 256 (`MIDI_STACK_CAPACITY`): the 44 past it are `lost` and
    /// nothing else; a block within the cap adds nothing; and the same view
    /// reports a plugin that runs in this process with no server and no
    /// pipeline.
    #[test]
    fn midi_past_a_blocks_capacity_reads_as_lost() {
        let mut s = offline();
        let node = s
            .add_plugin_blocking(
                &main_thread(),
                PluginLoadRequest::new(
                    PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())
                        .expect("a plugin file"),
                ),
            )
            .expect("the in-process VST2 loads");
        s.graph().connect(node, Root).unwrap();
        let (inlet, mut writer) = s.graph().open_event_inlet(EventCapacity::of(512));
        s.graph()
            .connect(inlet.events_out(), node.events_in())
            .unwrap();
        let _ = s.update();

        let diag = || s.plugin(node).unwrap().diagnostics();
        assert_eq!(diag().midi_in(), Losses::NONE, "nothing sent yet");
        assert_eq!(diag().server_pid(), None, "no server in process");
        assert_eq!(diag().missed_chunks(), 0, "no pipeline in process");
        assert!(diag().follows_project_meter());
        assert_eq!(
            diag().tail(),
            tutti_types::Tail::None,
            "the VST2 probe reports no tail"
        );

        let notes =
            |n: usize| (0..n).map(|i| midi::note_on(MidiChannel::FIRST, (i % 128) as u8, 0xFFFF));
        writer.push_all(notes(300).map(|n| (tutti_session::Stamp::NextBlock, n)));
        render_block(&s);
        assert_eq!(
            diag().midi_in(),
            Losses::lost(300 - tutti_plugin_types::MIDI_STACK_CAPACITY as u64)
        );
        writer.push_all(notes(10).map(|n| (tutti_session::Stamp::NextBlock, n)));
        render_block(&s);
        assert_eq!(
            diag().midi_in(),
            Losses::lost(300 - tutti_plugin_types::MIDI_STACK_CAPACITY as u64),
            "a block within the cap lost nothing more"
        );
    }
}
