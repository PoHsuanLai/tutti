# tutti-vst2-test-plugin

A reference VST2 plugin whose output is a **closed-form function of its input**,
used to test that `tutti-vst2-host` hands plugins the right samples in the right
slots at the right times — and that it survives plugins that lie.

It is ours. It is not an SDK sample, and it is not a usable audio effect.

## Why it exists

A commercial plugin cannot close this gap. A host that hands the plugin a
perfectly-formed `AEffect` call full of the *wrong audio* — swapped channels, an
input wired onto the wrong output, MIDI delivered at the wrong sample offset —
produces output that is finite, non-silent, and plausible. Nothing short of an
oracle catches it. And no commercial plugin will reliably misbehave on demand,
which is what testing the host's robustness requires.

## The oracle

```
out[ch][i] = in[ch][i] + channel_tag(ch)
```

`channel_tag(ch) = ch * 100 + 1`, so channel 0 gets `+1.0`, channel 1 `+101.0`,
channel 2 `+201.0`. The offsets dwarf any plausible audio content and are not
multiples of each other, so a host that swaps two channels, duplicates one
across both, or writes into the wrong slot produces arithmetically wrong samples
that name the guilty channel by inspection.

The constants are pinned as literals in **both** `config.rs`'s unit test and
`tests/vst2_probe_smoke.rs`. Writing the expectation as `in + channel_tag(ch)`
alone would make it move with any change to the function, so it could never
fail — coverage that does not exist. Change the formula and both fail; that is
the signal to re-check every mirrored constant.

## What it records

`ProcessCapture` (in `capture.rs`, `#[repr(C)]`) holds what the host actually
handed the plugin, read back through the exported `tutti_vst2_probe_capture`:

- block size, declared channel counts, and **which render entry point** ran
  (`processReplacing` vs the deprecated accumulating `process` vs the f64 path)
- the lifecycle the host drove: `effOpen`, `effSetSampleRate`,
  `effSetBlockSize`, and resume/suspend counts
- the `audioMasterGetTime` answer — presence, `samplePos`, `tempo`, `ppqPos`,
  `barStartPos`, time signature, and the **validity flag bitmask**, because a
  host that fills a field but leaves its `…Valid` flag clear has told the plugin
  to ignore the value it just supplied
- every MIDI event with its `deltaFrames`, in the order presented. VST 2.4
  requires `deltaFrames` be relative to the *current* block; a host forwarding
  an absolute timestamp is caught here and nowhere else.

The plugin and the test share one loaded image (the loader dedupes by path), so
a process-global behind a mutex is the simplest correct channel. It is
strictly test-only code.

## Misbehaviour

Everything above makes the probe a *well-behaved* oracle. It can also be a badly
behaved one, so the host can be tested against plugins that violate the spec.

### Runtime switches — `#[no_mangle] extern "C"`, in `switches.rs`

Call these across the dlopen seam on the image the host loaded. Reset with
`tutti_vst2_probe_reset_switches`.

| function | violation |
|---|---|
| `tutti_vst2_probe_set_can_do(answer, custom)` | `effCanDo` answers yes (1) / **don't-know (0)** / **explicitly-no (-1)** / an arbitrary integer |
| `tutti_vst2_probe_set_refuse_resume(true)` | the plugin declines to enter the resumed state and renders silence |
| `tutti_vst2_probe_set_silent_process(true)` | `process` returns without touching the outputs — the stale-buffer leak |
| `tutti_vst2_probe_set_write_extra_output(true)` | writes one channel past `numOutputs` — a genuine out-of-bounds write, off by default |
| `tutti_vst2_probe_set_read_extra_input(true)` | reads one channel past `numInputs` |

`can_do` is the important one. VST 2.4 defines **three** answers and hosts
routinely collapse them into two: `-1` means "explicitly no", not "unknown", and
it is also non-zero, so a host testing `!= 0` reads a refusal as consent.

### Construction-time metadata — `TUTTI_VST2_PROBE_*` env vars, in `config.rs`

`vst::main` calls `get_info()` synchronously while building the `AEffect`,
before `VSTPluginMain` has returned — so anything landing in the `AEffect` is
only reachable through the process environment. Same split the CLAP probe draws:
read-once construction state is an env var, everything a running plugin can
change its mind about is a switch.

| variable | effect |
|---|---|
| `INPUTS` / `OUTPUTS` | `numInputs` / `numOutputs` |
| `PARAMS` / `PROGRAMS` | declared `numParams` / `numPrograms` |
| `SERVICED_PARAMS` / `SERVICED_PROGRAMS` | how many it will actually answer for — set **below** the declared count for the enumeration hole |
| `LATENCY` | `initialDelay` |
| `IS_SYNTH` | `effFlagsIsSynth`, category `Synth` |
| `EDITOR` | `effFlagsHasEditor` |
| `NO_CHUNKS` | clears `effFlagsProgramChunks` |
| `F64` | `effFlagsCanDoubleReplacing` |
| `NO_CAN_REPLACING` | clears `effFlagsCanReplacing` while still being asked to process |
| `TAIL_SIZE` | raw `effGetTailSize` answer: `0`, `1`, and large all mean different things |

