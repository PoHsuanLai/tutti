//! A failed load is answered, and the server stays up.
//!
//! Drives the real `plugin-server` binary over its control socket: a file that
//! is not a plugin gets an `Error` reply carrying the loader's reason, and the
//! same server then loads a good plugin. A server that exited on the failed
//! load (the host reports that as `ProcessCrashed`) fails the second step.
//!
//! The wire is unchanged: `BridgeMessage::Error` already existed and the host
//! already turns it into `LoadFailed`, so `PROTOCOL_VERSION` is untouched.

#![cfg(all(unix, feature = "clap", feature = "vst3"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use interprocess::local_socket::Stream;
use interprocess::local_socket::traits::Stream as _;
use tutti_plugin_ipc::{BridgeMessage, HostMessage, read_frame, socket_name, write_frame};
use tutti_plugin_types::SampleFormat;
use tutti_plugin_types::{Load, SampleRate};

/// Kills the server if the test fails before it shuts it down.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(socket: &Path, server: &mut Server) -> Stream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(stream) = Stream::connect(socket_name(socket).expect("a socket name")) {
            return stream;
        }
        assert!(
            server.0.try_wait().expect("poll the server").is_none(),
            "the server exited before it bound its socket"
        );
        assert!(Instant::now() < deadline, "the server never bound");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn load(stream: &mut Stream, path: &Path) -> BridgeMessage {
    load_choosing(stream, path, Load::new())
}

fn load_choosing(stream: &mut Stream, path: &Path, load: Load) -> BridgeMessage {
    write_frame(
        stream,
        &HostMessage::LoadPlugin {
            path: path.to_path_buf(),
            load,
            sample_rate: SampleRate(48_000.0),
            block_size: 512,
            preferred_format: SampleFormat::Float32,
            shm_name: "load-reply".into(),
            state: None,
        },
    )
    .expect("send LoadPlugin");
    read_frame(stream).expect("the server answers a load; it did not exit")
}

/// The reference CLAP plugin's library under a `.vst3` name, which the server
/// reads as VST3 and the library is not, and under its `.clap` name, which
/// loads. The library's own name is `.so`, `.dylib` or `.dll` by platform, and
/// the server dispatches on the extension, so both names are links.
fn fixtures() -> (PathBuf, PathBuf) {
    let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    let not_a_plugin = tutti_fixtures::publish_with_extension(&real, "vst3");
    let good = tutti_fixtures::publish_with_extension(&real, "clap");
    (not_a_plugin, good)
}

/// A `plugin-server` process and its handshake connection, past `Ready`.
fn start_server() -> (Server, Stream) {
    // The default config makes a fresh socket path that fits a socket address.
    let socket = tutti_plugin_ipc::BridgeConfig::default().socket_path;

    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_plugin-server"))
            .arg(&socket)
            .env("TUTTI_PLUGIN_HOST_PID", std::process::id().to_string())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn plugin-server"),
    );
    let mut stream = connect(&socket, &mut server);
    assert!(matches!(
        read_frame::<_, BridgeMessage>(&mut stream),
        Ok(BridgeMessage::Ready { .. })
    ));
    (server, stream)
}

/// A load that fails gets the loader's reason and leaves the server serving.
#[test]
fn a_failed_load_is_answered_and_the_server_still_loads_a_good_plugin() {
    let (not_a_plugin, good) = fixtures();
    let (mut server, mut stream) = start_server();

    match load(&mut stream, &not_a_plugin) {
        BridgeMessage::Error { message } => {
            assert!(
                !message.contains("Unsupported plugin format"),
                "the library was refused before a format read it: {message}"
            );
            assert!(
                message.starts_with("Opening:") || message.contains("ailed"),
                "the reply names the format's reason: {message}"
            );
        }
        other => panic!("expected an Error reply, got {other:?}"),
    }
    assert!(
        server.0.try_wait().expect("poll the server").is_none(),
        "the server stays alive after a failed load"
    );

    assert!(
        matches!(load(&mut stream, &good), BridgeMessage::PluginLoaded { .. }),
        "the same server loads a good plugin after a failed one"
    );

    write_frame(&mut stream, &HostMessage::Shutdown).expect("send Shutdown");
    drop(server);
}

/// **A load that chooses a plugin of a file that exports several loads that
/// one, through the real `plugin-server`** (the choice crosses the wire in
/// `LoadPlugin::load`, protocol 28). The reference CLAP file is a factory of
/// two: the first without a choice, the second when named.
#[test]
fn a_load_that_names_a_plugin_loads_that_plugin_in_the_server() {
    let (_not_a_plugin, good) = fixtures();
    let (server, mut stream) = start_server();
    let id_of = |reply: BridgeMessage| match reply {
        BridgeMessage::PluginLoaded { descriptor, .. } => descriptor.id,
        other => panic!("expected PluginLoaded, got {other:?}"),
    };
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID
        .to_str()
        .expect("utf-8");

    assert_eq!(
        id_of(load(&mut stream, &good)),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8"),
        "no choice loads the first plugin"
    );
    assert_eq!(
        id_of(load_choosing(
            &mut stream,
            &good,
            Load::new().plugin(second)
        )),
        second,
        "the named plugin loads, not the first"
    );

    write_frame(&mut stream, &HostMessage::Shutdown).expect("send Shutdown");
    drop(server);
}
