//! Nodes through the session: mix, mode, multiband params, reverbs and the sampler.

mod multiband_params;
mod node_mix;
mod node_mode;
mod param_units;
mod reverb_nodes;
mod silenced_event;
// A sampler node the graph adds reads the session's tempo map (doc 020, stage 5).
#[cfg(feature = "sampler")]
mod sampler_tempo;
// A clip is a placed sample that is a graph node (the sampler builder).
#[cfg(feature = "sampler")]
mod clip_node;
// A voice of either tier arrives where the compiler puts it (doc 025 stage 5).
#[cfg(feature = "sampler")]
mod tier_alignment;
