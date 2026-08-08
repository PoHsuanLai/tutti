# CLAP format-layer coverage — `tutti-clap-host` vs CLAP 1.2.10

Graded 2026-08-03 against [clap-1.2.10-interface-surface.md](clap-1.2.10-interface-surface.md),
which was extracted from the official `free-audio/clap` headers **before** any of our code
was read. That ordering is the point: a checklist derived from our implementation can only
ever report that we do what we already do.

**Scope: the format layer only** — `crates/tutti/crates/plugin/formats/tutti-clap-host/`.
It says nothing about whether the engine above can reach these capabilities. See
[the boundary note](#the-boundary-above-changes-what-user-visible-means) — for this format
that caveat is load-bearing, not boilerplate.

## Method

One survey pass, one adversarial refutation pass, plus independent re-checks at source.

| | Count |
|---|---|
| Stable extension IDs in the surface (28 IDs from 27 headers) | **28** |
| Draft extension IDs (20 IDs from 18 headers) | **20** |
| Stable BOUND | **24** |
| Stable PARTIAL | **4** |
| Stable ABSENT | **0** |
| Claims raised | 5 |
| **Refuted or misdescribed on adversarial review** | **4** |
| **Upheld** | **1** |

Every claim was handed to a second agent instructed to **refute** it, defaulting to refuted
unless the evidence was airtight. Four of five did not survive — the same ~60–80% attrition
the VST3 pass produced, and for the same reasons: accurate code citations attached to a spec
contract that does not exist, and impact inflated past what the header supports.

**No stable CLAP extension is entirely unbound.** That is the headline, and it is unusual —
the equivalent VST3 pass found `IUnitInfo` completely absent.

## Confidence tags

- **[verified]** — re-checked at source independently of the agents.
- **[claimed]** — survived adversarial review, not independently re-checked.

## Upheld findings

### `clap_host_thread_pool.request_exec` returns `true` without executing anything — **[verified] · FIXED on main**

> **Resolved.** `host_thread_pool_request_exec` now returns `false`, and the
> write-only `thread_pool_pending` field is gone. The analysis below is kept as
> the record of why — line numbers refer to the code as it was when surveyed.

`host/callbacks.rs:713-729` stores `num_tasks` into an atomic and returns `true`.

`ext/thread-pool.h:53-58` is unusually explicit about what that return value means:

> Schedule num_tasks jobs in the host thread pool.
> **Will block until all the tasks are processed.**
> Returns true if the host **did execute all the tasks**, false if it rejected the request.

So `true` is not "accepted" — it is "already done". This host returns immediately having
executed zero tasks.

The header also states the correct answer for a host with no pool, twice. Prose at
`thread-pool.h:9-11`: *"the host may provide @ref clap_host_thread_pool. If it doesn't, the
plugin should process its data by its own means."* And the reference `process()` at
`thread-pool.h:24-30` shows the plugin's fallback:

```c
bool didComputeVoices = false;
if (host_thread_pool && host_thread_pool->request_exec)
   didComputeVoices = host_thread_pool->request_exec(host, N);
if (!didComputeVoices)
   for (uint32_t i = 0; i < N; ++i)
      myplug_thread_pool_exec(plugin, i);
```

A plugin that trusts `true` skips that loop. **The plugin is behaving correctly — the header
instructs it to.**

There is no mitigating path. `thread_pool_exec` (`instance/polling.rs:803-812`) is
`#[cfg(feature = "clap-extras")]`, compiled out by default, and takes a caller-supplied
`task_index` that nothing ever supplies. `thread_pool_pending` has **one write
(`callbacks.rs:720`), one initializer (`host/state.rs:41,51`), and zero reads** — verified
by grep across the crate.

**What breaks:** for a plugin implementing `clap.thread-pool` and following the header's
example, `process()` returns with its voices never computed — silence or partial output from
that plugin. Not an xrun; the buffer is simply unfilled. A minority of plugins, but for those
it is total failure of output rather than degradation.

**Why it is a defect and not a scope boundary:** the extension is *advertised*.
`host/mod.rs:139` returns `HOST_THREAD_POOL` from `get_extension`, so plugins are told the
pool exists. Declining it is explicitly sanctioned — `thread-pool.h:37-38`: *"If the host
knows that it is running under hard real-time pressure it may decide to not provide this
interface."* Advertising and then no-op'ing is strictly worse than either alternative.

Cheapest correct fix: return `false`.

Note the same asymmetry from the other side — the host-side vtable is advertised
unconditionally (`host/mod.rs:139`) while its only plugin-side driver is feature-gated off.
Whichever way it is resolved, the two should agree.

## Refuted — recorded so they are not re-raised

**`set_scale` hardcoded to `1.0`** (`instance/polling.rs:268-270`) — **REFUTED**, and the
real bug is the inverse. `ext/gui.h:141-152` describes `set_scale` as an *override* of OS
info: *"Should not be used if the windowing api relies upon logical pixels… If the plugin
prefers to work out the scaling factor itself by querying the OS directly, then ignore the
call."* No "MUST call" language exists. Worse for the original claim, `gui.h:56-59` says of
cocoa: *"uses logical size, don't call clap_plugin_gui->set_scale()"* — and
`platform_window_handle` returns `CLAP_WINDOW_API_COCOA` on macOS (`polling.rs:34-40`). So on
this project's primary platform the correct behaviour is to **skip the call**, and the actual
(small) bug is that `polling.rs:118` makes it unconditionally. Passing `1.0` is inert. The
TODO at `polling.rs:266-268` correctly names the blocker as the `WindowHandle` carrying no
DPI — data that lives in the frontend, not this crate.

**`request_show` / `request_hide` return `true` while doing nothing**
(`host/callbacks.rs:268-274`) — **MISDESCRIBED**. `ext/gui.h:229-237` does define `true` as
success, so returning it unconditionally is contractually wrong and `false` is the correct
no-op answer. But the claimed impact does not hold: **this host owns no window.**
`open_editor` (`instance/polling.rs:250-289`) embeds into a `parent: WindowHandle` supplied
by the caller. Visibility of the containing window belongs to the frontend. There is no
"reveal" action being skipped. A one-line correctness fix with near-nil user impact.

**`is_rescan_flag_supported` blanket-`true` while `rescan` discards flags**
(`host/callbacks.rs:294-310`) — **REFUTED**. The asserted contract is inverted.
`ext/audio-ports.h:103-111` defines `is_rescan_flag_supported` as asking whether the host
*allows* a change; blanket `true` is the maximally permissive, fully legal answer. The
illegality the header names runs the other way — it is illegal for the *plugin* to rescan
with an unsupported flag, and since all flags are declared supported, no request can be
illegal. `rescan` is specified as *"Rescan the **full list** of audio ports according to the
flags"*: the flags narrow what you may skip, so discarding them yields a correct superset,
never a subset. The consumer confirms the full-rescan model
(`instance/polling.rs:480-483` is an edge-triggered consume-once boolean).
Residue worth noting: the `[!active]` qualifier on flags `1<<1`–`1<<5` is a
deactivation-ordering signal that is genuinely dropped — a separate, smaller question.

**Nine stable extensions unreachable because `clap-extras` is off** (`Cargo.toml:12-22`) —
**REFUTED as a defect; it is a documented decision.** **[verified]** — no crate in the repo
enables the feature; the only mention repo-wide is its own declaration. The feature comment
names the gated extensions, states the criterion (*"no consumer currently calls"*), records
the rationale (lean default build), and confirms code and tests stay compilable behind it.
The gated call sites carry matching per-item comments (`polling.rs:797,801`). The
verification that nobody enables it is the comment's stated premise, not a contradiction.

## The boundary above changes what "user-visible" means

Independently verified while grading: **`PluginFormatHost`
(`tutti-plugin-types/src/format_host.rs`) has no preset method of any kind** — the surface is
descriptor, loaded, process, set_sample_rate, get/set_parameter, set_automation_state,
get_parameter_list, get/set_state, open/close_editor, editor_idle.

CLAP's format layer *does* implement presets — `instance/state.rs:140` `load_preset()`,
`polling.rs:628` `poll_preset_loaded()`. So does AU, and so does VST3 since PR #138. None of
the three can expose that through the shared trait.

Nor is there a formatted-parameter-text surface for any format: grep for
`value_to_text|param_text|display_value` across `tutti-plugin-types/src/` and every
`tutti-plugin-server/src/loaders/` returns nothing, even though CLAP binds
`value_to_text` at `instance/params.rs:195`.

This is the `PluginTail` precedent again — populated by all four loaders and read by nothing
for a full release cycle. It means a capability can be correctly bound here and still be
unreachable from the DAW, and it changes fix ordering: **widening the trait comes before
binding more of any format**, or the result is another write-only capability.

## What this document is not

A bug list. One finding survived out of five raised. The other four are recorded above with
their refutations precisely so the next pass does not re-raise them — three of the four were
refuted by reading the CLAP header rather than the code, and the fourth by reading a comment
the authors had already written.
