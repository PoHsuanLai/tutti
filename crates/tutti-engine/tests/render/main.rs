//! `Engine` over the graph: blocks, clocks, loop region, tempo and root channel layouts.

#[path = "../support/mod.rs"]
mod support;

mod block_clock;
mod engine_graph;
mod env_beat;
mod graph_contract;
mod loop_region;
mod root_channel_layouts;
mod tempo_ramps;
mod tempo_steps;
