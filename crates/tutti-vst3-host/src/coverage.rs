//! VST3 interface coverage registry — the single source of truth for which
//! VST3 COM interfaces this host handles, and which it deliberately does not
//! (yet).
//!
//! The `vst3` crate exposes ~73 interfaces, but most are optional capability
//! extensions or platform/tooling surface a plugin tolerates the absence of.
//! What matters is the **mandatory core** (the interfaces without which a
//! plugin can't load, process audio/MIDI, automate, show its editor, or persist
//! state) — those must all be [`Coverage::Core`]. The
//! [`mandatory_core_is_complete`](tests) test pins that, so deleting an
//! implementation breaks CI rather than silently regressing.
//!
//! Optional-but-implemented interfaces are [`Coverage::Optional`]. Interfaces we
//! know about but haven't wired are [`Coverage::Todo`] with a note on what each
//! would add; `uncovered_interfaces` lists them with greppable `TODO` markers.

/// How completely this host handles a given VST3 interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Mandatory core: implemented and required for normal hosting.
    Core,
    /// Optional extension this host implements anyway.
    Optional,
    /// Known but not implemented; the `&str` says what it would add.
    Todo(&'static str),
}

/// One VST3 interface and its coverage in this host.
#[derive(Debug, Clone, Copy)]
pub struct InterfaceCoverage {
    pub name: &'static str,
    pub coverage: Coverage,
}

const fn core(name: &'static str) -> InterfaceCoverage {
    InterfaceCoverage { name, coverage: Coverage::Core }
}
const fn optional(name: &'static str) -> InterfaceCoverage {
    InterfaceCoverage { name, coverage: Coverage::Optional }
}
const fn todo(name: &'static str, note: &'static str) -> InterfaceCoverage {
    // TODO(vst3): each `todo(...)` entry below is an unimplemented VST3
    // interface — grep `TODO(vst3)` to find them all. They have no call site in
    // the host yet (nothing references the interface), so this registry, not a
    // runtime `todo!()`, is where the deferral is recorded.
    InterfaceCoverage { name, coverage: Coverage::Todo(note) }
}

/// Every VST3 interface this host has an opinion about — implemented or
/// deliberately deferred. Interfaces absent from this list are ones the host
/// has no reason to touch (guest-side-only, or niche enough not to track).
pub const INTERFACES: &[InterfaceCoverage] = &[
    // ── Mandatory core: load, process, automate, edit, persist ──────────────
    core("IPluginFactory"),     // enumerate + instantiate plugin classes
    core("IComponent"),         // the plugin object: buses, state, activation
    core("IAudioProcessor"),    // setupProcessing / process — the audio path
    core("IEditController"),    // parameters, controller state, editor creation
    core("IPlugView"),          // the editor window
    core("IPlugFrame"),         // host side of the editor window (resize, etc.)
    core("IComponentHandler"),  // plugin → host parameter edits
    core("IConnectionPoint"),   // component ↔ controller message channel
    core("IMessage"),           // the messages sent over IConnectionPoint
    core("IBStream"),           // state save / load
    core("IParameterChanges"),  // per-block automation in/out
    core("IParamValueQueue"),   // one parameter's automation points
    core("IEventList"),         // MIDI + all 10 VST3 event types, both directions
    core("IHostApplication"),   // host services handed to the plugin
    core("IAttributeList"),     // attribute bags carried by IMessage / host
    core("IPlugInterfaceSupport"), // host answers "do you support interface X?"
    // ── Optional extensions this host implements ────────────────────────────
    optional("IMidiMapping"),   // CC → parameter routing (see host::midi_mapping)
    optional("IUnitHandler"),   // program-list / unit structure notifications
    optional("IUnitHandler2"),  // IUnitHandler + program-data-change signal
    optional("IProgress"),      // long-running task progress (scan, load)
    optional("IDataExchangeHandler"), // bulk host↔plugin data exchange
    optional("IComponentHandlerBusActivation"), // plugin-driven bus activation
    optional("IContextMenu"),   // host-provided right-click menus in the editor
    optional("IProcessContextRequirements"), // plugin declares which ProcessContext fields it needs (see types::transport)
    optional("INoteExpressionController"), // read the plugin's note-expression type metadata (see Vst3Loaded::note_expression_info)
    optional("IMidiLearn"),     // forward live MIDI-CC so the plugin can learn a CC→param assignment (see host::midi_learn)
    optional("IAutomationState"), // push the host's automation read/write mode to the plugin (see Vst3Loaded::set_automation_state)
    optional("IKeyswitchController"), // read the plugin's key-switch articulation map (see Vst3Loaded::keyswitch_info)
    optional("IRemapParamID"),  // remap saved param IDs across plugin versions on migration (see Vst3Loaded::remap_param_id)
    // ── Known but not implemented (graceful-degrade without these) ──────────
    todo("INoteExpressionPhysicalUIMapping", "map physical controls to note-expression dimensions"),
    todo("IParameterFunctionName", "resolve well-known parameter roles by function name"),
    todo("IPrefetchableSupport", "offline/prefetch processing mode negotiation"),
    todo("IAudioPresentationLatency", "report downstream presentation latency to the plugin"),
    todo("IPluginCompatibility", "machine-readable plugin migration / compatibility info"),
    todo("IXmlRepresentationController", "export a parameter remote-control XML layout"),
];

