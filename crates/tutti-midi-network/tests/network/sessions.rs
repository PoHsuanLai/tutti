//! A Client and a Host over a simulated network: Session establishment and
//! authentication, data transfer with loss, keep-alive, reset and Bye.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tutti_midi_network::{
    Admission, AuthFailure, AuthMethods, AuthState, Bye, ByeReason, Command, Credentials,
    DatagramBuilder, Digest, Endpoint, Event, Fec, HostConfig, Initiator, Invitation, NakReason,
    Nonce, PingId, Requests, Retransmit, RetransmitRequest, SendError, SequenceNumber,
    SessionConfig, SessionState, UmpData, UserAuthState, Username,
};

use crate::sim::{Direction, Fate, Sim, client_addr, fixed_nonce, host_addr, identity, note_on};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn datagram(command: &Command) -> Vec<u8> {
    let mut b = DatagramBuilder::new();
    b.push(command);
    b.finish()
}

fn host_with(admission: Admission) -> HostConfig {
    let mut config = HostConfig::new(identity("Studio", "HOST-1"));
    config.admission = admission;
    config
}

fn established() -> Sim {
    let mut sim = Sim::open();
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    sim.client_events.clear();
    sim.host_events.clear();
    sim.log.clear();
    sim
}

fn data_commands(sim: &Sim, dir: Direction) -> Vec<UmpData> {
    sim.commands(dir)
        .into_iter()
        .filter_map(|c| match c {
            Command::UmpData(d) => Some(d),
            _ => None,
        })
        .collect()
}

// ---- Establishing a Session ----

#[test]
fn an_invitation_to_an_open_host_establishes_both_ends_and_each_learns_the_other() {
    let mut sim = Sim::open();
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
    assert_eq!(
        sim.client_events,
        vec![Event::Established {
            peer: host_addr(),
            remote: identity("Studio", "HOST-1")
        }]
    );
    assert_eq!(
        sim.host_events,
        vec![Event::Established {
            peer: client_addr(),
            remote: identity("Laptop", "CLIENT-1")
        }]
    );
}

#[test]
fn a_pending_reply_waits_for_the_application_then_accepts() {
    let mut sim = Sim::new(
        host_with(Admission::Confirm),
        Credentials::none(),
        SessionConfig::default(),
    );
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::PendingInvitation);
    assert!(matches!(sim.host_events[..], [Event::Confirm { .. }]));
    // The client stops repeating while the host is pending.
    let invitations_before = sim.commands(Direction::ClientToHost).len();
    sim.advance(ms(2000));
    let invitations: usize = sim
        .commands(Direction::ClientToHost)
        .iter()
        .filter(|c| matches!(c, Command::Invitation(_)))
        .count();
    assert_eq!(invitations, 1, "no repeat after a Pending reply");
    assert!(sim.commands(Direction::ClientToHost).len() >= invitations_before);
    sim.host.accept(sim.now, client_addr());
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

#[test]
fn a_declined_invitation_ends_with_the_user_declined_reason() {
    let mut sim = Sim::new(
        host_with(Admission::Confirm),
        Credentials::none(),
        SessionConfig::default(),
    );
    sim.connect();
    sim.host.decline(sim.now, client_addr());
    sim.flush();
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        initiator: Initiator::Remote,
        reason: ByeReason::UserDeclined
    }));
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

#[test]
fn a_lost_invitation_is_repeated_and_the_lost_reply_is_answered_again() {
    let mut sim = Sim::open();
    let mut dropped = 0;
    sim.set_fate(move |dir, _| {
        if dir == Direction::HostToClient && dropped < 1 {
            dropped += 1;
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::PendingInvitation);
    sim.advance(ms(500));
    assert_eq!(sim.client.state(), SessionState::Established);
    let invitations = sim
        .commands(Direction::ClientToHost)
        .iter()
        .filter(|c| matches!(c, Command::Invitation(_)))
        .count();
    assert_eq!(invitations, 2);
}

#[test]
fn an_unanswered_invitation_is_cancelled_with_a_bye_after_the_timeout() {
    let mut sim = Sim::open();
    sim.set_fate(|_, _| Fate::Drop);
    sim.connect();
    sim.advance(Duration::from_secs(5));
    let sent = sim.commands(Direction::ClientToHost);
    assert!(
        sent.iter()
            .filter(|c| matches!(c, Command::Invitation(_)))
            .count()
            >= 5
    );
    assert!(sent.contains(&Command::Bye(Bye::new(ByeReason::InvitationCanceled))));
    sim.advance(Duration::from_secs(6));
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        initiator: Initiator::Local,
        reason: ByeReason::InvitationCanceled
    }));
}

#[test]
fn a_repeated_invitation_in_an_established_session_is_accepted_again() {
    let mut sim = established();
    let again = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("Laptop", "CLIENT-1"),
    }));
    sim.host.handle_datagram(sim.now, client_addr(), &again);
    sim.flush();
    assert!(matches!(
        sim.commands(Direction::HostToClient)[..],
        [Command::Accepted(_)]
    ));
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

