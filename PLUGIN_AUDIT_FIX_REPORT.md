# Plugin Layer Audit — Fix Run Report

Date: 2026-07-26 · Tree: `dawai-plugin-audit` · Base: `4e2a19528`
Scope: `crates/tutti/crates/plugin/**` only. No `crates/dawai-*` files touched. Nothing committed.

> This report **replaces** an earlier partial-run report. That run's format phase never
> executed and its GATE-2 entry was a stale pre-fix snapshot. Both are corrected here.

---

## 1. Bottom line

Out-of-process plugin audio **is fixed** — the audit's bypass (host reading its own input
back at unity gain) is gone. I forced failures under CPU load and classified every one:
all were `got silence`, zero `UNITY gain`, zero `PREVIOUS block`. No wrong audio is reachable.

`cargo check` on all 8 plugin crates: **EXIT 0**. `cargo test -p tutti-plugin` compiles and
runs: **117 passed / 1 failed** — the 1 failure (`probe_real_plugin`) is **pre-existing**,
reproduced at pristine HEAD (97 passed / 1 failed).

Still broken: the new RT wait budget is **too small to meet under load**. At the real call
site (`BATCH_SIZE=64`, 48 kHz) it is 667 µs, below the ~0.7–1 ms scheduler wake latency.
Idle: 20/20 green. Under 24× CPU load: **4/12 green**. Failure mode is silence, not corruption.
Also: VST3's module-entry fix **does not compile on Linux**, and NaN can reach a live AU param.

---

## 2. Reconciliation table

41 findings. **3 rows disagree outright, 5 are partial** — these 8 are what a human must review.

| # | Finding | Crate | Claimed | Verdict | Agree? |
|---|---|---|---|---|---|
| 1 | **GATE-2** | tutti-plugin | fixed (as *failing* repro tests) | fixed-but-regressed | ❌ **DISAGREE** |
| 2 | **TRANSPORT-C1** | tutti-plugin | fixed | fixed-but-regressed | ❌ **DISAGREE** |
| 3 | **TRANSPORT-C1-RETUNE** | tutti-plugin | fixed | fixed-but-regressed | ❌ **DISAGREE** |
| 4 | SHARED-H1 | tutti-plugin-types | fixed | confirmed-fixed | ✅ |
| 5 | SHARED-H2 | tutti-plugin-types | fixed | confirmed-fixed | ✅ |
| 6 | **SHARED-M1** | tutti-plugin-types | fixed | fixed-but-regressed | ⚠️ **PARTIAL** |
| 7 | SHARED-M2 | tutti-plugin-types | fixed | confirmed-fixed | ✅ |
| 8 | CLAP-C1 | tutti-clap-host | fixed | confirmed-fixed | ✅ |
| 9 | CLAP-C2 | tutti-clap-host | fixed | confirmed-fixed | ✅ |
| 10 | CLAP-H2 | tutti-clap-host | fixed | confirmed-fixed | ✅ |
| 11 | CLAP-H3 | tutti-clap-host | fixed | confirmed-fixed | ✅ |
| 12 | CLAP-H5 | tutti-clap-host | fixed | confirmed-fixed | ✅ |
| 13 | CLAP-H6 | tutti-clap-host | fixed | confirmed-fixed | ✅ |
| 14 | CLAP-L6 | tutti-clap-host | fixed *(deviates from audit)* | confirmed-fixed | ✅ |
| 15 | VST3-C1 | tutti-vst3-host | fixed | confirmed-fixed | ✅ |
| 16 | VST3-C2 | tutti-vst3-host | fixed | confirmed-fixed | ✅ |
| 17 | **VST3-H1** | tutti-vst3-host | fixed | fixed-but-regressed | ⚠️ **PARTIAL** |
| 18 | VST3-H2 | tutti-vst3-host | fixed | confirmed-fixed | ✅ |
| 19 | VST3-H3 | tutti-vst3-host | fixed | confirmed-fixed | ✅ |
| 20 | VST3-H5 | tutti-vst3-host | fixed | confirmed-fixed | ✅ |
| 21 | AU-C1 | tutti-au-host | fixed | confirmed-fixed | ✅ |
| 22 | **AU-H2** | tutti-au-host | fixed | **unverifiable** | ⚠️ **PARTIAL** |
| 23 | AU-H1 | tutti-plugin-server | fixed | confirmed-fixed | ✅ |
| 24 | AU-H3 | tutti-au-host | fixed | confirmed-fixed | ✅ |
| 25 | VST2-C1 | tutti-vst2-host | fixed | confirmed-fixed | ✅ |
| 26 | VST2-C2 | vst-tutti | fixed | confirmed-fixed | ✅ |
| 27 | VST2-C3 | vst-tutti | fixed | confirmed-fixed | ✅ |
| 28 | VST2-H1 | tutti-vst2-host | fixed | confirmed-fixed | ✅ |
| 29 | VST2-H2 | tutti-vst2-host | fixed | confirmed-fixed | ✅ |
| 30 | **VST2-H4** | vst-tutti | fixed | fixed-but-regressed | ⚠️ **PARTIAL** |
| 31 | VST2-H5 | vst-tutti | fixed | confirmed-fixed | ✅ |
| 32 | **DISC-C1** | tutti-plugin | fixed | fixed-but-regressed | ⚠️ **PARTIAL** |
| 33 | DISC-C2 | tutti-plugin | fixed | confirmed-fixed | ✅ |
| 34 | DISC-H5 | tutti-plugin | fixed | confirmed-fixed | ✅ |
| 35 | DISC-H6 | tutti-plugin | fixed | confirmed-fixed | ✅ |
| 36 | DISC-H7 | tutti-plugin | fixed | confirmed-fixed | ✅ |
| 37 | DISC-H1 | tutti-plugin | fixed | confirmed-fixed | ✅ |
| 38 | DISC-H2 | tutti-plugin | **deferred** | confirmed (honest defer) | ✅ |
| 39 | DISC-H8 | tutti-plugin-host | fixed | confirmed-fixed | ✅ |
| 40 | DISC-M1 | tutti-plugin-host | fixed *(partially decayed)* | confirmed-fixed | ✅ |
| 41 | DISC-L1 | tutti-plugin | fixed | confirmed-fixed | ✅ |

