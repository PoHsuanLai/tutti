//! Shared fixtures for this crate's integration suites.
//!
//! Each test binary compiles this tree separately, and no single binary uses
//! every item in it, so the unused-code lint is silenced here rather than
//! per-item: an item is "dead" only from the vantage of one binary, and
//! annotating for that would push noise into code that is live elsewhere.

#![allow(dead_code)]

pub use tutti_fixtures::main_thread;

pub mod active;
pub mod raw;

/// The MIDI events an output list holds, as the processor drains them.
pub fn midi_events(list: &tutti_clap_host::OutputEventList) -> Vec<tutti_plugin_types::WireMidi> {
    let mut out = tutti_plugin_types::RtMidiEvents::new();
    list.fill_midi_events(&mut out);
    out.iter().copied().collect()
}

/// The parameter changes an output list holds, grouped by parameter.
pub fn param_changes(
    list: &tutti_clap_host::OutputEventList,
) -> tutti_plugin_types::ParameterChanges {
    let mut out = tutti_plugin_types::ParameterChanges::reserved(16, 64);
    assert_eq!(list.fill_param_changes(&mut out), 0, "nothing was lost");
    out
}

/// The note expressions an output list holds, as the processor drains them.
pub fn note_expressions(
    list: &tutti_clap_host::OutputEventList,
) -> Vec<tutti_clap_host::ClapNoteExpression> {
    let mut out = tutti_clap_host::RtNoteExpressions::new();
    list.fill_note_expressions(&mut out);
    out.iter().copied().collect()
}
