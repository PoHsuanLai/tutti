# tutti-plugin-server

The **subprocess side** of Tutti's out-of-process plugin bridge.

## What this is

The implementation behind the `plugin-server` binary that
[`tutti-plugin`](../tutti-plugin) spawns once per loaded plugin. It hosts one
VST2 / VST3 / CLAP / AU plugin in isolation, speaks `tutti_plugin::server`'s
wire protocol over a Unix socket (named pipe on Windows), and moves audio
through a shared-memory slab.

Internally: `server` orchestrates the two-phase connection dance;
`session` is pure message-to-reaction dispatch and is unit-testable without
sockets; `audio_pipeline` owns the per-block machinery; `plugin` hides the
per-format `cfg`-gating behind one enum; `loaders::{vst2,vst3,clap,au}` are the
per-format `PluginInstance` adapters.

## Why it is its own crate

**`tutti-plugin` is the host side; this is the guest side.** They are two
processes, so they are two binaries, so they are two crates — a plugin that
segfaults takes down this process and nothing else, which is the entire point of
the split. The host never links a plugin SDK; this crate never links the audio
graph.

That asymmetry shows up in the dependency list: this crate owns the four format
hosts, `memmap2` for the slab, `interprocess` for the transport, and
`thread-priority` — the last because the host's audio callback runs on a thread
the OS backend created with realtime priority, while a subprocess we spawned
ourselves inherits ordinary priority and has to ask.

## Using it

Most callers want the `plugin-server` **binary**, not this library. The library
has exactly one entry point:

```rust,no_run
use tutti_plugin_server::{BridgeConfig, PluginServer};

let config = BridgeConfig {
    socket_path: "/tmp/tutti.sock".into(),
    ..Default::default()
};
PluginServer::new(config).unwrap().run().unwrap();
```

## Where it sits

Depends on `tutti-plugin` (for the protocol types and `BridgeConfig`, both
re-exported here), `tutti-core`, `tutti-midi-types`, and the four format host
crates. Only `bevy-tutti` names it, and only to make the binary available.

## Features

`default = ["vst2", "vst3", "clap", "au"]` — each pulls the matching format host
crate. `all` is the four together. `au` is macOS-only in practice.

## Orphan detection

If the host dies before connecting, this process must notice on its own. On Unix
an orphan is reparented to init, so `getppid() == 1` is the signal — no handle
and no host cooperation needed, which is the point: the case being handled is the
one where the host had no chance to cooperate. Windows neither has `getppid` nor
reparents orphans, so the parent PID is found by walking the process table and
then probed for liveness.

## License

MIT OR Apache-2.0