#[test]
fn a_malformed_invitation_is_a_protocol_error_bye() {
    let mut sim = Sim::open();
    // Name only: no Product Instance Id.
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x01, 0x01, 0x01, 0x00, b'a', 0, 0, 0]);
    sim.host.handle_datagram(sim.now, client_addr(), &bytes);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Bye::new(ByeReason::ProtocolError))]
    );
}

#[test]
fn a_host_at_its_limit_turns_a_newcomer_away_with_too_many_sessions() {
    let mut config = host_with(Admission::Open);
    config.max_sessions = std::num::NonZeroUsize::MIN;
    let mut sim = Sim::new(config, Credentials::none(), SessionConfig::default());
    sim.connect();
    let other = "10.0.0.3:6000".parse().unwrap();
    let inv = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("Other", "OTHER-1"),
    }));
    sim.host.handle_datagram(sim.now, other, &inv);
    let reply = sim.host.poll_transmit().unwrap();
    assert_eq!(reply.to, other);
    let commands: Vec<_> = tutti_midi_network::parse(&reply.payload)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        commands,
        vec![Command::Bye(Bye::new(ByeReason::TooManySessions))]
    );
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

#[test]
fn an_accepted_reply_with_no_invitation_gets_a_bye_for_no_pending_invitation() {
    let mut sim = Sim::open();
    let accepted = datagram(&Command::Accepted(identity("Studio", "HOST-1")));
    sim.client.handle_datagram(sim.now, host_addr(), &accepted);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::ClientToHost),
        vec![Command::Bye(Bye::new(ByeReason::NoPendingInvitation))]
    );
}

#[test]
fn a_client_without_a_host_naks_an_invitation_as_not_expected() {
    let mut sim = Sim::open();
    let inv = Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("X", "Y"),
    });
    sim.client
        .handle_datagram(sim.now, host_addr(), &datagram(&inv));
    sim.flush();
    let [Command::Nak(nak)] = &sim.commands(Direction::ClientToHost)[..] else {
        panic!("expected a NAK")
    };
    assert_eq!(nak.reason, NakReason::NotExpected);
    assert_eq!(nak.refused, inv.header());
}

// ---- Authentication ----

fn secret_sim(client_secret: &str) -> Sim {
    Sim::new(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().with_secret(client_secret),
        SessionConfig::default(),
    )
}

#[test]
fn the_right_shared_secret_establishes_the_session() {
    let mut sim = secret_sim("5483");
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    let to_client = sim.commands(Direction::HostToClient);
    assert!(matches!(
        to_client[0],
        Command::AuthRequired(ref c) if c.state == AuthState::First && c.nonce == fixed_nonce()
    ));
    let to_host = sim.commands(Direction::ClientToHost);
    assert!(
        to_host.contains(&Command::InvitationWithAuth(Digest::of_secret(
            &fixed_nonce(),
            "5483"
        )))
    );
}

#[test]
fn a_wrong_secret_is_answered_after_a_delay_and_the_client_gives_up() {
    let mut sim = secret_sim("0000");
    sim.connect();
    let challenges = |sim: &Sim| {
        sim.commands(Direction::HostToClient)
            .into_iter()
            .filter(|c| matches!(c, Command::AuthRequired(_)))
            .count()
    };
    assert_eq!(challenges(&sim), 1);
    sim.advance(ms(50));
    assert_eq!(
        challenges(&sim),
        1,
        "the reply to a wrong digest is held back"
    );
    sim.advance(ms(60));
    assert_eq!(challenges(&sim), 2);
    assert!(
        sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(
                c,
                Command::AuthRequired(c) if c.state == AuthState::DigestWrong
            ))
    );
    assert!(sim.client_events.contains(&Event::AuthenticationFailed {
        peer: host_addr(),
        cause: AuthFailure::DigestWrong
    }));
    assert!(
        sim.commands(Direction::ClientToHost)
            .contains(&Command::Bye(Bye::new(ByeReason::InvitationCanceled)))
    );
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

#[test]
fn the_delay_after_each_failed_digest_doubles() {
    let mut sim = secret_sim("5483");
    let guesser = "10.0.0.9:7000".parse().unwrap();
    let inv = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::SHARED_SECRET,
        identity: identity("Guess", "G-1"),
    }));
    let wrong = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    let drain = |sim: &mut Sim| {
        let mut n = 0;
        while sim.host.poll_transmit().is_some() {
            n += 1;
        }
        n
    };
    let start = sim.now;
    sim.host.handle_datagram(start, guesser, &inv);
    assert_eq!(drain(&mut sim), 1, "the challenge");
    // First failure: 100 ms.
    sim.host.handle_datagram(start, guesser, &wrong);
    sim.host.handle_timeout(start + ms(99));
    assert_eq!(drain(&mut sim), 0);
    sim.host.handle_timeout(start + ms(100));
    assert_eq!(drain(&mut sim), 1);
    // Second failure: 200 ms.
    let second = start + ms(100);
    sim.host.handle_datagram(second, guesser, &wrong);
    sim.host.handle_timeout(second + ms(199));
    assert_eq!(drain(&mut sim), 0);
    sim.host.handle_timeout(second + ms(200));
    assert_eq!(drain(&mut sim), 1);
}

