# tutti-alsa-ffi

The unsafe half of the Linux MIDI backend. Every `unsafe` call
`tutti-midi-hardware` makes into alsa-lib's sequencer and UMP API lives here,
behind safe handles; the backend above it is `#![forbid(unsafe_code)]` and holds
the logic (the pump and watch threads, the device model, the id packing).

Linux only, and only where alsa-lib is 1.2.10 or newer (the build script probes
it and sets `cfg(alsa_ump)`); on other targets, or with an older or missing
alsa-lib, the crate is empty and the backend takes its stub.

## What is here

| | |
|---|---|
| `Client` | an open sequencer client in UMP MIDI-2.0 mode: it closes on drop, and ports, subscriptions, `read_event`, `send_to_subscribers`, `stamp_port` and `list_ports` are methods |
| `UmpEndpoint`, `UmpBlock` | what the sequencer records of a client's UMP Endpoint (the protocol in use, the protocols offered) and its Function Blocks, from `Client::ump_endpoint` |
| `QueueClock` | the queue a stamped port reads its timestamps from, and the clock that says what time it is now |
| `Poller` | a client's descriptors and a stop descriptor, waited on with `poll(2)` |
| `Received`, `Announced` | an event copied out of alsa-lib's buffer, and what an announce-port event says |
| `Error` | the alsa-lib call that failed and its negative code |

The raw declarations are a private module (`sys`), with alsa-lib's C names so a
reader can grep its headers. `snd_seq_ump_event_t` mirrors a C struct whose last
member is a union, so its size and field offsets are measured against the real
header and re-checked by compile-time asserts.

Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
`undocumented_unsafe_blocks` and `missing_safety_doc` are denied.

## Linking and `links`

`alsa-sys` (through `cpal`) declares `links = "alsa"`, so this crate declares
`links = "asound"` and the two coexist. Both link `libasound`, which is
harmless. The key also carries the build script's probe result to dependents as
`DEP_ASOUND_ALSA_UMP` (alsa-lib >= 1.2.10) and `DEP_ASOUND_ALSA_UMP_CREATE`
(>= 1.2.13). With an older or missing alsa-lib the build warns and sets neither;
nothing is linked.

## Features

`test-support` adds `Client::rename` and `Client::set_port_caps`, which let a
test stand in for a device that appears, changes and goes. Off in a production
build.
