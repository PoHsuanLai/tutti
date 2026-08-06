//! The Linux backend: ALSA's UMP sequencer.
//!
//! Present only when `build.rs` found alsa-lib ≥ 1.2.10 (`cfg(alsa_ump)`).
//!
//! # UMP over a sequencer that predates it
//!
//! `snd_seq_set_client_midi_version(SND_SEQ_CLIENT_UMP_MIDI_2_0)` turns an
//! ordinary seq client into a **UMP** client: `snd_seq_ump_event_input` /
//! `_output_direct` then carry four-word UMP messages instead of the legacy
//! byte-oriented events.
//!
//! Paired with `snd_seq_set_client_ump_conversion(1)`, the kernel (≥ 6.5)
//! translates in both directions at the port boundary, so a UMP client can talk
//! to legacy hardware and a legacy app can talk to us — neither side needing to
//! know. That is what makes this backend useful today, when almost no Linux
//! machine has a native-UMP device: the conversion is the kernel's job, not
//! ours, and unlike a userspace MIDI-1.0 fallback it does not silently drop
//! MIDI-2-only messages that *are* representable.
//!
//! # Threading
//!
//! `snd_seq_ump_event_input` blocks, so each open input runs a pump thread that
//! reads and pushes into the connection's [`InputProducerHandle`] — the same
//! ring CoreMIDI's callback feeds. The thread exits when its connection drops.

pub mod sys;
