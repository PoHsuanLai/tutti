//! ECS wrapping for MIDI-CI (M2-101) and UMP-Stream endpoint discovery.
//!
//! This is the **outbound + resource** half of the two negotiation state
//! machines that live in `tutti-midi-runtime`. It follows the same shape as
//! [`super::device`]: a resource holding the pure state machine, fire-and-forget
//! request [`Message`]s, and systems that drive the machine and push its output
//! at external MIDI out via [`MidiOutRes`](super::track_out::MidiOutRes).
//!
//! Both protocols negotiate with *peer devices*, so their output goes to the
//! hardware-out mailbox, never the `MidiBus` synth fan-out — a synth inbox is
//! not a wire, and nothing would carry these to a peer.
//!
//! Two duties, one module because they share the pattern:
//!
//! - **MIDI-CI** — [`CiRes`] holds a [`CiResponder`] (this device's declared
//!   identity/profiles/properties) and a [`CiInitiator`] (the inquiring side).
//!   [`StartCiDiscovery`] sends a Discovery probe; [`InboundCiMessage`]
//!   feeds a decoded reply into both halves, emitting any responses back out
//!   and raising [`CiDeviceDiscovered`] when a peer's identity arrives.
//! - **UMP-Stream** — [`EndpointDiscoveryRes`] holds an [`EndpointInquiry`];
//!   [`StartEndpointDiscovery`] sends the Endpoint Discovery probe;
//!   [`InboundEndpointReply`] feeds reply events in, raising [`EndpointDiscovered`]
//!   once the endpoint assembles.
//!
//! **Inbound arrives from the app layer, not from here.**
//! [`InboundCiMessage`] / [`InboundEndpointReply`] have no producer *in this
//! crate*; the live one is the app's hardware drain (dawai's
//! `input::midi::hardware_route`), which classifies by UMP message type and
//! feeds these. The chain behind it is real: `core/hardware/input.rs`
//! reassembles MIDI-1.0 SysEx across driver callbacks and promotes it to UMP
//! SysEx7, which a `Sysex7Reassembler` + `ci::sysex7_to_ci` turn back into a
//! typed [`CiMessage`]. That works because MIDI-CI is Universal SysEx by design
//! (M2-101) — it has to survive a MIDI-1.0 transport, since it's how two devices
//! discover each other *before* either knows the other speaks MIDI 2.0.
//! `midi1_wire_sysex_promotes_to_a_typed_ci_message` covers that seam.
//!
//! **UMP-Stream needs a native-UMP transport.** That family has no MIDI-1.0
//! encoding, so it cannot arrive over midir (a MIDI-1.0 API) at all. On macOS
//! [`UmpVirtualDestination`](tutti_midi_io::UmpVirtualDestination) provides the
//! MIDI-2.0-protocol endpoint it needs — point it at the same input ring with
//! `with_producer` and UMP-Stream messages join the ordinary inbound stream,
//! reaching the `UmpStream` arm of the app's drain. (Its outbound counterpart is
//! `UmpVirtualSource`.) On other platforms the arm stays dormant until an
//! equivalent endpoint exists.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::tutti_midi_types::ci::{CiMessage, DiscoveryData, Muid};
use tutti_midi_runtime::tutti_midi_types::EndpointDiscoveryRequest;
use tutti_midi_runtime::{CiInitiator, CiResponder, DiscoveredCiDevice};
use tutti_midi_runtime::{DiscoveredEndpoint, EndpointInquiry};

/// The default group MIDI-CI negotiation runs on (function-block-wide).
const CI_GROUP: u8 = 0;

// ---------------------------------------------------------------------------
// MIDI-CI
// ---------------------------------------------------------------------------

/// The MIDI-CI negotiation state — this device's responder identity plus the
/// initiator that probes peers. Built from a [`DiscoveryData`] identity and a
/// seed MUID (the engine forbids `rand`/`Date::now`, so the seed is supplied by
/// the caller — e.g. a hash of the device name).
#[derive(Resource)]
pub struct CiRes {
    /// The answering side: replies to inbound Discovery / Profile / Property.
    pub responder: CiResponder,
    /// The inquiring side: probes peers and records what it discovers.
    pub initiator: CiInitiator,
}

impl CiRes {
    /// Build the CI state from this device's identity, seeding both MUIDs from
    /// `muid_seed` (initiator and responder get distinct MUIDs so a self-probe on
    /// a shared bus doesn't read as a collision).
    pub fn new(identity: DiscoveryData, muid_seed: u32) -> Self {
        Self {
            responder: CiResponder::new(Muid::from_seed(muid_seed), identity),
            initiator: CiInitiator::new(Muid::from_seed(muid_seed.wrapping_add(1)), identity),
        }
    }
}

/// Fire-and-forget request: broadcast a MIDI-CI Discovery probe on the bus.
#[derive(Message, Debug, Clone, Default)]
pub struct StartCiDiscovery;

/// A decoded inbound MIDI-CI message to feed into the negotiators.
///
/// No producer in this crate yet — the inbound-decode seam. A future pass that
/// reassembles SysEx7 off the hardware input (or a loopback test) writes this;
/// [`ci_ingest_system`] then drives both negotiator halves with it.
#[derive(Message, Debug, Clone)]
pub struct InboundCiMessage(pub CiMessage);

