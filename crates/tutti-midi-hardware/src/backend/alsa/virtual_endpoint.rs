//! Virtual endpoints on the sequencer: a client of this process with one port
//! other clients subscribe to.
//!
//! The client is a UMP client speaking the protocol the spec asks for, or MIDI
//! 1.0 where the kernel's sequencer has no MIDI 2.0 client mode; the protocol it
//! got is what the endpoint reports. It publishes a UMP Endpoint record (the
//! protocol, and the spec's Function Blocks), so an application enumerating it
//! reads the protocol it carries instead of taking it for a legacy port.

use tutti_alsa_ffi::{
    BLOCK_IS_LOWSPEED, BLOCK_IS_MIDI1, CAP_READ, CAP_SUBS_READ, CAP_SUBS_WRITE, CAP_WRITE, Client,
    DIR_BIDIRECTION, DIR_INPUT, DIR_OUTPUT, PROTO_MIDI1, PROTO_MIDI2, TYPE_APPLICATION,
    TYPE_MIDI_GENERIC, UmpBlock, UmpEndpoint, UmpVersion,
};
use tutti_midi_device::FunctionBlock;
use tutti_ump::Direction;

use super::input::{self, AlsaInput};
use crate::InputProducerHandle;
use crate::block::RawBlock;
use crate::error::{Error, Result};
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};

/// The call that fails when the kernel has no MIDI 2.0 client mode.
const SET_VERSION: &str = "snd_seq_set_client_midi_version";

/// The sequencer's version for a protocol.
fn version_of(protocol: VirtualProtocol) -> UmpVersion {
    match protocol {
        VirtualProtocol::Midi1 => UmpVersion::Midi1,
        VirtualProtocol::Midi2 => UmpVersion::Midi2,
    }
}

/// Opens the endpoint's client in the protocol the spec asks for, or in MIDI 1.0
/// when the kernel refuses a MIDI 2.0 client; the protocol it got goes with it.
fn open_client(spec: &VirtualSpec, nonblock: bool) -> Result<(Client, VirtualProtocol)> {
    let name = spec.name().as_str();
    match super::client::open(name, nonblock, version_of(spec.protocol())) {
        Ok(client) => Ok((client, spec.protocol())),
        Err(Error::Alsa {
            operation: SET_VERSION,
            code,
        }) if spec.protocol() == VirtualProtocol::Midi2 => {
            tracing::debug!("virtual MIDI endpoint falls back to MIDI 1.0 ({code})");
            Ok((
                super::client::open(name, nonblock, UmpVersion::Midi1)?,
                VirtualProtocol::Midi1,
            ))
        }
        Err(e) => Err(e),
    }
}

/// The sequencer's record of a Function Block.
fn ump_block(block: &FunctionBlock) -> UmpBlock {
    let raw = RawBlock::of(block);
    UmpBlock {
        id: raw.id,
        active: raw.active,
        flags: match raw.midi1 {
            0 => 0,
            1 => BLOCK_IS_MIDI1,
            _ => BLOCK_IS_MIDI1 | BLOCK_IS_LOWSPEED,
        },
        direction: match block.direction() {
            Direction::Input => DIR_INPUT,
            Direction::Output => DIR_OUTPUT,
            _ => DIR_BIDIRECTION,
        },
        first_group: raw.first_group,
        num_groups: raw.groups,
        midi_ci_version: raw.midi_ci_version,
        sysex8_streams: raw.sysex8_streams,
        ui_hint: raw.ui_hint,
        name: raw.name.map(str::to_owned),
    }
}

/// Publishes the endpoint record on `client`: the protocol it carries and the
/// spec's blocks.
fn publish(client: &Client, spec: &VirtualSpec, protocol: VirtualProtocol) -> Result<()> {
    let bits = match protocol {
        VirtualProtocol::Midi1 => PROTO_MIDI1,
        VirtualProtocol::Midi2 => PROTO_MIDI2,
    };
    client.publish_ump_endpoint(&UmpEndpoint {
        protocol: bits,
        protocol_caps: bits,
        name: Some(spec.name().as_str().to_owned()),
        blocks: spec.function_blocks().iter().map(ump_block).collect(),
    })?;
    Ok(())
}

/// A virtual source: the client's readable, subscribable port.
pub(crate) struct AlsaVirtualSource {
    // `Client` is `Send` and not `Sync`; sends are rare and short.
    inner: std::sync::Mutex<(Client, u8)>,
}

impl SourceBackend for AlsaVirtualSource {
    fn send(&self, words: &[u32]) -> Result<()> {
        // A poisoned lock guards a client handle and a port number, which a
        // panic cannot leave half-written.
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.0.send_to_subscribers(inner.1, words)?;
        Ok(())
    }
}

/// Publishes a virtual source for `spec`.
pub(crate) fn source(spec: &VirtualSpec) -> Result<(AlsaVirtualSource, VirtualProtocol)> {
    let (client, protocol) = open_client(spec, false)?;
    let port = client.open_port(
        spec.name().as_str(),
        CAP_READ | CAP_SUBS_READ,
        TYPE_MIDI_GENERIC | TYPE_APPLICATION,
    )?;
    publish(&client, spec, protocol)?;
    let port = super::output::seq_addr_byte(port, "snd_seq_create_simple_port")?;
    Ok((
        AlsaVirtualSource {
            inner: std::sync::Mutex::new((client, port)),
        },
        protocol,
    ))
}

/// Publishes a virtual destination for `spec`: the client's writable,
/// subscribable port, read by a pump into `producer`, each event stamped by the
/// kernel as it arrives.
pub(crate) fn destination(
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(AlsaInput, VirtualProtocol)> {
    let (client, protocol) = open_client(spec, true)?;
    let port = client.open_port(
        spec.name().as_str(),
        CAP_WRITE | CAP_SUBS_WRITE,
        TYPE_MIDI_GENERIC | TYPE_APPLICATION,
    )?;
    publish(&client, spec, protocol)?;
    let clock = client.stamp_port(port)?;
    Ok((input::start(client, clock, producer)?, protocol))
}
