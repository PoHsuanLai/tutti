# tutti-midi-runtime

MIDI **runtime state machines** for the Tutti audio engine.

## What this is

Pure MIDI types live in [`tutti-midi-types`](../tutti-midi-types); OS ports live
in [`tutti-midi-hardware`](../tutti-midi-hardware). This crate owns the runtime
state that connects them:

- `MidiMailbox` / `MidiSender` / `MidiReceiver` — lock-free per-unit MIDI
  inboxes. A node owns a receiver; callers push through senders.
- `MidiInPort` — a unit's whole MIDI endpoint in one borrow: routing address,
  push mailbox, and the source-install slot.
- `MidiBus` — fan-out from `MidiUnitId` to the right inbox; the engine installs
  one as its audio-thread dispatch target.
- `MidiSnapshot` / `MidiSnapshotReader` / `MidiClipSource` — non-destructive
  event storage and the readers that play it back, on a live or an offline
  timeline.
- `MidiRoutingTable` — the UI-thread writer that publishes immutable routing
  snapshots through `RtPublish`.
- `ClockMaster` / the `outbound` module — engine-produced MIDI *out* (Beat Clock,
  MTC, JR timestamps), riding the **same** mailbox as MIDI in.
- `MpeIngest` — the input-edge transform that rewrites classic-MPE channel
  spread into native MIDI-2 per-note messages.
- `negotiate` / `sysex` — UMP-Stream endpoint discovery, MIDI-CI, and SysEx7/8
  packet reassembly.

## Why it is its own crate

It is the middle of a three-way split that keeps two costs off consumers who do
not pay them. `tutti-midi-types` is pure values with no state; this crate is
state with **no OS API**; `tutti-midi-hardware` is the OS edge. So a synth that
needs a MIDI inbox, or an offline export that needs a snapshot reader, gets both
without linking CoreMIDI or ALSA.

One placement worth noting: MPE lives here as an *ingestion* transform, not as a
per-synth state machine. Per M2-104, MPE is an input-edge concern — synth voices
track per-note expression themselves.

## Where it sits

Depends on `tutti-midi-types` and `tutti-core` (with `midi`).
`tutti-polysynth`, `tutti-soundfont`, `tutti-plugin`, `tutti-midi-hardware`,
`tutti-cpal` (behind its `midi` feature) and `bevy-tutti` all depend on it.

It re-exports `MidiRoutingTable` and the MPE mode/zone types from
`tutti-midi-types`, so a consumer of the runtime needs one import rather than
two.

## Features

`default = []`, and nothing else — the crate is the runtime.

## License

MIT OR Apache-2.0