/// Raised when the initiator learns a peer's identity from its Discovery Reply.
#[derive(Message, Debug, Clone)]
pub struct CiDeviceDiscovered(pub DiscoveredCiDevice);

/// Send a Discovery probe to external MIDI out when [`StartCiDiscovery`] fires.
///
/// MIDI-CI negotiates with *peer devices*, so the destination is the hardware-out
/// mailbox rather than the synth fan-out bus.
pub fn ci_discovery_system(
    ci: Option<Res<CiRes>>,
    out: Option<Res<super::track_out::MidiOutRes>>,
    mut requests: MessageReader<StartCiDiscovery>,
) {
    let (Some(ci), Some(out)) = (ci, out) else {
        requests.clear();
        return;
    };
    let sender = out.sender();
    for _ in requests.read() {
        send_ci(&sender, &ci.initiator.discovery());
    }
}

/// Fragment a CI message into SysEx7 UMP packets and push them at the
/// hardware-out mailbox.
///
/// The encoding is `ci_to_sysex7`'s, which is the only one — an earlier version
/// of this comment compared it to `MidiBus::broadcast_ci`, a method that does
/// not exist.
fn send_ci(sender: &tutti_midi_runtime::MidiSender, message: &CiMessage) {
    let mut packets = Vec::new();
    tutti_midi_runtime::tutti_midi_types::ci::ci_to_sysex7(CI_GROUP, message, &mut packets);
    sender.queue(&packets);
}

