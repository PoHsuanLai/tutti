//! Modulation through the session: lanes, routes onto params, edge gain and mix.

#[cfg(feature = "modulation")]
mod common;

// The session's modulation loop (`Session::mods`, `ModInput`).
#[cfg(feature = "modulation")]
mod modulation;
// A route onto an edge's gain (doc 013, "Edge gain", G2).
#[cfg(feature = "modulation")]
mod edge_modulation;
// A route onto a node's mix (doc 013, "Bypass and mute are graph state", M4b).
#[cfg(feature = "modulation")]
mod mix_modulation;
// Modulation on control lanes through the session: renders, takes, refusals
// and the node-side declarations (`export` and `synth` gate two cases inside
// the file).
#[cfg(feature = "modulation")]
mod mod_lanes;
// The insertion paths address a node's params, so a lane's knob is its cell.
#[cfg(feature = "modulation")]
mod params_at_insert;
// Indexed param addresses through the session: a graph write and a modulation
// route to a node's second param of one kind.
#[cfg(feature = "modulation")]
mod indexed_params;