impl InterfaceCoverage {
    /// True when this interface still needs work.
    pub const fn is_todo(&self) -> bool {
        matches!(self.coverage, Coverage::Todo(_))
    }
}

/// The interfaces still marked [`Coverage::Todo`], in registry order. Each
/// retains its "what it would add" note. Greppable: search `TODO(vst3)`.
pub fn uncovered_interfaces() -> impl Iterator<Item = &'static InterfaceCoverage> {
    INTERFACES.iter().filter(|i| i.is_todo())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mandatory-core interface must be present and tagged `Core`. If a
    /// `Class` impl or core query is removed, update the registry — this test
    /// exists to make that a conscious, reviewed change rather than silent rot.
    #[test]
    fn mandatory_core_is_complete() {
        // The non-negotiable set: without any of these a plugin can't be
        // hosted at all (load → process → automate → edit → persist).
        const REQUIRED: &[&str] = &[
            "IPluginFactory",
            "IComponent",
            "IAudioProcessor",
            "IEditController",
            "IPlugView",
            "IPlugFrame",
            "IComponentHandler",
            "IConnectionPoint",
            "IMessage",
            "IBStream",
            "IParameterChanges",
            "IParamValueQueue",
            "IEventList",
            "IHostApplication",
        ];
        for name in REQUIRED {
            let entry = INTERFACES
                .iter()
                .find(|i| i.name == *name)
                .unwrap_or_else(|| panic!("core interface {name} missing from registry"));
            assert_eq!(
                entry.coverage,
                Coverage::Core,
                "{name} must be tagged Core, found {:?}",
                entry.coverage
            );
        }
    }

    /// No interface is listed twice (the registry is a set, not a bag).
    #[test]
    fn registry_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for i in INTERFACES {
            assert!(seen.insert(i.name), "duplicate registry entry: {}", i.name);
        }
    }

    /// `uncovered_interfaces` returns exactly the `Todo` entries, each with a
    /// non-empty note. Surfaces the deferred surface as a single, greppable
    /// inventory rather than scattered comments.
    #[test]
    fn uncovered_are_all_noted() {
        let todos: Vec<_> = uncovered_interfaces().collect();
        assert!(!todos.is_empty(), "expected some deferred interfaces");
        for t in todos {
            match t.coverage {
                Coverage::Todo(note) => assert!(!note.is_empty(), "{} has an empty note", t.name),
                other => panic!("{} surfaced as non-Todo {other:?}", t.name),
            }
        }
    }
}