/// Feed decoded inbound CI messages into both negotiator halves: the responder
/// emits replies (queued back onto the bus), and the initiator records
/// discovered peers (raising [`CiDeviceDiscovered`]) and any collision
/// Invalidate it must send.
pub fn ci_ingest_system(
    ci: Option<ResMut<CiRes>>,
    out: Option<Res<super::track_out::MidiOutRes>>,
    mut inbound: MessageReader<InboundCiMessage>,
    mut discovered: MessageWriter<CiDeviceDiscovered>,
) {
    let (Some(mut ci), Some(out)) = (ci, out) else {
        inbound.clear();
        return;
    };
    let sender = out.sender();
    for InboundCiMessage(message) in inbound.read() {
        // Responder: any replies this message warrants go straight back out.
        for reply in ci.responder.respond_to(message) {
            send_ci(&sender, &reply);
        }
        // Initiator: record what we learn; emit any collision Invalidate it asks for.
        let before = ci.initiator.discovered().cloned();
        for feedback in ci.initiator.ingest(message) {
            send_ci(&sender, &feedback);
        }
        // A newly discovered peer (identity we didn't have before) → surface it.
        if let Some(peer) = ci.initiator.discovered() {
            if before.as_ref() != Some(peer) {
                discovered.write(CiDeviceDiscovered(peer.clone()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UMP-Stream endpoint discovery
// ---------------------------------------------------------------------------

/// The UMP-Stream discoverer state — the inquiring half of endpoint negotiation.
#[derive(Resource, Default)]
pub struct EndpointDiscoveryRes {
    pub inquiry: EndpointInquiry,
}

/// Fire-and-forget request: broadcast a UMP-Stream Endpoint Discovery probe
/// (asking for everything).
#[derive(Message, Debug, Clone, Default)]
pub struct StartEndpointDiscovery;

/// A decoded inbound UMP-Stream reply event to feed into the discoverer.
///
/// Like [`InboundCiMessage`], the inbound-decode seam — no producer in this crate
/// yet (the hardware path doesn't surface Stream messages).
#[derive(Message, Debug, Clone)]
pub struct InboundEndpointReply(pub tutti_midi_runtime::tutti_midi_types::ump::MidiEvent);

/// Raised once the discoverer assembles a peer endpoint (Endpoint Info seen).
#[derive(Message, Debug, Clone)]
pub struct EndpointDiscovered(pub DiscoveredEndpoint);

/// Send an Endpoint Discovery probe to external MIDI out when
/// [`StartEndpointDiscovery`] fires. Like MIDI-CI, this asks a *peer endpoint*
/// about itself, so it goes to the hardware-out mailbox.
pub fn endpoint_discovery_system(
    out: Option<Res<super::track_out::MidiOutRes>>,
    mut requests: MessageReader<StartEndpointDiscovery>,
) {
    let Some(out) = out else {
        requests.clear();
        return;
    };
    let sender = out.sender();
    for _ in requests.read() {
        sender.queue(&[
            tutti_midi_runtime::tutti_midi_types::ump::MidiEvent::endpoint_discovery(
                1,
                1,
                EndpointDiscoveryRequest::all(),
            ),
        ]);
    }
}

/// Feed inbound reply events into the discoverer; raise [`EndpointDiscovered`]
/// the first frame a complete endpoint assembles.
pub fn endpoint_ingest_system(
    mut endpoint: ResMut<EndpointDiscoveryRes>,
    mut inbound: MessageReader<InboundEndpointReply>,
    mut discovered: MessageWriter<EndpointDiscovered>,
) {
    for InboundEndpointReply(event) in inbound.read() {
        let had = endpoint.inquiry.result().is_some();
        endpoint.inquiry.ingest(event);
        if !had {
            if let Some(result) = endpoint.inquiry.result() {
                discovered.write(EndpointDiscovered(result.clone()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Wires MIDI-CI and UMP-Stream endpoint discovery into the ECS.
///
/// Registers the request/result [`Message`]s and their driving systems. It does
/// **not** insert [`CiRes`] — that needs a device identity + MUID seed the app
/// supplies (`app.insert_resource(CiRes::new(identity, seed))`); the CI systems
/// early-out until it's present. [`EndpointDiscoveryRes`] is defaulted in
/// (stateless until a discovery starts).
pub struct MidiNegotiationPlugin;

impl Plugin for MidiNegotiationPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<StartCiDiscovery>();
        app.add_message::<InboundCiMessage>();
        app.add_message::<CiDeviceDiscovered>();
        app.add_message::<StartEndpointDiscovery>();
        app.add_message::<InboundEndpointReply>();
        app.add_message::<EndpointDiscovered>();

        app.init_resource::<EndpointDiscoveryRes>();

        app.add_systems(
            Update,
            (
                ci_discovery_system,
                ci_ingest_system,
                endpoint_discovery_system,
                endpoint_ingest_system,
            )
                .run_if(crate::graph::engine_ready)
                // Push into the outbound mailbox before the pump drains it, so a
                // probe requested this frame reaches the wire this frame rather
                // than waiting one.
                .before(super::track_out::pump_midi_out_system),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::track_out::MidiOutRes;
    use super::*;
    use tutti_midi_runtime::tutti_midi_types::ci::CiCategories;

    fn identity(mfr: [u8; 3]) -> DiscoveryData {
        DiscoveryData {
            manufacturer: mfr,
            family: 0x1234,
            family_model: 0x0055,
            software_revision: [1, 0, 0, 0],
            categories: CiCategories::PROFILE_CONFIGURATION,
            max_sysex_size: 512,
        }
    }

    /// Drive `ci_ingest_system` with a peer's Discovery Reply and assert it
    /// surfaces a `CiDeviceDiscovered` — proving the ECS seam feeds the
    /// initiator, not that the wiring is dead code.
    #[test]
    fn inbound_reply_surfaces_discovered_device() {
        let mut world = World::new();
        world.init_resource::<MidiOutRes>();
        world.insert_resource(CiRes::new(identity([0x11, 0x22, 0x33]), 0xABCD));
        world.init_resource::<Messages<InboundCiMessage>>();
        world.init_resource::<Messages<CiDeviceDiscovered>>();

        // A peer responds to our initiator's Discovery with its own reply.
        let peer = CiResponder::new(Muid::from_seed(0x5EED), identity([0x00, 0x21, 0x09]));
        let our_probe = world.resource::<CiRes>().initiator.discovery();
        let reply = peer.respond_to(&our_probe).remove(0);
        world
            .resource_mut::<Messages<InboundCiMessage>>()
            .write(InboundCiMessage(reply));

        let mut schedule = Schedule::default();
        schedule.add_systems(ci_ingest_system);
        schedule.run(&mut world);

        // The initiator recorded the peer, and the ECS message went out.
        let discovered = world.resource::<CiRes>().initiator.discovered().cloned();
        assert_eq!(
            discovered.map(|d| d.identity.manufacturer),
            Some([0x00, 0x21, 0x09])
        );
        let events = world.resource::<Messages<CiDeviceDiscovered>>();
        let mut cursor = events.get_cursor();
        assert_eq!(cursor.read(events).count(), 1, "one device surfaced");
    }

    /// A Discovery probe must land in the mailbox the hardware pump drains.
    ///
    /// Regression: these broadcasts used to go to `MidiBus`'s per-unit *system*
    /// ring, which nothing ever polled — so every CI probe, endpoint-discovery
    /// request, and Flex-metadata message was silently dropped. Loopback tests
    /// of the negotiators couldn't catch it; only checking the destination can.
    #[test]
    fn discovery_probe_reaches_the_hardware_out_mailbox() {
        let mut world = World::new();
        world.init_resource::<MidiOutRes>();
        world.insert_resource(CiRes::new(identity([0x11, 0x22, 0x33]), 0xABCD));
        world.init_resource::<Messages<StartCiDiscovery>>();
        world
            .resource_mut::<Messages<StartCiDiscovery>>()
            .write(StartCiDiscovery);

        let mut schedule = Schedule::default();
        schedule.add_systems(ci_discovery_system);
        schedule.run(&mut world);

        // Drain the outbound mailbox the way `pump_midi_out_system` does.
        let out = world.resource::<MidiOutRes>();
        let (_, receiver) = (out.sender(), out.receiver_for_test());
        let mut buf = [tutti_midi_runtime::tutti_midi_types::ump::MidiEvent::noop(); 64];
        let n = receiver.poll_into(&mut buf);
        assert!(
            n > 0,
            "the Discovery probe must reach the hardware-out mailbox, not a ring nobody drains"
        );
    }
}