Set these before `Vst2Instance::load` and serialize against other tests —
`set_var` is process-global.

Two of these live outside the `Plugin` trait and are patched onto the `AEffect`
by this crate's hand-written `VSTPluginMain`:

- **`NO_CAN_REPLACING`** — `vst::main` sets `effFlagsCanReplacing`
  unconditionally, so the only way to ask "does the host fall back to the
  deprecated `process`, or call `processReplacing` anyway through a slot the
  plugin never promised?" is to clear the bit after the struct is built.
- **`TAIL_SIZE`** — vst-rs's dispatcher rewrites a trait-reported `0` into `1`,
  erasing exactly the distinction VST 2.4 draws: `0` = "no tail information,
  host must decide", `1` = "no tail at all, stop rendering immediately". A host
  that conflates them either truncates reverb tails or renders silence forever.
  `TAIL_SIZE` installs a raw dispatcher that answers verbatim.

### Two limitations, recorded rather than worked around

- **A refused resume cannot be signalled by return code.** `effMainsChanged` has
  no failure return in VST 2.4 — every host ignores the dispatcher's answer, and
  `Plugin::resume` returns `()` accordingly. So `set_refuse_resume` refuses *in
  substance*: the plugin stays suspended and renders silence, which is what a
  plugin whose device or licence claim failed actually does. `resume_count` in
  the capture still increments, so a test can separate "the host never resumed"
  from "the plugin declined".
- **The parameter hole is invisible in values.** `AEffect::getParameter` returns
  a bare `float` with no way to say "absent", so the probe's `None` collapses to
  `0.0` on the way out and the host reads `Some(0.0)` for every index in the
  hole. Assert on the *string* opcodes instead — an out-of-range
  `get_parameter_name` / `get_parameter_label` answers empty, and
  `can_be_automated` answers false.

## Why absence is a hard failure

`tests/support/probe_path.rs` **panics**, listing every path it searched. It
does not return `Option` and there is deliberately no `_or_skip` variant.

The probe is built from this tree by a dev-dependency edge, in the same
`cargo test` invocation, so its absence is a build failure — never a property of
the machine. Making it skippable would put the suite one typo away from
reporting success having executed nothing, which has happened here before: a
renamed VST3 probe made all nine of its tests skip **while printing
`test result: ok. 9 passed`**.

`probe_path` also resolves the **newest** of several candidates rather than the
first. Cargo writes the cdylib to `<profile>/deps/<name>` and hardlinks it up to
`<profile>/<name>` without always refreshing the uplifted copy, so an
interrupted build or a shared target dir can leave a stale file at the more
obvious path. Loading a stale probe silently reverses results: the switch the
test just set does not exist in the old image, so the plugin behaves well, and
the test asserting the host survives misbehaviour passes for entirely the wrong
reason.

## How it is built

Cargo builds it. `tutti-vst2-host` takes this crate as a dev-dependency
(`crate-type = ["cdylib", "rlib"]`), so the loadable artifact is produced by the
same `cargo test` invocation that runs the tests. `tutti-vst2-host/build.rs`
only computes where Cargo will have put it.

Shelling out to a nested `cargo build` from the build script instead would
**deadlock**: the outer `cargo test` holds the target-directory lock for the
whole build, and the nested `cargo` waits on that same lock forever. This
workspace shares one external target dir across worktrees, so the lock is always
contended.

```bash
cargo test -p tutti-vst2-host
```

## Adding a mode

1. Decide which half it belongs in. Can the value be read after the plugin is
   loaded? Switch. Does it land in the `AEffect`? Env var. When in doubt prefer
   the switch — an env var is process-global *and* sticky for the life of the
   image, so a test that forgets to unset one silently poisons every later test
   in the same binary.
2. Implement it, and add it to the table above.
3. **Verify the new assertion is non-vacuous** by mutating the probe (shift a
   tag, or force the switch to its off value unconditionally) and confirming the
   test fails.

Step 3 is not optional. The usual way to fail it is to assert only "didn't
crash, stayed finite" — which is true of a well-behaved plugin too, so the test
passes for the wrong reason. A test that cannot fail is worse than no test,
because it reports coverage that does not exist.

Two constraints on anything added here:

- **The probe must misbehave only as specified.** A crash *inside the plugin*
  reads as a host bug and wastes the reader's time. Guard defensively — that is
  why the out-of-range parameter accessors answer empty rather than panicking.
- **Never enable a crash-capable switch by default.**
  `set_write_extra_output` is a genuine out-of-bounds write; that is the
  finding, but only when a test asked for it.
