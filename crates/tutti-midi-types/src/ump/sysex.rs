//! The SysEx payload cap the engine shares. Writing SysEx7 is
//! `tutti_ump::sysex7_packets`; reading it is `tutti_ump::decode` and
//! `Sysex7Packet::data`; reassembling is `tutti_ump::SysexAssembler`.

/// The longest SysEx payload (the bytes between `0xF0` and `0xF7`) the engine
/// carries: the hardware input abandons a longer dump, and the plugin hosts
/// reserve room to reassemble one this long. A path's event caps can be
/// lower: a plugin node takes at most 256 MIDI events a chunk (a device
/// callback; a block for the in-process VST2 node), so a dump whose packets
/// arrive in one block (as the hardware input's do) reaches a plugin only up
/// to 1536 payload bytes.
///
/// A cap exists because the hazard is not a large dump but a **lost
/// terminator**: a device unplugged mid-dump, or a dropped buffer, leaves a
/// reassembler mid-message with no end ever arriving, and an uncapped buffer
/// then grows for the lifetime of the connection.
///
/// 64 KiB sits above real traffic and below anything alarming: the largest
/// thing MIDI-CI negotiates is a Property Exchange chunk, and CI's own
/// `max_sysex_size` is a 28-bit field that devices populate with values in the
/// hundreds-to-low-thousands of bytes. Synth bulk dumps (a 32-voice DX7 bank
/// is 4104 bytes) and sample-dump packets are far below it. A message past
/// this is a fault, not a big message.
pub const SYSEX7_MAX_PAYLOAD: usize = 64 * 1024;