#[test]
fn five_failed_digests_end_the_invitation_with_authentication_failed() {
    let mut sim = Sim::new(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().with_secret("5483"),
        SessionConfig::default(),
    );
    // A guesser invites, and answers every challenge with a wrong digest.
    let guesser = "10.0.0.9:7000".parse().unwrap();
    let inv = datagram(&Command::Invitation(Invitation {
        methods: AuthMethods::SHARED_SECRET,
        identity: identity("Guess", "G-1"),
    }));
    let wrong = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    sim.host.handle_datagram(sim.now, guesser, &inv);
    for _ in 0..5 {
        sim.host.handle_datagram(sim.now, guesser, &wrong);
    }
    let mut saw_bye = false;
    while let Some(t) = sim.host.poll_transmit() {
        for c in tutti_midi_network::parse(&t.payload)
            .unwrap()
            .map(Result::unwrap)
        {
            saw_bye |= c == Command::Bye(Bye::new(ByeReason::AuthenticationFailed));
        }
    }
    assert!(saw_bye);
}

#[test]
fn a_client_with_no_matching_credentials_is_told_there_is_no_matching_method() {
    let mut sim = Sim::new(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none(),
        SessionConfig::default(),
    );
    sim.connect();
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        initiator: Initiator::Remote,
        reason: ByeReason::NoMatchingAuthentication
    }));
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

static NONCE_CALLS: AtomicU8 = AtomicU8::new(0);

fn counting_nonce() -> Nonce {
    let n = NONCE_CALLS.fetch_add(1, Ordering::SeqCst);
    Nonce::from_entropy([n; 16])
}

#[test]
fn a_repeated_invitation_gets_the_same_nonce_and_a_new_session_a_new_one() {
    NONCE_CALLS.store(0, Ordering::SeqCst);
    let mut sim = Sim::with_nonces(
        host_with(Admission::SharedSecret("5483".into())),
        Credentials::none().with_secret("5483"),
        SessionConfig::default(),
        counting_nonce,
    );
    // The first challenge is lost, so the client repeats its invitation.
    let mut dropped = false;
    sim.set_fate(move |dir, _| {
        if dir == Direction::HostToClient && !dropped {
            dropped = true;
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.connect();
    sim.advance(ms(500));
    assert_eq!(sim.client.state(), SessionState::Established);
    let nonces: Vec<Nonce> = sim
        .commands(Direction::HostToClient)
        .into_iter()
        .filter_map(|c| match c {
            Command::AuthRequired(c) => Some(c.nonce),
            _ => None,
        })
        .collect();
    assert_eq!(nonces.len(), 2);
    assert_eq!(nonces[0], nonces[1]);
    assert_eq!(NONCE_CALLS.load(Ordering::SeqCst), 1);
    // A fresh invitation from the same peer after the session ends is new.
    sim.client.close(sim.now, ByeReason::UserTerminated);
    sim.flush();
    sim.log.clear();
    let mut again = tutti_midi_network::Client::new(
        identity("Laptop", "CLIENT-1"),
        Credentials::none().with_secret("5483"),
        SessionConfig::default(),
        host_addr(),
        sim.now,
    );
    again.connect(sim.now);
    let t = again.poll_transmit().unwrap();
    sim.host.handle_datagram(sim.now, client_addr(), &t.payload);
    sim.flush();
    let fresh = sim.commands(Direction::HostToClient);
    assert!(matches!(
        fresh[0],
        Command::AuthRequired(ref c) if c.nonce != nonces[0]
    ));
}

#[test]
fn an_authenticated_invitation_without_an_invitation_is_rejected_for_no_prior_invitation() {
    let mut sim = secret_sim("5483");
    let auth = datagram(&Command::InvitationWithAuth(Digest::new([0; 32])));
    sim.host.handle_datagram(sim.now, client_addr(), &auth);
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Bye::new(ByeReason::NoPriorInvitation))]
    );
}

fn users_sim(user: &str, password: &str) -> Sim {
    Sim::new(
        host_with(Admission::Users(vec![(
            Username::new("Rosa").unwrap(),
            "RPBqBno".into(),
        )])),
        Credentials::none().with_user(Username::new(user).unwrap(), password),
        SessionConfig::default(),
    )
}

