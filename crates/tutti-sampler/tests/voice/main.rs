//! A sampler voice: gain, loops, seeks, commands and the two tiers.

mod block_length_invariance;
mod disk_placement;
mod frame_exact_entry;
mod gain;
mod loops_and_handles;
mod reverse_and_loop;
mod rt_no_alloc;
mod stretched_seek;
mod tier_parity;
mod voice_gain_survives_commit;
mod voice_node_commands;
mod wrap_fade_modes;

/// Aborts inside an `assert_no_alloc` region (`disk_placement`, `rt_no_alloc`); inert elsewhere.
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
