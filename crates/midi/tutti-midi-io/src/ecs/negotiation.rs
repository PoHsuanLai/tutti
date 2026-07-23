//! ECS wrapping for MIDI-CI (M2-101) and UMP-Stream endpoint discovery.
//!
//! This is the **outbound + resource** half of the two negotiation state
//! machines that live in `tutti-midi-runtime`. It follows the same shape as
//! [`super::device`]: a resource holding the pure state machine, fire-and-forget
//! request [`Message`]s, and systems that drive the machine and push its output
//! onto the [`MidiBusRes`](super::bus::MidiBusRes) as system events.
//!
//! Two duties, one module because they share the pattern:
//!
//! - **MIDI-CI** — [`CiRes`] holds a [`CiResponder`] (this device's declared
//!   identity/profiles/properties) and a [`CiInitiator`] (the inquiring side).
//!   [`StartCiDiscovery`] broadcasts a Discovery probe; [`InboundCiMessage`]
//!   feeds a decoded reply into both halves, emitting any responses back onto the
//!   bus and raising [`CiDeviceDiscovered`] when a peer's identity arrives.
//! - **UMP-Stream** — [`EndpointDiscoveryRes`] holds an [`EndpointInquiry`];
//!   [`StartEndpointDiscovery`] sends the Endpoint Discovery probe;
//!   [`InboundEndpointReply`] feeds reply events in, raising [`EndpointDiscovered`]
//!   once the endpoint assembles.
//!
//! **Inbound is a stub seam.** Nothing decodes live SysEx off the hardware input
//! yet (`core/hardware/input.rs` drops SysEx before it reaches the UMP stream),
//! so [`InboundCiMessage`] / [`InboundEndpointReply`] have no producer in this
//! crate today — they're the injection point a future inbound-decode pass (or a
//! test / loopback) writes into. The negotiators themselves are fully exercised
//! by the runtime crate's loopback tests; this layer only wires their *outbound*
//! path to the bus and surfaces their *results* as ECS messages.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::tutti_midi_types::ci::{CiMessage, DiscoveryData, Muid};
use tutti_midi_runtime::tutti_midi_types::EndpointDiscoveryRequest;
use tutti_midi_runtime::{CiInitiator, CiResponder, DiscoveredCiDevice};
use tutti_midi_runtime::{DiscoveredEndpoint, EndpointInquiry};

use super::bus::MidiBusRes;

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

/// Broadcast a Discovery probe when [`StartCiDiscovery`] is requested.
pub fn ci_discovery_system(
    ci: Option<Res<CiRes>>,
    bus: Option<Res<MidiBusRes>>,
    mut requests: MessageReader<StartCiDiscovery>,
) {
    let (Some(ci), Some(bus)) = (ci, bus) else {
        requests.clear();
        return;
    };
    for _ in requests.read() {
        bus.broadcast_ci(CI_GROUP, &ci.initiator.discovery());
    }
}

/// Feed decoded inbound CI messages into both negotiator halves: the responder
/// emits replies (queued back onto the bus), and the initiator records
/// discovered peers (raising [`CiDeviceDiscovered`]) and any collision
/// Invalidate it must send.
pub fn ci_ingest_system(
    ci: Option<ResMut<CiRes>>,
    bus: Option<Res<MidiBusRes>>,
    mut inbound: MessageReader<InboundCiMessage>,
    mut discovered: MessageWriter<CiDeviceDiscovered>,
) {
    let (Some(mut ci), Some(bus)) = (ci, bus) else {
        inbound.clear();
        return;
    };
    for InboundCiMessage(message) in inbound.read() {
        // Responder: any replies this message warrants go straight back out.
        for reply in ci.responder.respond_to(message) {
            bus.broadcast_ci(CI_GROUP, &reply);
        }
        // Initiator: record what we learn; emit any collision Invalidate it asks for.
        let before = ci.initiator.discovered().cloned();
        for feedback in ci.initiator.ingest(message) {
            bus.broadcast_ci(CI_GROUP, &feedback);
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

/// Broadcast an Endpoint Discovery probe when [`StartEndpointDiscovery`] fires.
pub fn endpoint_discovery_system(
    bus: Option<Res<MidiBusRes>>,
    mut requests: MessageReader<StartEndpointDiscovery>,
) {
    let Some(bus) = bus else {
        requests.clear();
        return;
    };
    for _ in requests.read() {
        bus.request_endpoint_discovery(EndpointDiscoveryRequest::all());
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
                .run_if(tutti_core::graph::engine_ready),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_runtime::tutti_midi_types::ci::CiCategories;
    use tutti_midi_runtime::MidiBus;

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
        world.insert_resource(MidiBusRes(MidiBus::new()));
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
}