### The disagreements, explained

**Rows 1–3 (GATE-2 / TRANSPORT-C1 / TRANSPORT-C1-RETUNE) — one story, not three.**
These are sequential edits to the *same* code, and their narratives contradict each other
and the shipped diff:

- **GATE-2** reported itself as *two intentionally failing repro tests*, verbatim
  `0 passed; 2 failed`, against a "97 passed" baseline. That snapshot is **stale** — the
  tree contains the full production fix and the measured baseline is **117 passed**.
- **TRANSPORT-C1** claims flakiness was cured by a **measured 1 ms floor**, "verified 30/30
  consecutive green". I grepped the code: **`MIN_PROCESS_WAIT`/`MAX_PROCESS_WAIT` do not
  exist.** `PROCESS_WAIT_FRACTION = 2` is the only sizing input, and two tests were added
  specifically to *prevent* a floor being reintroduced.
- **TRANSPORT-C1-RETUNE** then *deliberately deleted* that floor and reported
  "117 passed, 1 failed … both bypass tests pass".

So a floor was added, then removed, and TRANSPORT-C1's evidence was never updated. The net
shipped behaviour is the retune's (no floor). **I measured both regimes:** idle 20/20 pass;
under 24× CPU load 4/12 pass. The two earlier verifiers measured 19/20 and 29/40 failures on
loaded machines — their numbers were right; they just did not identify **load as the
discriminator**, which is why the three reports look irreconcilable. They are not: the
correctness fix is real in all of them, and only the timeout tuning is wrong.

