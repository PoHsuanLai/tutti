//! A failed load is answered, and the server stays up.
//!
//! Drives the real `plugin-server` binary over its control socket: a file that
//! is not a plugin gets an `Error` reply carrying the loader's reason, and the
//! same server then loads a good plugin. A server that exited on the failed
//! load (the host reports that as `ProcessCrashed`) fails the second step.
//!
//! The wire is unchanged: `BridgeMessage::Error` already existed and the host
//! already turns it into `LoadFailed`, so `PROTOCOL_VERSION` is untouched.

#![cfg(all(unix, feature = "clap", feature = "vst2"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use interprocess::local_socket::Stream;
use interprocess::local_socket::traits::Stream as _;
use tutti_plugin::server::{
    BridgeMessage, HostMessage, SampleFormat, read_frame, socket_name, write_frame,
};
use tutti_plugin_types::SampleRate;

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
    write_frame(
        stream,
        &HostMessage::LoadPlugin {
            path: path.to_path_buf(),
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

/// The reference CLAP plugin's library under a `.vst` name, which the server
/// reads as VST2 and the library is not, and under its `.clap` name, which
/// loads. The library's own name is `.so`, `.dylib` or `.dll` by platform, and
/// the server dispatches on the extension, so both names are links.
fn fixtures() -> (PathBuf, PathBuf) {
    let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    let not_a_plugin = tutti_fixtures::publish_with_extension(&real, "vst");
    let good = tutti_fixtures::publish_with_extension(&real, "clap");
    (not_a_plugin, good)
}

/// A load that fails gets the loader's reason and leaves the server serving.
///
/// Mutation: restore the `?` on `Plugin::load` in `Session::handle_load` and
/// the first read fails with the server gone (end of file).
#[test]
fn a_failed_load_is_answered_and_the_server_still_loads_a_good_plugin() {
    let dir = std::env::temp_dir().join(format!("tutti-load-reply-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let (not_a_plugin, good) = fixtures();
    let socket = dir.join("s.sock");

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

    match load(&mut stream, &not_a_plugin) {
        BridgeMessage::Error { message } => {
            assert!(
                !message.contains("Unsupported plugin format"),
                "the library reached a VST2 loader: {message}"
            );
            #[cfg(target_os = "linux")]
            assert!(
                message.contains("NotAPlugin"),
                "the reply names the loader's reason: {message}"
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
    let _ = std::fs::remove_dir_all(&dir);
}