#[test]
fn a_known_user_with_the_right_password_establishes_the_session() {
    let mut sim = users_sim("Rosa", "RPBqBno");
    sim.connect();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert!(matches!(
        sim.commands(Direction::HostToClient)[0],
        Command::UserAuthRequired(ref c) if c.state == UserAuthState::First
    ));
}

#[test]
fn an_unknown_user_is_told_the_user_was_not_found() {
    let mut sim = users_sim("Bob", "RPBqBno");
    sim.connect();
    sim.advance(ms(150));
    assert!(
        sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(
                c,
                Command::UserAuthRequired(c) if c.state == UserAuthState::UserNotFound
            ))
    );
    assert!(sim.client_events.contains(&Event::AuthenticationFailed {
        peer: host_addr(),
        cause: AuthFailure::UserNotFound
    }));
}

#[test]
fn a_wrong_password_is_told_the_digest_is_wrong() {
    let mut sim = users_sim("Rosa", "nope");
    sim.connect();
    sim.advance(ms(150));
    assert!(sim.client_events.contains(&Event::AuthenticationFailed {
        peer: host_addr(),
        cause: AuthFailure::DigestWrong
    }));
}

// ---- Data transfer ----

#[test]
fn ump_arrives_once_in_order_and_sequence_numbers_start_at_zero() {
    let mut sim = established();
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    assert_eq!(Sim::umps(&sim.host_events), notes);
    let first = data_commands(&sim, Direction::ClientToHost)[0].sequence();
    assert_eq!(first, SequenceNumber::ZERO);
}

