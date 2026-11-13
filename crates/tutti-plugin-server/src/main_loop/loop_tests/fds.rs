//! The plugin's descriptors, waited on by the loop together with the socket
//! (unix: CLAP's `posix-fd-support`). One `cfg` on this module's declaration
//! holds the test and its helpers.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use tutti_plugin_ipc::{read_frame, write_frame};

use super::*;
use crate::main_loop::Stream;
use crate::transport::SocketTransport;

/// How long the loop idles between ticks here: far longer than the latency the
/// test demands, so a descriptor served at the tick would not pass.
const SLOW_TICK: Duration = Duration::from_millis(600);

/// **A descriptor the plugin registered is served the instant it is readable,
/// well under a tick.** The loop waits on the control socket and the plugin's
/// descriptors in one `poll`. The plugin registers a pipe, and the test makes
/// it readable five times, each time asking for `on_fd` within 250 ms of a loop
/// that ticks every 600 ms: one served at the tick would be late about three times in
/// five.
#[test]
fn a_registered_descriptor_that_becomes_readable_is_served_well_under_a_tick() {
    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_FD);
    let (mut fixture, _slab) = session("fds");
    let host = fixture.host();

    // A real socket pair: the loop polls the socket's descriptor together with
    // the plugin's.
    let (mut host_end, server_end) = UnixStream::pair().expect("socket pair");
    let mut transport = SocketTransport::new(Stream::from(OwnedFd::from(server_end)));

    let driver = std::thread::spawn(move || {
        write_frame(&mut host_end, &load_message()).expect("sends the load");
        let _loaded: BridgeMessage = read_frame(&mut host_end).expect("the load is answered");
        assert!(
            host.block(1, 64, Duration::from_secs(5)).is_some(),
            "the render thread answers the block"
        );
        // The block asked for a callback; the tick serves it, and it registers
        // the descriptor.
        let registered_by = Instant::now() + Duration::from_secs(5);
        while !crate::test_utils::clap_probe_thread_capture().fd_registered {
            assert!(
                Instant::now() < registered_by,
                "the descriptor was never registered"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut slowest = Duration::ZERO;
        for fire in 1..=5u32 {
            // Let the loop settle into its long wait.
            std::thread::sleep(Duration::from_millis(40));
            let poked = Instant::now();
            assert!(
                crate::test_utils::clap().fd_poke(),
                "the pipe is registered"
            );
            while crate::test_utils::clap_probe_thread_capture().fd_fires < fire {
                assert!(
                    poked.elapsed() < Duration::from_secs(3),
                    "on_fd was never called"
                );
                std::thread::sleep(Duration::from_micros(500));
            }
            slowest = slowest.max(poked.elapsed());
        }
        write_frame(&mut host_end, &HostMessage::Shutdown).expect("asks it to stop");
        slowest
    });

    let main = main_thread();
    let render = RenderThread::spawn(
        fixture.side.take().expect("run once"),
        Box::new(fixture.audio.take().expect("run once")),
    )
    .expect("the render thread");
    let result = serve_every(
        &mut fixture.session,
        &mut transport,
        &mut Native::default(),
        &main,
        &render,
        SLOW_TICK,
    );
    fixture.session.drop_plugin(&main);
    drop(render.stop());
    assert!(result.is_ok(), "{result:?}");

    let slowest = driver.join().expect("the driver ran");
    let cap = probe.thread_capture();
    assert_eq!(
        cap.fd_fires, 5,
        "one call of `on_fd` for each time it was readable"
    );
    assert_eq!(
        cap.fd_last_events, 1,
        "told it was readable (CLAP_POSIX_FD_READ)"
    );
    assert!(
        slowest < Duration::from_millis(250),
        "the slowest `on_fd` came {slowest:?} after its descriptor was readable, \
         on a loop that ticks every {SLOW_TICK:?}"
    );
}
