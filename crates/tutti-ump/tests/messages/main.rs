//! Messages and their fields: the codec, the spec tables and the checked types.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod common;

mod by_value;
mod constructors;
mod fields;
mod key_event;
mod no_alloc;
mod note_id;
mod note_velocity;
mod properties;
mod reserved;
mod serde;
mod spec_tables;
mod spec_words;
mod value_types;
