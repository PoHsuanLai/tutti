//! What a clip, a harmony lane and the note ledger send, and what they count when they lose.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod clip_node;
mod event_loss;
mod harmony_node;
mod note_ledger;
