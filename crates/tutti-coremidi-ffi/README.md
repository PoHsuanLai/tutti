# tutti-coremidi-ffi

The unsafe half of the macOS MIDI backend. Every `unsafe` call
`tutti-midi-hardware` makes into CoreMIDI and the Mach clock lives here, behind
safe handles; the backend above it is `#![forbid(unsafe_code)]` and holds the
logic (enumeration, the device watch, the packet to `Instant` placement).

The crate is empty off macOS.

## What is here

| | |
|---|---|
| `UmpSource` | a native-UMP (MIDI 2.0 protocol) virtual source: it disposes its endpoint on drop, and sends a UMP message as an event list. The safe `coremidi` wrapper has the destination side and no source |
| `ump_endpoints` | the UMP Endpoints and Function Blocks CoreMIDI reports (macOS 15's `MIDIUMPEndpoint`, Objective-C classes bound through `objc2-core-midi`), as plain values; `None` before macOS 15, where the classes do not exist |
| `Timebase`, `host_ticks` | the Mach host clock CoreMIDI timestamps its packets with, and the ratio from its ticks to nanoseconds |

Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
`undocumented_unsafe_blocks` and `missing_safety_doc` are denied. What the safe
`coremidi` crate already wraps (ports, endpoints, properties) stays in the
backend: it needs no `unsafe` of ours.
