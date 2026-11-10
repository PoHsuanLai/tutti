# Archive

Dated records of finished work. Each file is kept as written: it describes the
code and the questions as they stood when it was made, so a statement in it may
no longer hold. Read the current design in `docs/design/` and `CLAUDE.md`
first; come here for the history behind a decision.

| File | What it was |
|---|---|
| `004-plugin-host-gap-audit.md` | The audit of what the plugin hosts lacked (the D-, C- and A- items); its gaps are closed. |
| `005-plugin-presets.md` | The design for plugin presets (D-9), shipped. |
| `007-channel-topology.md` | The design for speaker arrangement and channel topology (D-11), reporting built, the proposal deferred. |
| `008-plugin-lifecycle-strategies.md` | Why the four hosts model their lifecycles differently; its rule, the fifth-format checklist and the AU pinned pointer now live in `docs/design/016-plugin-hosts.md` §2.3. |
| `009-plugin-crash-notification.md` | The design for telling the app a plugin crashed, built. |
| `010-parameter-grouping.md` | The design for parameter groups across the four formats, built. |
| `011-outcome-types.md` | The design for `Result` and enum outcomes on the plugin surface, built. |
| `unit-adoption-backlog.md` | The backlog of moving bare floats to unit types, completed apart from app-blocked items. |
| `fundsp-comparison.md` | The comparison of hand-written DSP against fundsp opcodes, from before fundsp was deleted. |
| `fundsp-fork-audit.md` | The audit of which modules of the vendored fundsp fork were reachable, from before the fork was deleted. |
| `fundsp-vs-tutti-units-audit.md` | The audit of `tutti-nodes` against fundsp duplicates, from before fundsp was deleted. |
| `benchmarks.md` | The benchmark baseline recorded on one machine on 2026-09-22; figures do not travel between machines. |
| `plugin-hosting/` | The CLAP and VST3 completion plans, the VST3 multi-bus plan and the CLAP conformance harness note, all executed. |