#[test]
fn each_datagram_carries_the_two_previous_commands_before_the_new_one() {
    let mut sim = established();
    for n in 60..63 {
        sim.client.send(sim.now, &[note_on(n)]).unwrap();
    }
    let mut last = None;
    while let Some(t) = sim.client.poll_transmit() {
        last = Some(t);
    }
    let commands: Vec<_> = tutti_midi_network::parse(&last.unwrap().payload)
        .unwrap()
        .map(|c| match c.unwrap() {
            Command::UmpData(d) => d.sequence().get(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        commands,
        vec![0, 1, 2],
        "older commands first, in the order sent"
    );
}

#[test]
fn forward_error_correction_recovers_two_lost_datagrams_with_no_retransmit_request() {
    let mut sim = established();
    let mut seen = 0;
    sim.set_fate(move |dir, _| {
        if dir != Direction::ClientToHost {
            return Fate::Deliver;
        }
        seen += 1;
        // The second and third datagrams are lost.
        if seen == 2 || seen == 3 {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(ms(500));
    assert_eq!(Sim::umps(&sim.host_events), notes);
    assert!(
        !sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(c, Command::RetransmitRequest(_))),
        "FEC filled the gap before any request was due"
    );
}

fn no_fec() -> SessionConfig {
    SessionConfig::default().with_fec(Fec::Off)
}

fn lossy_sim(client: SessionConfig, lost: Vec<usize>) -> Sim {
    let mut sim = Sim::new(
        HostConfig::new(identity("Studio", "HOST-1")),
        Credentials::none(),
        client,
    );
    sim.connect();
    sim.client_events.clear();
    sim.host_events.clear();
    sim.log.clear();
    let mut seen = 0;
    sim.set_fate(move |dir, bytes| {
        let is_data = bytes.len() > 8 && bytes[4] == 0xFF;
        if dir != Direction::ClientToHost || !is_data {
            return Fate::Deliver;
        }
        seen += 1;
        if lost.contains(&seen) {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim
}

#[test]
fn a_missing_command_is_requested_after_the_wait_and_retransmitted_in_order() {
    let mut sim = lossy_sim(no_fec(), vec![2]);
    let notes: Vec<_> = (60..63).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    // Held, not yet delivered past the gap.
    assert_eq!(Sim::umps(&sim.host_events), vec![notes[0]]);
    sim.advance(ms(10));
    assert_eq!(Sim::umps(&sim.host_events), notes);
    assert!(
        sim.commands(Direction::HostToClient)
            .contains(&Command::RetransmitRequest(RetransmitRequest {
                first: SequenceNumber::new(1),
                count: 1
            }))
    );
    assert!(
        !sim.host_events
            .iter()
            .any(|e| matches!(e, Event::DataLost { .. }))
    );
}

#[test]
fn a_retransmit_request_the_sender_cannot_serve_reports_the_data_lost() {
    // Only the last command is kept; commands 1 and 2 are gone.
    let mut sim = lossy_sim(
        no_fec()
            .with_retransmit(Retransmit::Buffer(std::num::NonZeroUsize::MIN))
            .with_idle_interval(ms(300)),
        vec![2, 3],
    );
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(ms(10));
    assert!(
        sim.commands(Direction::ClientToHost)
            .iter()
            .any(|c| matches!(
                c,
                Command::RetransmitError(e) if e.first == SequenceNumber::new(3)
            ))
    );
    let kinds: Vec<_> = sim
        .host_events
        .iter()
        .filter(|e| matches!(e, Event::Ump { .. } | Event::DataLost { .. }))
        .cloned()
        .collect();
    assert_eq!(
        kinds,
        vec![
            Event::Ump {
                peer: client_addr(),
                packet: notes[0]
            },
            Event::DataLost {
                peer: client_addr(),
                first: SequenceNumber::new(1),
                count: 2
            },
            Event::Ump {
                peer: client_addr(),
                packet: notes[3]
            },
        ]
    );
}

#[test]
fn a_peer_without_retransmit_naks_the_request_and_the_gap_is_given_up() {
    let mut sim = lossy_sim(no_fec().with_retransmit(Retransmit::Unsupported), vec![2]);
    let notes: Vec<_> = (60..63).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(ms(30));
    assert!(
        sim.commands(Direction::ClientToHost)
            .iter()
            .any(|c| matches!(
                c,
                Command::Nak(n) if n.reason == NakReason::NotSupported && n.refused.code() == 0x80
            ))
    );
    assert!(sim.host_events.contains(&Event::DataLost {
        peer: client_addr(),
        first: SequenceNumber::new(1),
        count: 1
    }));
    assert_eq!(Sim::umps(&sim.host_events), vec![notes[0], notes[2]]);
    // No second request after the refusal.
    let requests = sim
        .commands(Direction::HostToClient)
        .iter()
        .filter(|c| matches!(c, Command::RetransmitRequest(_)))
        .count();
    assert_eq!(requests, 1);
}

#[test]
fn a_gap_that_is_never_filled_is_given_up_after_the_requests_are_spent() {
    // Every retransmission is lost as well.
    let mut sim = lossy_sim(no_fec(), std::iter::once(2).chain(4..60).collect());
    let notes: Vec<_> = (60..63).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    sim.advance(Duration::from_secs(2));
    assert!(sim.host_events.contains(&Event::DataLost {
        peer: client_addr(),
        first: SequenceNumber::new(1),
        count: 1
    }));
    assert_eq!(Sim::umps(&sim.host_events), vec![notes[0], notes[2]]);
}

#[test]
fn duplicated_datagrams_deliver_each_ump_once() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::ClientToHost {
            Fate::Twice
        } else {
            Fate::Deliver
        }
    });
    let notes: Vec<_> = (60..64).map(note_on).collect();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        sim.flush();
    }
    assert_eq!(Sim::umps(&sim.host_events), notes);
}

#[test]
fn commands_arriving_out_of_order_are_delivered_in_order() {
    let mut sim = Sim::new(
        HostConfig::new(identity("Studio", "HOST-1")),
        Credentials::none(),
        no_fec(),
    );
    sim.connect();
    sim.host_events.clear();
    let notes: Vec<_> = (60..63).map(note_on).collect();
    let mut datagrams = Vec::new();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        datagrams.push(sim.client.poll_transmit().unwrap());
    }
    for i in [2, 0, 1] {
        sim.host
            .handle_datagram(sim.now, client_addr(), &datagrams[i].payload);
    }
    sim.flush();
    assert_eq!(Sim::umps(&sim.host_events), notes);
}

#[test]
fn sequence_numbers_wrap_through_ffff_without_losing_or_repeating_a_command() {
    let mut sim = established();
    let total = 65_540usize;
    for i in 0..total {
        sim.client
            .send(sim.now, &[note_on(u8::try_from(i % 128).unwrap())])
            .unwrap();
        sim.flush();
    }
    let umps = Sim::umps(&sim.host_events);
    assert_eq!(umps.len(), total);
    assert!(
        umps.iter()
            .enumerate()
            .all(|(i, u)| *u == note_on(u8::try_from(i % 128).unwrap()))
    );
    let (_, last) = sim.log.last().unwrap();
    let last_seq = tutti_midi_network::parse(last)
        .unwrap()
        .filter_map(|c| match c.unwrap() {
            Command::UmpData(d) => Some(d.sequence()),
            _ => None,
        })
        .last()
        .unwrap();
    assert_eq!(last_seq, SequenceNumber::new(3));
}

#[test]
fn a_burst_larger_than_one_command_is_split_and_no_datagram_exceeds_1400_bytes() {
    let mut sim = established();
    let packets: Vec<_> = (0..100)
        .map(|i| tutti_ump::Ump([0x5000_0000 | i, 1, 2, 3]))
        .collect();
    sim.client.send(sim.now, &packets).unwrap();
    sim.flush();
    assert_eq!(Sim::umps(&sim.host_events), packets);
    assert!(sim.log.iter().all(|(_, b)| b.len() <= 1400));
    assert!(sim.log.len() >= 2, "the burst took more than one datagram");
    assert!(
        data_commands(&sim, Direction::ClientToHost)
            .iter()
            .all(|d| d.words().len() <= 64)
    );
}

#[test]
fn sending_before_a_session_is_established_fails_with_the_state() {
    let mut sim = Sim::open();
    assert_eq!(
        sim.client.send(sim.now, &[note_on(60)]),
        Err(SendError::NotEstablished {
            state: SessionState::Idle
        })
    );
    assert_eq!(
        sim.host.send(sim.now, client_addr(), &[note_on(60)]),
        Err(SendError::NotEstablished {
            state: SessionState::Idle
        })
    );
}

#[test]
fn the_host_sends_to_the_client_too() {
    let mut sim = established();
    sim.host
        .send(sim.now, client_addr(), &[note_on(72)])
        .unwrap();
    sim.flush();
    assert_eq!(Sim::umps(&sim.client_events), vec![note_on(72)]);
}

// ---- Idle periods ----

fn zero_length(sim: &Sim) -> usize {
    data_commands(sim, Direction::ClientToHost)
        .iter()
        .filter(|d| d.is_idle())
        .map(|d| d.sequence())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

#[test]
fn an_idle_period_is_declared_with_zero_length_commands_that_use_sequence_numbers() {
    let mut sim = established();
    sim.client.send(sim.now, &[note_on(60)]).unwrap();
    sim.flush();
    assert_eq!(zero_length(&sim), 0);
    sim.advance(ms(10));
    assert_eq!(zero_length(&sim), 1);
    // The two FEC repeats follow at the same interval.
    sim.advance(ms(20));
    assert_eq!(zero_length(&sim), 3);
    let idle: Vec<_> = data_commands(&sim, Direction::ClientToHost)
        .iter()
        .filter(|d| d.is_idle())
        .map(|d| d.sequence().get())
        .collect();
    assert!(
        idle.starts_with(&[1, 1]) || idle.contains(&1),
        "zero-length numbers continue the sequence: {idle:?}"
    );
    assert!(
        sim.host_events
            .iter()
            .all(|e| !matches!(e, Event::Ump { packet, .. } if *packet != note_on(60)))
    );
}

#[test]
fn zero_length_commands_slow_down_and_then_stop() {
    let mut sim = established();
    sim.client.send(sim.now, &[note_on(60)]).unwrap();
    sim.flush();
    sim.advance(Duration::from_secs(10));
    let after_ten = zero_length(&sim);
    assert!(after_ten > 3 && after_ten < 20, "{after_ten}");
    sim.advance(Duration::from_secs(10));
    assert_eq!(
        zero_length(&sim),
        after_ten,
        "no more once the interval is long"
    );
}

#[test]
fn the_first_zero_length_command_comes_within_300_ms_whatever_is_configured() {
    let mut sim = Sim::new(
        HostConfig::new(identity("Studio", "HOST-1")),
        Credentials::none(),
        SessionConfig::default().with_idle_interval(Duration::from_secs(5)),
    );
    sim.connect();
    sim.log.clear();
    sim.client.send(sim.now, &[note_on(60)]).unwrap();
    sim.flush();
    sim.advance(ms(299));
    assert_eq!(zero_length(&sim), 0);
    sim.advance(ms(1));
    assert_eq!(zero_length(&sim), 1);
}

// ---- Keep-alive ----

#[test]
fn quiet_peers_ping_each_other_and_keep_the_session_alive() {
    let mut sim = established();
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.client.state(), SessionState::Established);
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
    let pings: Vec<PingId> = sim
        .commands(Direction::ClientToHost)
        .into_iter()
        .filter_map(|c| match c {
            Command::Ping(id) => Some(id),
            _ => None,
        })
        .collect();
    assert!(pings.len() >= 5, "{pings:?}");
    let replies: Vec<PingId> = sim
        .commands(Direction::HostToClient)
        .into_iter()
        .filter_map(|c| match c {
            Command::PingReply(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        &replies[..pings.len().min(replies.len())],
        &pings[..pings.len().min(replies.len())]
    );
}

#[test]
fn a_ping_gets_a_reply_with_the_same_id_in_any_state() {
    let mut sim = Sim::open();
    sim.host.handle_datagram(
        sim.now,
        client_addr(),
        &datagram(&Command::Ping(PingId(0xCAFE))),
    );
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::PingReply(PingId(0xCAFE))]
    );
}

#[test]
fn a_ping_reply_nobody_asked_for_gets_a_bad_ping_reply_nak() {
    let mut sim = established();
    sim.host.handle_datagram(
        sim.now,
        client_addr(),
        &datagram(&Command::PingReply(PingId(77))),
    );
    sim.flush();
    assert!(
        sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(
                c,
                Command::Nak(n) if n.reason == NakReason::BadPingReply
            ))
    );
}

#[test]
fn a_silent_peer_ends_the_session_with_a_timeout_bye_that_is_repeated() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::HostToClient {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.advance(Duration::from_secs(20));
    let byes = sim
        .commands(Direction::ClientToHost)
        .into_iter()
        .filter(|c| *c == Command::Bye(Bye::new(ByeReason::Timeout)))
        .count();
    assert!(
        byes >= 2,
        "the Bye is repeated until answered or given up: {byes}"
    );
    assert!(sim.host_events.contains(&Event::Closed {
        peer: client_addr(),
        initiator: Initiator::Remote,
        reason: ByeReason::Timeout
    }));
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        initiator: Initiator::Local,
        reason: ByeReason::Timeout
    }));
    assert_eq!(sim.client.state(), SessionState::Idle);
}

