//! The session's I/O edges: devices and their restart, the recorder, the inlets and the taps.

// The device-free device tests (`ManualStreamDriver`): `device` is a feature,
// so this suite runs under it (CI's dark-features job names it; a workspace run
// no longer turns it on).
#[cfg(feature = "device")]
mod device;
#[cfg(feature = "device")]
mod device_listing;
// A restart onto another rate, width or block (the session halves of
// `bevy-tutti`'s `engine::restart` tests), and the MIDI clock's re-rate.
#[cfg(all(feature = "device", feature = "midi"))]
mod device_midi_clock;
#[cfg(feature = "device")]
mod device_restart;
// The recorder: its thread, and the pump over a tap and a decoded file.
mod event_inlet;
mod event_tap;
#[cfg(feature = "record")]
mod file_in_pump;
mod inlet;
#[cfg(feature = "record")]
mod recorder_thread;
mod tap_close;
#[cfg(feature = "record")]
mod tap_to_wav_roundtrip;
