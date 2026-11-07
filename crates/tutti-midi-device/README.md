# tutti-midi-device

The pure core of MIDI device detection for the Tutti engine (design 017 §5).

- `DeviceTable`: takes an `OsChange` from a backend and returns the
  `DeviceEvent` it causes, if any. A repeat of a known state returns `None`.
- `DeviceId`: an identity plus an ordinal. The identity is what survives a restart
  where the OS gives something to hold (an ALSA client number is only a handle to
  the current connection, so an ALSA device is its client and port name). The
  ordinal tells alike devices apart within a run; it does not name a physical
  unit across a replug or restart.
- `DeviceTable::resolve`: what a saved `DeviceId` names among the devices
  present now. `Exact` for the one device of a one-of-a-kind identity;
  `Ambiguous`, with the candidates, when the saved ordinal is above 0 or several
  devices are alike; `Missing` otherwise. With the `serde` feature, `DeviceId`
  and `Identity` serialize, so an app can remember a device.
- `EndpointId`: the OS's own handle, opaque, valid while the device is present.

No OS, no thread, no clock, and no dependency but optional `serde`. `tutti-midi-hardware` feeds the
table from ALSA and CoreMIDI. Licensed under MIT or Apache-2.0.