// ---- Bye ----

#[test]
fn a_bye_is_answered_once_and_both_ends_return_to_idle() {
    let mut sim = established();
    sim.client.close(sim.now, ByeReason::UserTerminated);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Idle);
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
    assert!(sim.host_events.contains(&Event::Closed {
        peer: client_addr(),
        initiator: Initiator::Remote,
        reason: ByeReason::UserTerminated
    }));
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        initiator: Initiator::Local,
        reason: ByeReason::UserTerminated
    }));
    let replies = sim
        .commands(Direction::HostToClient)
        .iter()
        .filter(|c| **c == Command::ByeReply)
        .count();
    assert_eq!(replies, 1);
    assert_eq!(
        sim.client.send(sim.now, &[note_on(1)]),
        Err(SendError::NotEstablished {
            state: SessionState::Idle
        })
    );
}

#[test]
fn a_bye_from_a_peer_in_no_session_is_still_answered() {
    let mut sim = Sim::open();
    sim.host.handle_datagram(
        sim.now,
        client_addr(),
        &datagram(&Command::Bye(Bye::new(ByeReason::UserTerminated))),
    );
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::ByeReply]
    );
    assert_eq!(sim.host.state(client_addr()), SessionState::Idle);
}

#[test]
fn a_host_that_powers_down_ends_every_session() {
    let mut sim = established();
    sim.host.close_all(sim.now);
    sim.flush();
    assert!(sim.client_events.contains(&Event::Closed {
        peer: host_addr(),
        initiator: Initiator::Remote,
        reason: ByeReason::PowerDown
    }));
}