**Row 6 (SHARED-M1)** — the doc correction is right and valuable, but `ParameterInfo::to_plain`
/`to_normalized` have **zero call sites repo-wide**. The AU agent hand-rolled its own
`ParamBounds::to_plain` in `au.rs`, directly against the new doc's "do not hand-roll"
instruction. Both copies **propagate NaN**, and `au.rs` feeds wire-supplied `point.value`
through it into a live `AudioUnitSetParameter`. Untested. See R3.

**Row 17 (VST3-H1)** — macOS half is correct and verified against a real SpectraLayers bundle
(it now logs its own bundle path and runs its licence lookup, proving the entry point had
been skipped). The **Linux/BSD half does not compile**. See R2.

**Row 22 (AU-H2)** — logic verified, **call site is not**. Deleting both `check_sample_rate`
call sites leaves all 40 tests green. Apple's AUDelay accepts every rate offered (probed
0.5 Hz → 1 MHz), so no installed AU can drive the rejection branch. Honestly disclosed by the
fixer in a test doc comment rather than left looking like coverage.

**Row 30 (VST2-H4)** — the `can_replacing` flag guard works. The **null-pointer guard does not
survive optimization**. See R4.

**Row 32 (DISC-C1)** — crash/timeout blacklisting works and was traced to real production
error paths. But **`BridgeError::LoadFailed` still writes nothing to the catalog**. See R5.

**Row 14 (CLAP-L6) — a deviation worth knowing about, not a disagreement.** The audit called
"mid-scale reads as −6 dB" the bug; for a linear-amplitude gain, 0.5 **is** −6.02 dB, so the
audit was wrong. The fixer anchored MIDI full-scale to CLAP unity 1.0 instead of rescaling to
4.0 — which would have moved unity to 0.25 and attenuated every ordinary value by 12 dB.
Correct call, and stated plainly rather than quietly complied with.

---

## 3. Landed

### tutti-plugin — out-of-process audio + discovery
- **Audio bypass killed.** `buffer_id` is now echoed end-to-end (`ProcessAudioData` →
  `Session::handle_process` → `AudioProcessed` → `await_response`), echoed on **both** reply
  paths including the degenerate no-shm one. The host reads the slab only after *that block's*
  reply arrives, so a block's output can never be another block's audio nor the input echoed
  back. `PROTOCOL_VERSION` 2→3 (bincode is not self-describing, so skew now fails at
  handshake). The 100 µs sleep-poll was replaced with park/unpark.
  *Guarded by:* `plugin_process_returns_current_block`,
  `plugin_process_steady_state_is_not_stale_or_echoed` — deliberately **separate** tests, so a
  partial fix that only removes the one-block lag cannot look green.
- **Wait budget** derived per block as period/2, no clamp (see R1 — this is the regression).
  *Guarded by:* `budget_is_always_under_the_block_period` (12-config matrix) + 4 more.
- **Discovery:** crash/timeout probes now blacklist (`ProbeFailure::from_bridge_error`, a real
  production classifier, not a test mirror); blacklist gained an inverse
  (`unblacklist`/`clear_blacklist`/`blacklisted()`) plus an mtime escape hatch so reinstall
  re-admits; catalog flush is temp+fsync+atomic-rename with corrupt DBs quarantined to
  `.corrupt` instead of silently overwritten; arch subdir keys on `target_arch`
  (aarch64-linux, arm64-win — the old code hardcoded `x86_64-*` and **the test helper
  hardcoded the same wrong strings**, so the suite passed on ARM while every real bundle
  failed); one `FORMAT_BY_EXTENSION` table with a const-assert so the extension lists cannot
  drift; `.VST3`/`.CLAP` matched case-insensitively; VST2 `.dll`/`.so` discoverable at all.
  *Guarded by:* 15 tests. The const-assert was mutation-tested (adding an entry without
  bumping `N` → `error[E0080]`).

### tutti-plugin-types
- `with_loop` sets the `_quarters` pair (VST2/VST3 read *those*; a 0..0 loop was being
  asserted as real via the unconditional cycle-valid bit).
