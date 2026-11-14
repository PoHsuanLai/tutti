//! A Client and a Host over real UDP sockets on the loopback interface.

use std::time::{Duration, Instant};

use tutti_midi_network::{
    ByeReason, Client, Credentials, Event, Host, HostConfig, Initiator, SessionConfig,
    SessionState, UdpDriver,
};

use crate::sim::{identity, note_on};

/// Polls both drivers until `done` holds or the deadline passes.
fn run_until(
    host: &mut UdpDriver<Host>,
    client: &mut UdpDriver<Client>,
    events: &mut Vec<(&'static str, Event)>,
    mut done: impl FnMut(&[(&'static str, Event)]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !done(events) {
        host.poll(Duration::from_millis(5)).unwrap();
        client.poll(Duration::from_millis(5)).unwrap();
        while let Some(e) = host.poll_event() {
            events.push(("host", e));
        }
        while let Some(e) = client.poll_event() {
            events.push(("client", e));
        }
    }
    assert!(done(events), "timed out; events so far: {events:?}");
}

#[test]
fn a_session_runs_over_real_udp_sockets_from_invitation_to_bye() {
    let mut host = UdpDriver::bind(
        "127.0.0.1:0",
        Host::new(HostConfig::new(identity("Studio", "HOST-1"))),
    )
    .unwrap();
    let host_addr = host.local_addr().unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_addr = socket.local_addr().unwrap();
    let mut client = UdpDriver::new(
        socket,
        Client::new(
            identity("Laptop", "CLIENT-1"),
            Credentials::none(),
            SessionConfig::default(),
            host_addr,
            Instant::now(),
        ),
    );
    let mut events = Vec::new();

    client.endpoint_mut().connect(Instant::now());
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter()
            .filter(|(_, ev)| matches!(ev, Event::Established { .. }))
            .count()
            == 2
    });
    assert_eq!(client.endpoint().state(), SessionState::Established);

    // Client to host and back.
    let notes = [note_on(60), note_on(64), note_on(67)];
    client.endpoint_mut().send(Instant::now(), &notes).unwrap();
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter()
            .filter(|(who, ev)| *who == "host" && matches!(ev, Event::Ump { .. }))
            .count()
            == 3
    });
    host.endpoint_mut()
        .send(Instant::now(), client_addr, &[note_on(72)])
        .unwrap();
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter().any(|(who, ev)| {
            *who == "client" && matches!(ev, Event::Ump { packet, .. } if *packet == note_on(72))
        })
    });
    let heard: Vec<_> = events
        .iter()
        .filter_map(|(who, ev)| match ev {
            Event::Ump { packet, .. } if *who == "host" => Some(*packet),
            _ => None,
        })
        .collect();
    assert_eq!(heard, notes, "UMPs arrive once and in order");

    client
        .endpoint_mut()
        .close(Instant::now(), ByeReason::UserTerminated);
    run_until(&mut host, &mut client, &mut events, |e| {
        e.iter()
            .filter(|(_, ev)| matches!(ev, Event::Closed { .. }))
            .count()
            == 2
    });
    assert!(events.iter().any(|(who, ev)| {
        *who == "host"
            && matches!(
                ev,
                Event::Closed {
                    initiator: Initiator::Remote,
                    reason: ByeReason::UserTerminated,
                    ..
                }
            )
    }));
    assert_eq!(client.endpoint().state(), SessionState::Idle);
}