#[test]
fn data_outside_a_session_gets_a_bye_for_session_not_established() {
    let mut sim = Sim::open();
    let data = Command::UmpData(UmpData::new(SequenceNumber::ZERO, &[note_on(60)]).unwrap());
    sim.host
        .handle_datagram(sim.now, client_addr(), &datagram(&data));
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Bye::new(ByeReason::SessionNotEstablished))]
    );
    assert!(Sim::umps(&sim.host_events).is_empty());
}

// ---- Session reset ----

#[test]
fn a_reset_zeroes_both_sequence_numbers_and_flushes_the_buffers() {
    let mut sim = established();
    for n in 60..63 {
        sim.client.send(sim.now, &[note_on(n)]).unwrap();
        sim.flush();
    }
    sim.client.reset(sim.now);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Established);
    assert!(
        sim.client_events
            .contains(&Event::Reset { peer: host_addr() })
    );
    assert!(sim.host_events.contains(&Event::Reset {
        peer: client_addr()
    }));
    sim.log.clear();
    sim.host_events.clear();
    sim.client.send(sim.now, &[note_on(100)]).unwrap();
    sim.flush();
    let sent = data_commands(&sim, Direction::ClientToHost);
    assert_eq!(
        sent.len(),
        1,
        "the flushed buffer leaves no FEC repeat to send"
    );
    assert_eq!(sent[0].sequence(), SequenceNumber::ZERO);
    assert_eq!(Sim::umps(&sim.host_events), vec![note_on(100)]);
}

#[test]
fn no_data_is_sent_while_a_reset_is_pending_and_the_reset_is_repeated() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::HostToClient {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.client.reset(sim.now);
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::PendingSessionReset);
    assert_eq!(
        sim.client.send(sim.now, &[note_on(1)]),
        Err(SendError::NotEstablished {
            state: SessionState::PendingSessionReset
        })
    );
    sim.advance(ms(1200));
    let resets = sim
        .commands(Direction::ClientToHost)
        .iter()
        .filter(|c| **c == Command::SessionReset)
        .count();
    assert!(resets >= 3, "{resets}");
}

