# tutti-midi-device

The pure core of MIDI device detection for the Tutti engine (design 017 §5), and the
MIDI-CI codec.

- `DeviceTable`: takes an `OsChange` from a backend and returns the
  `DeviceEvent` it causes, if any. A repeat of a known state returns `None`.
- `DeviceId`: an identity plus an ordinal. The identity is what survives a restart
  where the OS gives something to hold (an ALSA client number is only a handle to
  the current connection, so an ALSA device is its client and port name). The
  ordinal tells alike devices apart within a run; it does not name a physical
  unit across a replug or restart.
- `DeviceTable::resolve`: what a saved `DeviceInfo` names among the devices
  present now. `Exact` where the identity names a unit (a CoreMIDI unique id; a
  Product Instance Id, told apart per port by name and direction); `SameName`
  for a lone device whose identity is only a name (ALSA), which may be another
  unit of that name; `Ambiguous`, with the candidates, for a saved ordinal
  above 0 or several alike devices; `Missing` otherwise. With the `serde`
  feature, `DeviceInfo`, `DeviceId`, `Identity` and `Direction` serialize, so
  an app can remember a device; renaming an `Identity` variant breaks saved
  files.
- `EndpointId`: the OS's own handle, opaque, valid while the device is present.
- `CiMessage`: the MIDI-CI (M2-101) codec over SysEx payload bytes: the header, `Muid`
  (checked: the reserved range and the broadcast value are refused; broadcast is a
  `Destination`), Discovery and its Reply, Invalidate MUID and NAK. `encode` returns
  the bytes between `F0` and `F7`; frame them with `tutti_ump::frame`, and decode the
  bytes a `tutti_ump::SysexAssembler` completes. There is no state machine and no
  inbound path yet (design 017 §5, "MIDI-CI").

- `Endpoint`: what one UMP endpoint declares (M2-104 §7.1): UMP version, protocols
  and JR timestamps supported and active, identity, name, Product Instance Id and up
  to 32 `FunctionBlock`s, all held inline. `Endpoint::reply` answers an Endpoint
  Discovery, a Function Block Discovery or a Stream Configuration Request into a
  caller's `Extend<Ump>`. `Endpoint::midi1_device` is a MIDI 1.0 device with its one
  inferred block.
- `Connection`: the discovering side (M2-100 Table 4 steps 2 to 4): Endpoint
  Discovery, a Stream Configuration Request for the wanted protocol and JR, Function
  Block Discovery. Time is an `Instant` the caller passes to `on_message` and
  `on_timeout`; a step is sent three times, a second apart, and a peer that never
  answers is settled as a MIDI 1.0 device. No backend feeds it yet.

No OS, no thread, no clock, and no dependency but `tutti-ump`, `bitflags` and optional `serde`. `tutti-midi-hardware` feeds the
table from ALSA and CoreMIDI. Licensed under MIT or Apache-2.0.