- `TransportPosition::samples` → `Option<i64>`; the type change surfaced a **third bug** the
  audit missed — CLAP `song_pos_seconds` was `samples/sample_rate` ≡ 0 forever, even though a
  correct `position.seconds` was already computed.
- `ParameterQueue` hand-written `Deserialize` → `normalize()` (clamps negative offsets,
  stable-sorts). Verified reachable: VST3 `getPoint` and CLAP hand the offset to the plugin
  verbatim, so an unsorted queue is a ramp that jumps backwards and a negative offset is a
  sample index before the buffer start.
- *Guarded by:* 13 tests. The vst3 fixture moved off struct literals onto `with_*`
  constructors, so 5 existing assertions now exercise the constructor that was broken.

### tutti-clap-host — all 7, none decayed
Thread-check roles made mutually exclusive via a Mutex-backed RAII `AudioThreadClaim`
(C1/C2 share one root fix, grounded in the canonical `thread-check.h` wording); note-on/off/
expression now share a `note_id`; event times **clamped, not dropped** (both classes carry
state — a dropped NOTE_OFF is a stuck note, a dropped PARAM_VALUE is a permanently stale
parameter); `PluginHandle` bound *before* the init checks so `destroy` covers every exit;
entry registry keyed by `Weak<LiveEntry>` so a dlclose'd image re-inits; per-note VOLUME
mapped into CLAP's gain range.
*Guarded by:* 14 tests including an **end-to-end FFI test** against a real dlopened plugin.

### tutti-vst3-host
Note-expression ids now read from the SDK enumerators (Expression was dropped entirely and
Brightness mislabelled onto its id); dangling `DataEvent.bytes` fixed at the **root** by
deleting the scratch buffer rather than reserving capacity; `bundleEntry`/`ModuleEntry`/
`InitDll` called with paired exit; `setHostContext` + unicode class info; `kContTimeValid`
now set (the continuous-clock feature was entirely dead); ParamID vs index namespaces split.
*Guarded by:* 127 lib tests. C2's test was **proven real** by re-introducing the old scratch
(it failed with a freed-memory read), then restoring.

### tutti-au-host / tutti-plugin-server
`AudioBufferList` sized from `offset_of!` and 8-aligned via `Box<[u64]>` — the old formula
under-allocated 4 bytes (ABI confirmed by compiling a C probe against the real SDK header);
AU automation denormalized through a load-time range cache (AU takes plain units, not 0..1 —
so writing 1.0 to a [10, 22050] cutoff set **1 Hz**); sample-rate set is now read back and
verified; render callback wrapped in `catch_unwind` and bounded by the AU's own
`mDataByteSize`.
*Guarded by:* 40 tests, each **mutation-tested by reverting the fix**.

### tutti-vst2-host / vst-tutti
`effClose` now dispatched and `dlclose` leaked — previously exactly backwards (matches
JUCE/Ardour); `Mutex` removed from the audio-thread callback (every `Host` method already
took `&self`, so it bought nothing but a lock and a poison-panic path); 3 panics plus **3
latent UB sites** fixed (`assume_init()` on a buffer an *optional* opcode may never write; a
`match` on an out-of-range `#[repr(i32)]` discriminant); `catch_unwind` on both `extern "C"`
entry points; `TRANSPORT_CHANGED` is now an **edge not a level** (verified against JUCE and
Ardour); VALID flags gated on field usability.
*Guarded by:* 29 + 22 tests. **`cargo test -p vst-tutti` never compiled before this run** —
a missing `rand` dev-dep and 12 broken doctests meant 34 tests had never executed once.

---

## 4. Not landed

