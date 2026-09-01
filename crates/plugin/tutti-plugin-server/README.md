# tutti-plugin-server

The **subprocess side** of Tutti's out-of-process plugin bridge.

## What this is

The implementation behind the `plugin-server` binary that
[`tutti-plugin`](../tutti-plugin) spawns once per loaded plugin. It hosts one
VST2 / VST3 / CLAP / AU plugin in isolation, speaks [`tutti_plugin::server`]'s
wire protocol over a Unix socket (named pipe on Windows), and moves audio through
a shared-memory slab.

## What it does not own

**The protocol.** The message shapes and `PROTOCOL_VERSION` are
[`tutti-plugin`](../tutti-plugin)'s; this crate imports them and never restates
their history. [`BridgeConfig`] and [`BridgeError`] are re-exported from there
rather than defined here.

**The format loaders.** VST2, VST3, CLAP and AU each have their own crate under
`../formats/`. What lives here is a thin `PluginInstance` adapter over each, so
`ClapInstance` and `Vst3Instance` in `loaders::` are **this crate's** adapter
types — not the format crates', whose own types are `ClapLoaded`/`ClapActive` and
`Vst3Loaded`/`Vst3Instance`.

**The audio graph.** The host never links a plugin SDK; this crate never links
the graph.

## Why it is its own crate

**`tutti-plugin` is the host side; this is the guest side.** They are two
processes, so they are two binaries, so they are two crates — a plugin that
segfaults takes down this process and nothing else, which is the entire point of
the split.

That asymmetry shows up in the dependency list: this crate owns the four format
hosts, `memmap2` for the slab, `interprocess` for the transport, and
`thread-priority` — the last because the host's audio callback runs on a thread
the OS backend created with realtime priority, while a subprocess we spawned
ourselves inherits ordinary priority and has to ask.

## Using it

Most callers want the `plugin-server` **binary**, not this library. The library
has exactly one entry point. `no_run`: [`run`][`PluginServer::run`] binds a socket
and blocks for the lifetime of the session.

```rust,no_run
use tutti_plugin_server::{BridgeConfig, PluginServer};

// The host chooses the rendezvous path and passes it in — a subprocess
// deriving its own could not meet the host that spawned it. Everything else
// defaults; `max_buffer_size` is denominated in FRAMES and sizes the slab,
// so a later block may not exceed it.
let config = BridgeConfig {
    socket_path: std::env::args().nth(1).expect("socket path").into(),
    ..Default::default()
};

// One server serves one host, then returns. A crash here takes the plugin
// down and leaves the host running — which is the point of the split.
PluginServer::new(config)
    .expect("record parent pid")
    .run()
    .expect("session");
```

`socket_path` is the one field with no safe default: `BridgeConfig::default`
derives a *unique* path per call precisely so a `..Default::default()` cannot
become a latent collision, in which the second bridge to bind unlinks the first's
live socket.

## The wire

Framing is a u32 big-endian length prefix plus a bincode payload. Both phases
open by sending `PROTOCOL_VERSION`, and a host that does not recognise the
version refuses.

This is an **IPC boundary, so the unit newtypes stop here**, as they do at the C
ABIs of the hosted plugin formats. A raw `f64` sample rate crossing the wire or
entering `AudioUnitSetParameter` is correct, not an omission.

## Internal layout

- `server` — outer shell ([`PluginServer`]); orchestrates the two-phase
  connection dance and drives a `Session` over a `Transport`.
- `session` — pure message-to-reaction dispatch. Owns plugin + shm + pipeline +
  editor state. Unit-testable without sockets.
- `audio_pipeline` — per-block audio machinery (scratch buffers, shared-memory
  I/O, plugin invocation).
- `plugin` — format-polymorphic wrapper; hides the VST2/VST3/CLAP/AU `cfg`-gating
  behind a single `Plugin` enum whose `load` dispatches on the file extension.
- `editor` — editor window state.
- `transport` — IPC framing; trait seam for testability.
- `loaders::{vst2, vst3, clap, au}` — per-format `PluginInstance` adapters.

## Orphan detection

If the host dies before connecting, this process must notice on its own. On Unix
an orphan is reparented to init, so `getppid() == 1` is the signal — no handle and
no host cooperation needed, which is the point: the case being handled is the one
where the host had no chance to cooperate. Windows neither has `getppid` nor
reparents orphans, so the parent PID is found by walking the process table and
then probed for liveness.

## Where it sits

Depends on `tutti-plugin` (for the protocol types and `BridgeConfig`, both
re-exported here), `tutti-core`, `tutti-midi-types`, and the four format host
crates. Only `bevy-tutti` names it, and only to make the binary available.

## Features

`default = ["vst2", "vst3", "clap", "au"]` — each pulls the matching format host
crate. `all` is the four together. `au` is macOS-only in practice: its loader is
additionally gated on `target_os = "macos"`, so enabling the feature elsewhere
compiles to no AU support rather than to a build error.

## License

MIT OR Apache-2.0

[`PluginServer`]: crate::PluginServer
[`PluginServer::run`]: crate::PluginServer::run
[`BridgeConfig`]: crate::BridgeConfig
[`BridgeError`]: crate::BridgeError
[`tutti_plugin::server`]: tutti_plugin::server
