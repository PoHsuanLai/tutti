# tutti-midi-device

The pure core of MIDI device detection for the Tutti engine (design 017 §5).

- `DeviceTable`: takes an `OsChange` from a backend and returns the
  `DeviceEvent` it causes, if any. A repeat of a known state returns `None`.
- `DeviceId`: what survives a restart where the OS gives something to hold. An
  ALSA client number is only a handle to the current connection, so an ALSA
  device is its client and port name plus an ordinal.
- `EndpointId`: the OS's own handle, opaque, valid while the device is present.

No OS, no thread, no clock, no dependency. `tutti-midi-hardware` feeds the
table from ALSA and CoreMIDI. Licensed under MIT or Apache-2.0.
