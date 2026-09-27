# tutti-midi-hardware

Native-UMP OS MIDI I/O for the Tutti audio engine: enumerate endpoints, connect
them, send MIDI 2.0 out and receive it back, through CoreMIDI (macOS) and the
ALSA UMP sequencer (Linux). Re-exported by `tutti` as `tutti::midi_hardware`
(feature `midi-hardware`).

Events reach the wire as UMP words, not MIDI 1.0 bytes, so MIDI-2-only messages
(per-note controllers, per-note pitch bend, JR Timestamps) survive.

## Quick start

[`MidiSession`] is the whole OS edge. Inbound events never pass *through* it:
each opened input gets its own lock-free ring in [`HardwareMidiInputs`], which
the audio thread drains through [`MidiIn::poll_block`] (usually from a
[`MidiInputNode`] in the graph); the session only owns the connection, and
dropping it closes the port.

```rust,no_run
use std::sync::Arc;
use tutti_midi_hardware::prelude::*;
use tutti_midi_hardware::HardwareMidiInputs;

// The rings inbound events land in, then a session over this platform's backend.
let ports = Arc::new(HardwareMidiInputs::new(1024));
let session = MidiSession::new(Arc::clone(&ports));

// A fresh snapshot per call: device lists go stale on hot-plug, so nothing is
// cached. Capability (protocol, function blocks) is read from the OS.
for endpoint in session.inputs() {
    println!("{}: {:?}", endpoint.name, endpoint.capability);
}

// Connect by id when the choice matters (see "Matching by name" below).
if let Some(first) = session.inputs().first() {
    session.connect_input(first.id)?;
}

// Outbound: `send` returns how many events were accepted, 0 when nothing is
// connected.
session.connect_output_by_name("iac")?;
let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
assert_eq!(session.send(&[note]), 1);
# Ok::<(), tutti_midi_hardware::Error>(())
```

## Main types

- [`MidiSession`]: enumerate, connect, disconnect and send.
- [`HardwareMidiInputs`]: the per-input lock-free rings the audio thread polls;
  it implements [`MidiIn`].
- [`EndpointInfo`], [`EndpointId`], [`UmpCapability`]: what enumeration returns.
- [`MidiEndpoints`]: the backend trait; [`backend::active`] returns this
  platform's implementation.
- [`Sysex7ByteAssembler`]: reassembles SysEx7 runs from transports that deliver
  raw `F0 … F7` bytes.
- [`Error`]: what opening, connecting and sending can fail with.
- [`prelude`]: the common types in one import, including the graph's MIDI nodes
  and mailbox re-exported from `tutti-midi-runtime`.

## Matching by name picks a device you did not choose

Every `*_by_name` method matches **case-insensitive substring, first hit wins**,
in the backend's enumeration order, which is not sorted and not stable across a
hot-plug. So `"iac"` matches `"IAC Driver Bus 1"`, and a name matching two
devices takes whichever the OS listed first. Connect by [`EndpointId`] when the
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

[`backend::active`] picks one, once, at construction; nothing downstream learns
which OS it is talking to.

**A build may have no backend, and it is not an error.** On Linux, `build.rs`
probes alsa-lib via `pkg-config`; below 1.2.10 (or with no alsa-lib) it emits a
`cargo:warning` naming the version and compiles the same stub Windows gets:
zero endpoints, and [`Error::Unsupported`] naming the reason. So code using this
crate compiles everywhere and simply enumerates nothing where there is no
backend.

## Features

- `test-support`: exports `test_support::FakeBackend`, a device list with no OS
  behind it, for [`MidiSession::with_backend`] in headless tests. Off by
  default.

## Where it sits

Builds on `tutti-midi-types` (the wire vocabulary) and `tutti-midi-runtime`
(the graph's MIDI nodes and mailbox), and re-exports the parts of both a
hardware consumer needs. It does **not** include the file codecs: SMF and the
path-level Clip File codec are in `tutti-midi-file`, so a consumer that only
reads `.mid` files does not link CoreMIDI or ALSA. Re-exported by `tutti`; the
Bevy systems that drive a session are in `bevy-tutti`.

## Examples

Both loopback examples drive real hardware, so they need a device present:

```bash
cargo run --example list_devices          # enumerate, with protocol + function blocks
cargo run --example iac_loopback_test     # macOS, needs the IAC Driver enabled
cargo run --example alsa_loopback_test    # Linux, uses `Midi Through`
```

Two things the Linux example documents that are easy to get wrong: **VirMIDI does
not echo** (writing to it feeds the raw-MIDI device, not that port's own
subscribers, so a send/receive pair on one virmidi port receives nothing), and
`Midi Through` is the kernel's actual loopback.

## License

MIT OR Apache-2.0
