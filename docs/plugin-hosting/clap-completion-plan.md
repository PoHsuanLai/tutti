# CLAP hosting completion plan

**Goal:** take tutti's CLAP host from ~85–90% to production-grade across platforms.
tutti's CLAP host is already the strongest of the four — true multi-port audio,
full param/automation/state/MIDI, 26 host extensions, RT-safe. The remaining
gaps are **host-side follow-through**, not missing bindings. None require forking
a dependency.

Repo: `/Users/pohsuanlai/Documents/dawAI/dawai` (the tutti sub-workspace is at
`crates/tutti/`; run cargo from there). Crates: `tutti-clap-host`,
`tutti-plugin` (GUI bridge), `tutti-plugin-server` (subprocess driver).

Testing policy (non-negotiable, from CLAUDE.md): never weaken a test to make it
pass; fix the underlying issue. Add tests for new behavior. Keep the RT path
allocation-free (there are `assert_no_alloc` tests in `instance/audio.rs` — they
must stay green).

---

## Work items (priority order)

### C1 — [HIGHEST ROI] Pump timer-support + posix-fd in the GUI idle loop
**Problem.** `tutti-clap-host` *implements* `clap.timer-support` and
`clap.posix-fd-support` host extensions (`instance/polling.rs:314` `poll_timers`,
`:579` `poll_posix_fds`), but the in-process GUI bridge never calls them. See
`crates/tutti/crates/tutti-plugin/src/bridge/gui/clap.rs:45-50` — `editor_idle`
only does `poll_params_flush_requested`/`flush_params`. **Consequence:** Linux
CLAP editors that drive repaint/animation off timers or an X11 fd freeze.

**Fix.** In `ClapGuiInstance::editor_idle` (clap.rs:45), also call
`self.inner.poll_timers()` and, on unix, `self.inner.poll_posix_fds()`. Verify
those methods are `pub` on `ClapInstance` (they are, per polling.rs) and callable
without audio activation (the GUI instance is not activated — confirm they don't
assert active).

**Caveat to document, not necessarily fix now:** firing is poll-interval-bound,
not a real event loop — a timer with period < the idle tick interval fires at the
tick rate. Note this in a code comment; a real timer wheel is out of scope.

**Test.** Add a test that registers a timer via the host extension and asserts
`poll_timers` fires it after the period elapses (can use a mock/elapsed-time
injection if the timer store allows; otherwise an integration test gated on a
real CLAP plugin). At minimum, a unit test that `editor_idle` invokes the poll
paths (e.g. via a counter on a test double).

**Effort: S. Impact: HIGH (Linux GUI).**

### C2 — Linux Wayland window API + GUI API negotiation
**Problem.** `instance/polling.rs:46-50` hardcodes `CLAP_WINDOW_API_X11` on Linux;
no Wayland, no negotiation. Wayland-only plugins can't embed.
**Fix.** Before `set_parent`, query the plugin's `is_api_supported` /
`get_preferred_api` (clap_plugin_gui). Choose X11 or Wayland based on what the
host window actually is (the `WindowHandle` carries a raw handle — determine its
type) and what the plugin supports. Keep Cocoa/Win32 paths unchanged.
**Test.** Unit-test the API-selection logic (given supported set + window type →
chosen api) without a real plugin.
**Effort: M. Impact: medium (Linux Wayland users).**

### C3 — gui.set_scale for HiDPI
**Problem.** `set_scale` is never called; plugins render at wrong DPI on
HiDPI/Retina/fractional-scale.
**Fix.** After `create`/before `show`, call the plugin GUI `set_scale` with the
host window's scale factor. Plumb a scale factor into `open_editor` (the GUI
bridge `open_editor` signature may need a scale param, or read it from the
window). Make it best-effort (many plugins ignore it).
**Test.** Assert `set_scale` is invoked with the expected factor via a test double.
**Effort: S. Impact: medium.**

### C4 — param value_to_text / text_to_value
**Problem.** No wrapper exists (confirmed absent in `tutti-clap-host/src`). Host
shows raw 0–1 normalized values; no "−6.0 dB" display, no typed entry.
**Fix.** Add `value_to_text(param_id, value) -> Option<String>` and
`text_to_value(param_id, &str) -> Option<f64>` on `ClapInstance` calling the
`clap_plugin_params.value_to_text`/`text_to_value` fns. These are **[main-thread]**
per the CLAP spec — add the existing `assert_main_thread()` guard (the host already
has one, see C-host main-thread infra). Surface through the metadata/param path so
the frontend can use them.
**Test.** Integration test against a real CLAP plugin (TAL-NoiseMaker at
`/Library/Audio/Plug-Ins/CLAP/TAL-NoiseMaker.clap`, gated `#[cfg(feature="clap")]`):
round-trip a known param value → text → value.
**Effort: S. Impact: medium (everyday UX).**

### C5 — Note dialect breadth (MPE / MIDI2)
**Problem.** Host advertises only `CLAP|MIDI` in
`host_note_ports_supported_dialects` (`host/callbacks.rs:269-271`); note *input*
downconverts UMP→MIDI1 (`instance/events.rs:187-207`), losing MPE/MIDI2 nuance
(per-note expression, note_id).
**Fix.** Advertise MPE and MIDI2 dialects; carry CLAP-native note events
(note_id, per-note expression) end-to-end instead of collapsing to MIDI1 where the
plugin supports the richer dialect. This touches the event conversion in
`events.rs` and the subprocess MIDI plumbing. **Larger/riskier than C1–C4** —
scope carefully; if it balloons, land the dialect advertisement + note_id
preservation first and defer full per-note expression.
**Test.** Round-trip an MPE note (with note_id + pitch expression) through the
event path; assert it isn't flattened.
**Effort: M. Impact: medium (expressive synths). Do LAST.**

---

## Sequencing for the agent
1. C1 (tiny, highest impact) → 2. C3 (small) → 3. C4 (small) → 4. C2 (medium) →
5. C5 (medium, optional if time/risk). Land each as its own commit. Run
`cargo test -p tutti-clap-host` and `cargo clippy -p tutti-clap-host
-p tutti-plugin -- -D warnings` after each. Keep the `assert_no_alloc` audio
tests green.

## Out of scope
Real timer-wheel event loop; CLAP preset-discovery factory browsing; thread-pool
worker; floating-window GUIs. Note them as follow-ups, don't build.

## Done criteria
- `editor_idle` pumps timers + posix-fds (C1); Linux CLAP editor repaints.
- API negotiation picks Wayland when appropriate (C2).
- set_scale called (C3); value_to_text/text_to_value available + tested (C4).
- (If done) MPE/MIDI2 dialects advertised and note_id preserved (C5).
- All `tutti-clap-host` + `tutti-plugin` tests pass; clippy -D warnings clean.
