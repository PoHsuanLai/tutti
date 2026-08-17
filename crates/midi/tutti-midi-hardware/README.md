# tutti-midi-hardware

Native-UMP OS MIDI I/O: enumerate endpoints, connect them, send and receive
MIDI 2.0.

## What this is

The OS edge. Everything here is a port — there are no features, because with the
file codecs living in [`tutti-midi-file`](../tutti-midi-file) there is nothing in
this crate that is not OS MIDI.

**No MIDI 1.0 transport.** Events reach the wire as UMP words, so MIDI-2-only
messages — per-note controllers, per-note pitch bend, and JR Timestamps —
survive. The MIDI-1.0 path this replaced ran every event through
`to_midi1_bytes`, which returns `None` for exactly those and dropped them
silently.

## Backends

| Platform | Backend | Notes |
|---|---|---|
| macOS | CoreMIDI | `MIDIInputPortCreateWithProtocol` / `MIDISendEventList`. No new dependencies. |
| Linux | ALSA UMP sequencer | Needs alsa-lib ≥ 1.2.10. Kernel ≥ 6.5 converts UMP↔legacy at the port boundary, so it works against MIDI-1.0 hardware. |
| Windows | stub | No `windows` crate version binds Windows MIDI Services; WinRT `Devices.Midi` is MIDI-1.0 message types. Enumerates nothing, `Error::Unsupported` on open. |

`backend::active()` picks one. It is the only `cfg(target_os)` in the crate that
decides anything — and it decides at construction, not on the hot path, so
nothing downstream learns which OS it is talking to.

On Linux, `build.rs` probes alsa-lib via `pkg-config` and **degrades rather than
fails**: below 1.2.10 it emits a `cargo:warning` naming the version and compiles
the stub, so a distro with an older alsa-lib still builds.

## Quick start

```rust,no_run
use std::sync::Arc;
use tutti_midi_hardware::{HardwareMidiInputs, MidiSession};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

// The ring the audio thread drains, and the session that fills it.
let ports = Arc::new(HardwareMidiInputs::new(256));
let session = MidiSession::new(ports.clone());

// Endpoints carry what they can *carry* — protocol, function blocks — read
// from the OS rather than assumed.
for e in session.inputs() {
    println!("{} ({:?})", e.name, e.capability.protocol);
}

session.connect_input_by_name("Keystep")?;
session.connect_output_by_name("Synth")?;

// `send` returns how many events were accepted: 0 when nothing is connected,
// so "nothing is coming out" has an answer.
session.send(&[MidiEvent::note_on(
    MidiGroup::FIRST,
    MidiChannel::FIRST,
    60,
    0x8000, // 16-bit velocity — this is MIDI 2.0
)]);

// Inbound events never pass through the session. A backend pushes them into
// `ports`, which the audio thread drains through `MidiIn::poll_into`.
# Ok::<(), tutti_midi_hardware::Error>(())
```

## Examples

Both drive real hardware, so they need a device present:

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
