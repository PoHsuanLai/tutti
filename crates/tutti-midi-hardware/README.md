# tutti-midi-hardware

Native-UMP OS MIDI I/O for the Tutti audio engine: enumerate endpoints, connect
them, send MIDI 2.0 out and receive it back, through CoreMIDI (macOS) and the
ALSA UMP sequencer (Linux). Re-exported by `tutti` as `tutti::midi_hardware`
(feature `midi-hardware`).

Events reach the wire as UMP words, not MIDI 1.0 bytes, so MIDI-2-only messages
(per-note controllers, per-note pitch bend, JR Timestamps) survive where the
device's Protocol carries them: an output is put in the device's wire form (the
OS's description, then what the UMP Stream negotiation settled), so a MIDI 1.0
endpoint is sent MIDI 1.0 and never type `0x4`, and JR Timestamps go only to a
device that receives them.

## Devices

[`Devices`] lists the MIDI devices and keeps the list current while it lives:
`open()` starts a watch, which stops when the last `Devices` and [`DeviceEvents`]
over it are dropped. `subscribe()` gives a [`DeviceEvents`] of its own, so one
reader never takes another's events. Each device has a [`DeviceId`], and
`open_input(&id, &ports)` returns an [`OpenInput`] that closes the connection
and retires its ring when dropped.
`resolve(&saved)` restores a remembered [`DeviceInfo`] (see `tutti-midi-device`'s
`serde` feature): `Exact` is safe to bind, `SameName` is a lone device that
matches only by name (ALSA), `Ambiguous` lists candidates. An id names a device within a run:
its identity is what the OS lets survive a restart, and the ordinal after it only
tells alike devices apart (unplug two identical keyboards and plug them back in
the other order, and their ordinals swap).

```rust,no_run
use std::time::Duration;
use tutti_midi_hardware::{DeviceEvent, Devices};

let devices = Devices::open()?;
let events = devices.subscribe();
for device in events.initial() {
    println!("{}: {}", device.id, device.name);
}
// Blocks until a device comes or goes, or the timeout passes.
while let Ok(event) = events.recv_timeout(Duration::from_secs(5)) {
    match event {
        DeviceEvent::Added(device) => println!("plugged in: {}", device.name),
        DeviceEvent::Removed(device) => println!("gone: {}", device.name),
        DeviceEvent::Changed(device) => println!("changed: {}", device.name),
    }
}
# Ok::<(), tutti_midi_hardware::Error>(())
```

- **Linux:** a thread subscribes to the sequencer's `System:Announce` port, so a
  device that vanishes is noticed when the kernel says so. A [`DeviceId`] is the
  client and port name plus an ordinal for identical devices; the client number
  is a handle to the current connection and not part of it. A client number the
  kernel hands to a new device straight after an old one exits is a new device,
  told by the exit announcement. On ALSA the name is the identity, so a renamed
  device is `Removed` and then `Added` under the new name, which closes a port
  held on it; `Changed` is for backends whose identity survives a rename.
- **macOS:** one process-wide CoreMIDI client runs its run loop on one thread
  for setup notifications, and each [`Devices`] registers with it. A
  [`DeviceId`] is the endpoint's `MIDIUniqueID`. Untested by the author; see
  doc 017.
- If the watch ends on its own, [`DeviceEvents::recv_timeout`] says why, once the events the watch reported are taken, and the list
  stays as it was.
- **Windows (and Linux without alsa-lib 1.2.10):** the list is empty and stays
  so, as enumeration is; opening reports [`Error::Unsupported`].

A [`DeviceId`] built on the UMP Product Instance Id is in the types, but no
backend reads one yet: alsa-lib exposes none, and CoreMIDI's unique id stands in.

## Quick start

[`MidiSession`] is the whole OS edge. Inbound events never pass *through* it:
each opened input is a port of [`HardwareMidiInputs`], a slot of the shared
[`DeviceInlets`], and the device's own thread stamps each arrival with the frame
the engine's block clock puts it on and pushes it into that device's event
inlet. The graph reads the inlets on their frames (a [`MidiIngestNode`] has one
event input per slot), so the audio thread converts no time. The session only
owns the connection. Disconnecting closes the port and gives its slot back, so
hot-plug cycles do not accumulate inlets.

```rust,no_run
use std::sync::Arc;
use tutti_midi_hardware::prelude::*;
use tutti_midi_hardware::{DeviceInlets, HardwareMidiInputs};
use tutti_fixtures::midi::note_on;
use tutti_graph::Offset;
use tutti_ump::{MidiChannel, MidiGroup};

// The inlets inbound events land in (four devices, 1024 events each, placed
// 10 ms after they arrive), then a session over this platform's backend.
let inlets = Arc::new(DeviceInlets::new(4, 1024, std::time::Duration::from_millis(10)));
let ports = Arc::new(HardwareMidiInputs::new(inlets));
let session = MidiSession::open(Arc::clone(&ports))?;

// The devices present now; capability (protocol, function blocks) is read from
// the OS.
for device in session.inputs() {
    println!("{}: {:?}", device.name, session.devices().capability(&device.id));
}

// Connect by id when the choice matters (see "Matching by name" below).
if let Some(first) = session.inputs().first() {
    session.connect_input(&first.id)?;
}

// Outbound: `send` returns how many events were accepted, 0 when nothing is
// connected.
session.connect_output_by_name("iac")?;
let note = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
assert_eq!(session.send(&[(Offset::ZERO, note)]), 1);
# Ok::<(), tutti_midi_hardware::Error>(())
```

## Main types

- [`Devices`]: the devices present, and the changes to them.
- [`MidiSession`]: connect, disconnect and send; owns a [`Devices`].
- [`HardwareMidiInputs`]: the open input ports, each a slot of the
  [`DeviceInlets`] and the [`InputProducerHandle`] its driver thread pushes through.
- [`DeviceInfo`], [`DeviceId`], [`DeviceEvent`]: what the list holds and reports;
  they live in `tutti-midi-device`, with the pure `DeviceTable` behind them.
- [`EndpointInfo`], [`EndpointId`], [`UmpCapability`]: what a backend enumerates.
- [`DeviceStream`], [`Basis`]: what a device's UMP Stream conversation settled
  (`MidiSession::poll` steps it, `MidiSession::stream` reads it).
- [`MidiEndpoints`]: the backend trait; [`MidiSession::open`] picks this
  platform's implementation.
- [`Error`]: what opening, connecting and sending can fail with.
- [`prelude`]: the common types in one import, including the graph's MIDI nodes
  and mailbox re-exported from `tutti-midi-runtime`.

## Matching by name picks a device you did not choose

Every `*_by_name` method matches **case-insensitive substring, first hit wins**,
in id order. So `"iac"` matches `"IAC Driver Bus 1"`, and a name matching two
devices takes the one with the lowest id. Connect by [`DeviceId`] when the
choice matters.

[`MidiSession::disconnect_input_by_name`] is weaker still: it searches a map of
open connections, so there is no "first" at all and a name matching two open
inputs closes an **arbitrary** one. Use [`MidiSession::disconnect_input`] with
an id, or [`MidiSession::disconnect_all_inputs`].

## Backends

| Platform | Backend | Notes |
|---|---|---|
| macOS | CoreMIDI | `MIDIInputPortCreateWithProtocol` / `MIDISendEventList`. |
| Linux | ALSA UMP sequencer | Needs alsa-lib ≥ 1.2.10. Kernel ≥ 6.5 converts UMP↔legacy at the port boundary, so it works against MIDI 1.0 hardware. |
| Windows | stub | No `windows` crate version binds Windows MIDI Services; WinRT `Devices.Midi` is MIDI 1.0 message types. Enumerates nothing, [`Error::Unsupported`] on open. |

[`MidiSession::open`] picks one, once, at construction; nothing downstream learns
which OS it is talking to.

**A build may have no backend, and it is not an error.** On Linux, the
`tutti-alsa-ffi` build script probes alsa-lib via `pkg-config`; below 1.2.10 (or with no alsa-lib) it emits a
`cargo:warning` naming the version and compiles the same stub Windows gets:
zero endpoints, and [`Error::Unsupported`] naming the reason. So code using this
crate compiles everywhere and simply enumerates nothing where there is no
backend.

## Features

- `test-support`: exports `test_support::FakeBackend`, a device list with no OS
  behind it, for [`MidiSession::open_over`] in headless tests; its `plug` and
  `unplug` drive hot-plug. Off by default.
- `sequencer-tests`: the suites that run `Devices` against the real ALSA
  sequencer, with user-space clients as the devices. Linux only; needs the
  `snd-seq` module and a readable `/dev/snd/seq`. Off by default.

## Where it sits

Builds on `tutti-ump` (the wire vocabulary and its decoded `Message`) and `tutti-midi-runtime`
(the graph's MIDI nodes and mailbox), and re-exports the parts of both a
hardware consumer needs. It does **not** include the file codecs: SMF and the
path-level Clip File codec are in `tutti-midi-file`, so a consumer that only
reads `.mid` files does not link CoreMIDI or ALSA. Re-exported by `tutti`; the
Bevy systems that drive a session are in `bevy-tutti`.

## Examples

The loopback examples drive real hardware, so they need a device present:

```bash
cargo run --example list_devices          # list, then watch hot-plug for ten seconds
cargo run --example iac_loopback_test     # macOS, needs the IAC Driver enabled
cargo run --example alsa_loopback_test    # Linux, uses `Midi Through`
```

Two things the Linux example documents that are easy to get wrong: **VirMIDI does
not echo** (writing to it feeds the raw-MIDI device, not that port's own
subscribers, so a send/receive pair on one virmidi port receives nothing), and
`Midi Through` is the kernel's actual loopback.

## License

MIT OR Apache-2.0
