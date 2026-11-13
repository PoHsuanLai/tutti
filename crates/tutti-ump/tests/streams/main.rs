//! Streams: SysEx, SysEx8, Mixed Data Sets, Flex Data text and `split_stream`.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod data128_no_alloc;
mod flex_text;
mod flex_text_assembler;
mod midi1_stream;
mod mixed_data;
mod split_stream;
mod sysex;
mod sysex8;
mod sysex_assembler;
mod sysex_packets;