| Item | Status | Blocking reason / pickup notes |
|---|---|---|
| **DISC-H2** — catalog file locking | Deferred (honest) | Needs `fs2`/`fd-lock` (new cross-platform dep) or cfg'd `flock`/`LockFileEx`, **plus a policy call**: should a locked DB block or fail the scan? Likely changes `PluginCatalog::flush`'s signature. `Plugins::rescan()` (`plugins.rs:117,140`) builds a *second* `JsonCatalog` over the same `db_path`, so two live catalogs clobber last-writer-wins. DISC-H1's atomic rename removed the *torn-file* mode but **not lost updates**. |
| **SHARED-M1** — `NormalizedValue`/`PlainValue` newtypes | Declined this pass | The `f64` crosses bincode IPC, `PluginGui::set_parameter`, the RT `set_parameter_rt` path, tutti-wasm-plugin and the in-process VST2 backend — ~8 crates, 4 owned by concurrent agents. Would have collided. The doc contract is now precise enough to mechanize later. |
| **AU-H1 follow-through** | Partially landed | AU loader denormalizes correctly, but via a *hand-rolled* `ParamBounds::to_plain` rather than the shared `ParameterInfo::to_plain`. Consolidate — but fix NaN (R3) in both first. |
| **VST3-H1 Linux/BSD** | **Broken — see R2** | `platform::enter` does not compile off-macOS. |
| **VST3-H5** — `ParamId(u32)` newtype | Not attempted | Would break `tutti-plugin/src/format/gui/vst3.rs:62`, outside this crate's scope. Names/docs were corrected instead and index-addressed methods added alongside. |
| **`TransportPosition::samples` producer** | Open | Now honestly `None`, so VST2 `samplePos` / VST3 `projectTimeSamples` are still 0 for every plugin — the type makes the gap **visible** but does not close it. Needs a real project-time sample clock on `tutti_core::transport::TransportState` (distinct from the existing monotonic `steady_time`). TODOs left at both forwarding sites. |
| **`samplesToNextClock` / `systemTime`** | Open | No producer; flags correctly left **clear** rather than claiming time zero. TODOs in place. |
| **CLAP conformance thread-check coverage** | Open | The reference plugin's `ProcessCapture` records no thread-check state, so C1/C2 could only be tested at the host vtable level, not end-to-end. |
| **AU per-scope parameters** | Pre-existing limit | All AU param access is hard-wired to `kAudioUnitScope_Global` / element 0; the new range cache inherits that. A per-bus AU would need per-scope keys. |

---

## 5. Regressions

### R1 — RT wait budget is unmeetable under load *(highest severity)*
`PROCESS_WAIT_FRACTION = 2` with **no floor**. `BATCH_SIZE` is hardcoded 64
(`batcher.rs:26`, matching fundsp `MAX_BUFFER_SIZE`) and is **not host-configurable**, so
production budgets are 726 µs @44.1k, **667 µs @48k**, 333 µs @96k, **167 µs @192k** — all
below the ~0.7–1 ms scheduler wake latency the code's own comment documents. The round trip
needs two scheduler wakeups plus a blocking socket hop.

**Measured by me on this machine:**

| Condition | Result |
|---|---|
| Idle | **20 / 20 pass** |
| Under 24× `yes` CPU load | **4 / 12 pass** |

**Failure mode is benign** — captured directly, not inferred:
```
block 2: got silence (process returned false — no reply was waiting)
block 4 ch 0 sample 0: expected 82, got 0 — silence (process returned false — no reply was waiting)
```
Zero `UNITY gain`, zero `PREVIOUS block` across every observed failure. **Silence, never
wrong audio.** HEAD previously had no bound at all and shipped audible full bypass, so this
is a net improvement — but the *asserted* RT guarantee is false and the two tests certifying
the fix are load-flaky.

The unit tests cannot catch this: they assert `budget < period` and `budget == period/2`,
both true **by construction**. They say nothing about whether the budget is *achievable*.
This is the repo's own recorded "test at real call-site args" lesson recurring.

Compounding: `await_response` busy-spins with `yield_now` **on the audio thread**, and fundsp
runs nodes serially, so N stalled plugins spin N × budget inside one callback — the very
overrun the design claims to prevent. And `dispatch.rs::PROCESS_TIMEOUT` is **500 ms**,
~750× the audio-thread budget, so the bridge thread stays blocked in `recv_reply` and stops
draining commands — turning one slow reply into a *run* of silent blocks.