#[test]
fn an_unanswered_reset_ends_the_session_with_a_timeout_bye() {
    let mut sim = established();
    sim.set_fate(|dir, _| {
        if dir == Direction::HostToClient {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    });
    sim.client.reset(sim.now);
    sim.advance(Duration::from_secs(6));
    assert!(
        sim.commands(Direction::ClientToHost)
            .contains(&Command::Bye(Bye::new(ByeReason::Timeout)))
    );
}

#[test]
fn a_reset_outside_a_session_gets_a_bye_for_session_not_established() {
    let mut sim = Sim::open();
    sim.host
        .handle_datagram(sim.now, client_addr(), &datagram(&Command::SessionReset));
    sim.flush();
    assert_eq!(
        sim.commands(Direction::HostToClient),
        vec![Command::Bye(Bye::new(ByeReason::SessionNotEstablished))]
    );
}

// ---- Unreadable and unknown commands ----

#[test]
fn an_unknown_command_gets_a_nak_for_command_not_supported() {
    let mut sim = established();
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x55, 0x00, 0x00, 0x00]);
    sim.host.handle_datagram(sim.now, client_addr(), &bytes);
    sim.flush();
    let [Command::Nak(nak)] = &sim.commands(Direction::HostToClient)[..] else {
        panic!()
    };
    assert_eq!(nak.reason, NakReason::NotSupported);
    assert_eq!(nak.refused.word(), 0x5500_0000);
    assert_eq!(
        sim.host.state(client_addr()),
        SessionState::Established,
        "a NAK ends no session"
    );
}

#[test]
fn a_malformed_command_gets_a_nak_for_command_malformed() {
    let mut sim = established();
    let mut bytes = b"MIDI".to_vec();
    bytes.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
    sim.host.handle_datagram(sim.now, client_addr(), &bytes);
    sim.flush();
    assert!(matches!(
        &sim.commands(Direction::HostToClient)[..],
        [Command::Nak(n)] if n.reason == NakReason::Malformed
    ));
}

#[test]
fn a_datagram_without_the_signature_changes_nothing() {
    let mut sim = established();
    sim.host
        .handle_datagram(sim.now, client_addr(), b"XXXX\xF0\x00\x01\x00");
    sim.flush();
    assert!(sim.log.is_empty());
    assert_eq!(sim.host.state(client_addr()), SessionState::Established);
}

#[test]
fn a_client_ignores_datagrams_from_any_address_but_its_host() {
    let mut sim = established();
    let stranger = "10.0.0.77:1".parse().unwrap();
    sim.client.handle_datagram(
        sim.now,
        stranger,
        &datagram(&Command::Bye(Bye::new(ByeReason::UserTerminated))),
    );
    sim.flush();
    assert_eq!(sim.client.state(), SessionState::Established);
}

#[test]
fn a_stranger_that_is_not_inviting_is_answered_statelessly_and_costs_the_host_no_entry() {
    let mut sim = Sim::open();
    let stranger = "10.0.0.50:9000".parse().unwrap();
    let replies = |sim: &mut Sim, bytes: &[u8]| {
        sim.host.handle_datagram(sim.now, stranger, bytes);
        let mut out = Vec::new();
        while let Some(t) = sim.host.poll_transmit() {
            assert_eq!(t.to, stranger);
            out.extend(
                tutti_midi_network::parse(&t.payload)
                    .unwrap()
                    .map(Result::unwrap),
            );
        }
        out
    };
    let data = Command::UmpData(UmpData::new(SequenceNumber::ZERO, &[note_on(60)]).unwrap());
    assert_eq!(
        replies(&mut sim, &datagram(&data)),
        vec![Command::Bye(Bye::new(ByeReason::SessionNotEstablished))]
    );
    assert_eq!(sim.host.session_count(), 0);
    assert_eq!(
        replies(&mut sim, &datagram(&Command::Ping(PingId(5)))),
        vec![Command::PingReply(PingId(5))]
    );
    assert_eq!(sim.host.session_count(), 0);
    assert_eq!(
        replies(&mut sim, &datagram(&Command::SessionReset)),
        vec![Command::Bye(Bye::new(ByeReason::SessionNotEstablished))]
    );
    assert_eq!(sim.host.session_count(), 0);
    assert_eq!(sim.host.state(stranger), SessionState::Idle);
}

#[test]
fn only_an_invitation_below_capacity_creates_a_host_entry() {
    let mut sim = Sim::open();
    sim.connect();
    assert_eq!(sim.host.session_count(), 1);
}

#[test]
fn a_receiver_that_never_requests_gives_a_gap_up_without_asking() {
    let mut host = HostConfig::new(identity("Studio", "HOST-1"));
    host.session = SessionConfig::default().with_requests(Requests::Never);
    let mut sim = Sim::new(host, Credentials::none(), no_fec());
    sim.connect();
    sim.host_events.clear();
    let notes: Vec<_> = (60..63).map(note_on).collect();
    let mut datagrams = Vec::new();
    for n in &notes {
        sim.client.send(sim.now, &[*n]).unwrap();
        datagrams.push(sim.client.poll_transmit().unwrap());
    }
    for i in [0, 2] {
        sim.host
            .handle_datagram(sim.now, client_addr(), &datagrams[i].payload);
    }
    sim.advance(ms(10));
    assert!(sim.host_events.contains(&Event::DataLost {
        peer: client_addr(),
        first: SequenceNumber::new(1),
        count: 1
    }));
    assert!(
        !sim.commands(Direction::HostToClient)
            .iter()
            .any(|c| matches!(c, Command::RetransmitRequest(_)))
    );
}
