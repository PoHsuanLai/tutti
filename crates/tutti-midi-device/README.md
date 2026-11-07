# tutti-midi-device

The pure core of MIDI device detection for the Tutti engine (design 017 §5).

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

No OS, no thread, no clock, and no dependency but optional `serde`. `tutti-midi-hardware` feeds the
table from ALSA and CoreMIDI. Licensed under MIT or Apache-2.0.