### R2 — VST3-H1 breaks the Linux/BSD build
`module_entry.rs`, `cfg(all(unix, not(target_os = "macos")))`:
`let unix: &UnixLibrary = library.as_ref();` — `libloading::Library` has **no `AsRef` impl**;
`os::unix::Library::into_raw(self)` consumes a non-`Copy` value. E0599 when cross-checked
against `x86_64-unknown-linux-gnu`. Invisible to the macOS-only check below. Also
semantically wrong: `into_raw` transfers ownership and `mem::forget`s the `Library`, which
would leak the handle and defeat the paired-unload design the rest of the change is built on.
**The crate compiled on Linux before this change.**

### R3 — NaN propagates into live plugin parameters
`ParameterInfo::to_plain` and the live `ParamBounds::to_plain` (`au.rs:334`) both propagate
NaN — `f64::clamp` returns NaN for a NaN input. The doc asserts "this never produces a value
outside what the plugin declared"; that is false. Wire-supplied `point.value` reaches
`AudioUnitSetParameter` on the audio path, so a hostile or buggy IPC peer can drive a NaN
into a live AU parameter. No test covers it.

### R4 — VST2-H4 null guard is compiled out under `-O`
`ProcessProc` is a non-nullable `extern "C" fn`, so `(replacing as *const u8).is_null()` is
foldable to `false`. A minimal repro traps (exit 133) under `rustc -O` and works unoptimized.
The `can_replacing` **bool** guard — which covers the realistic case of a plugin clearing
`effFlagsCanReplacing` — does work. Pre-existing idiom (`host.rs:684` does the same) and
`#![allow(useless_ptr_null_checks)]` was already at HEAD, so this is not newly introduced —
but the claim overstates what ships. Fix: type the field `Option<ProcessProc>`.

### R5 — DISC-C1 `LoadFailed` still writes nothing
Non-crash / non-timeout probe failures produce **no catalog upsert**, so `needs_rescan`
stays true forever and those plugins are re-probed at full subprocess-spawn cost on **every**
scan — verbatim the failure mode the fix's own doc comment says it eliminated. It is also
the case the fix cites JUCE for (a scan yielding nothing → `failedFiles` → `addToBlacklist`).

### Minor
- CLAP `song_pos_seconds` uses `if transport.position.seconds != 0.0` — a 0.0-means-absent
  sentinel, one line below the `Option` that removed exactly that anti-pattern. Harmless
  today (both branches yield 0.0); mis-reports **true song-position-zero** the moment any
  host supplies a real `samples` clock.
- VST3-H1 probes all 4 Windows ARM subdirs unconditionally; the SDK gates the arm64x/x86_64
  fallbacks on `SMTG_CPU_ARM_64EC`. A liberalization, not a break (wrong-arch picks fail
  later at `LoadLibrary`).
- `PROTOCOL_VERSION` v3 changelog omits the `Option<i64>` transport wire break (covered
  incidentally by the `buffer_id` bump).
- Stale comment in `ClapLoaded::drop`: "EntryGuard::Drop is a no-op" — H6 made that false.
- CLAP `HostState::audio_thread_id` is still `pub`, so a caller could bypass
  `claim_audio_thread`. Every in-crate writer goes through the claim; narrowing it is a
  public-API break.
- H6 opens a narrow new race (EntryGuard drops before `_library`), far narrower than the bug
  it replaces; dead `Weak` entries are never evicted (bounded by distinct plugin paths).

---

## 6. Test posture

### No test was weakened — audited mechanically, not taken on trust

```
removed  #[test]        : 1     (refactored into a shared helper, NOT deleted)
added    #[ignore]      : 0
removed  assert lines   : 2
added    assert lines   : 278
```

All three removals are legitimate:

1. The removed `#[test]` in `tutti-vst2-host/src/time_info.rs` was **refactored**, not
   dropped — that file's test count went **2 → 10**, and both original tests
   (`flags_recording_and_cycle_active`, `flags_stopped_no_cycle`) still exist at lines 148
   and 180.
2. The removed `debug_assert!` in `clap-host/params.rs` **was CLAP-C2 itself** — it compiled
   out in release and, in debug, compared against the very id C1 had just set (tautological).
3. The removed assert in `vst3-host/events.rs` compared the lookup table **against itself**;
   replaced with the spec's absolute id (`== 5`).

Three tests were **strengthening rewrites of tests that had been pinning the bug**: one
asserted a table against itself, one asserted a transport *field* but never its *flag*, and
two iterated indices into ParamID slots. Correcting these to assert the spec is
strengthening, and each is called out per-finding above rather than buried.

Two `#[ignore]`d suites were **run explicitly** with `--ignored` against real installed
plugins (clap-host no-alloc: 2 passed; au-host no-alloc: 1 passed), confirming the new RT
paths stay allocation-free.

### Counts, verbatim

| Crate | Result |
|---|---|
| **tutti-plugin** (lib) | `test result: FAILED. 117 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out` |
| tutti-plugin-types | `ok. 18 passed; 0 failed; 0 ignored` |
| tutti-plugin-server | `ok. 101 passed; 0 failed; 2 ignored` · doc `ok. 1 passed; 0 failed` |
| tutti-clap-host | lib `ok. 48 passed; 0 failed` · conformance `ok. 7 passed; 0 failed` · no-alloc `ok. 0 passed; 0 failed; 2 ignored` · unit `ok. 98 passed; 0 failed` · doc `ok. 0 passed; 3 ignored` |
| tutti-vst3-host | lib `ok. 127 passed; 0 failed` · integration `ok. 0 passed; 10 ignored` · no-alloc `ok. 0 passed; 2 ignored` |
| tutti-au-host | lib `ok. 40 passed; 0 failed` · no-alloc `ok. 0 passed; 1 ignored` · doc `ok. 1 passed` |
| tutti-vst2-host | lib `ok. 29 passed; 0 failed` · integration `ok. 1 passed; 13 ignored` · no-alloc `ok. 0 passed; 2 ignored` · doc `ok. 1 passed` |
| tutti-plugin-host | `ok. 3 passed; 0 failed; 0 ignored` |
| vst-tutti | lib `ok. 22 passed; 0 failed` · doc `ok. 12 passed; 0 failed` |

### The one failure is pre-existing
```
---- host::discovery::scanner::tests::probe_real_plugin stdout ----
TAL-NoiseMaker should report a synth class, got Vst3 { category: "" }
```
Reproduced at pristine HEAD in a clean worktree: **`97 passed; 1 failed`**, same test, `got
Unknown`. The observed value changed only because the plugin-server binary now builds so the
real probe path runs at all. **The test's premise may itself be wrong**: it asserts a *VST2*
`Vst2Category::Synth` for a plugin its candidate list picks as *VST3*. Whoever owns it should
decide whether to rewrite it against the VST3 subcategory vocabulary or gate it on the plugin
being installed — not silently ignore it.

### `#[ignore]`d for lack of installed plugins (all pre-existing, none added this run)
- tutti-vst3-host: **10** integration + **2** no-alloc
- tutti-vst2-host: **13** integration + **2** no-alloc
- tutti-clap-host: **2** no-alloc + **3** doctests
- tutti-au-host: **1** no-alloc
- tutti-plugin-server: **2**

⚠️ **Caveat on the CLAP conformance suite:** it uses a `load_or_skip` harness that **silently
skips** (printing a message and returning `Ok`) when the reference plugin dylib is not built.
A green `7 passed` therefore proves nothing unless `tutti-clap-test-plugin` was built first.
It passes genuinely here, but in CI as configured it could pass vacuously.

---

## 7. Follow-ups, ranked

### ① HIGHEST VALUE — generalize `clap_conformance.rs` into a format-parametrized suite over `&mut dyn PluginFormatHost`

Those tests run against a **real dlopened plugin through the real FFI** and are the
best-tested corner of the layer — and they test **CLAP, the format that is usually right**.

Every cross-format disagreement in this audit was one format silently deviating from the
others: note-expression ids (VST3 had Expression/Brightness swapped), note-id pairing (CLAP
hardcoded −1), event-time clamping (CLAP wrapped negatives to 4.29 billion), normalized-vs-
plain params (AU was the outlier), transport VALID flags (VST2 asserted them
unconditionally), loop `_quarters` vs `_beats`. **A shared conformance suite run against
every format would have caught nearly all of them mechanically**, and would give AU-H2 and
VST3-H5 the coverage they currently cannot have.

The reference test plugin already exists (`tutti-clap-test-plugin`). The work is a
trait-object harness plus per-format reference plugins or gated real-plugin fixtures.
Fix the `load_or_skip` silent-skip at the same time so a missing fixture fails loudly.

### ② Fix R1 — reconcile the two timeouts
667 µs (audio thread) vs 500 ms (dispatch) is a ~750× mismatch. Either reduce bridge-thread
wake latency (RT priority / busy-wait handoff) so period/2 is actually achievable, or accept
a floor and document that a plugin exceeding its block period is *structurally* incompatible
with small buffers. **Do not simply widen the budget past the period** — that trades one
plugin's silence for a graph-wide overrun. Also align `PROCESS_TIMEOUT` so one slow reply
does not cascade into a run of silent blocks.

### ③ Fix R2 — Linux build
Small and mechanical; the crate is currently broken off-macOS. **Add a Linux CI target** so
`cfg`-gated code cannot rot invisibly again — that is the durable half of this fix.

### ④ Fix R3 — NaN
Guard both `to_plain` copies with `is_finite()`, add the test, then consolidate `au.rs` onto
the shared helper (which closes the SHARED-M1 dead-code gap too).

### ⑤ Close DISC-C1's `LoadFailed` gap
Upsert on every probe failure, blacklistable or not, so `needs_rescan` clears.

### ⑥ DISC-H2 file locking — needs the block-vs-fail policy decision first.

### ⑦ VST2-H4 — type the field `Option<ProcessProc>` so the niche makes `is_none()` real.

### ⑧ Surface the blacklist in dawai *(out of scope here)*
`Plugins::{blacklisted, unblacklist, clear_blacklist}` and `ScanResult::newly_blacklisted`
now exist with **no UI**, so a plugin auto-blacklisted by DISC-C1 is invisible to the user
unless they happen to reinstall it.

---

## 8. Verbatim `cargo check`

```
$ cargo check --manifest-path /Users/pohsuanlai/Documents/dawAI/dawai-plugin-audit/crates/bevy-tutti/Cargo.toml \
    -p tutti-plugin -p tutti-plugin-types -p tutti-plugin-server -p tutti-clap-host \
    -p tutti-vst3-host -p tutti-au-host -p tutti-vst2-host -p tutti-plugin-host

    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.16s
EXIT=0
```

Cold build, same invocation:
```
    Checking tutti-clap-host v0.1.0 (.../formats/tutti-clap-host)
    Checking tutti-vst3-host v0.1.0 (.../formats/tutti-vst3-host)
    Checking tutti-plugin v0.0.1 (.../tutti-plugin)
    Checking tutti-au-host v0.0.1 (.../formats/tutti-au-host)
    Checking tutti-vst2-host v0.0.1 (.../formats/tutti-vst2-host)
    Checking tutti-plugin-server v0.0.1 (.../tutti-plugin-server)
    Checking tutti-plugin-host v0.0.1 (.../tutti-plugin-host)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.20s
```

**PASSES — EXIT 0, zero warnings**, on macOS aarch64.

⚠️ This check **cannot** surface regression R2, which is `cfg`-gated to non-macOS Unix. A
Linux target must be added before this green result can be trusted as cross-platform.
