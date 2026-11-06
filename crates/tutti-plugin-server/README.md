# tutti-plugin-server

The `plugin-server` subprocess behind Tutti's out-of-process plugin hosting.

## What this is

[`tutti-plugin`](https://docs.rs/tutti-plugin) spawns one `plugin-server`
process per loaded plugin. This crate is that process: it hosts one VST2, VST3,
CLAP or AU plugin in isolation, speaks [`tutti_plugin::server`]'s wire protocol
over a Unix socket (a named pipe on Windows), and moves audio through a
shared-memory slab. A plugin that crashes takes down this process and nothing
else.

Most users never call this library. They build the binary and let
`tutti-plugin` find it:

```text
cargo build -p tutti-plugin-server
```

`tutti-plugin` looks for `plugin-server` in the `TUTTI_PLUGIN_SERVER`
environment variable, next to the running executable, in its parent directory,
and on `PATH`. `bevy-tutti`'s plugin features depend on this crate so the binary
is built alongside the app.

## Running a server yourself

The library has one entry point, [`PluginServer`]. The host chooses the socket
path and passes it in; [`run`][`PluginServer::run`] binds it, serves one host
and returns when the host disconnects or asks for shutdown.

```rust,no_run
use tutti_plugin_server::{BridgeConfig, PluginServer};

// `max_buffer_size` is in frames and sizes the shared-memory slab, so a later
// block may not exceed it. The other fields keep their defaults.
let config = BridgeConfig {
    socket_path: std::env::args().nth(1).expect("socket path").into(),
    ..Default::default()
};

PluginServer::new(config)
    .expect("record parent pid")
    .run()
    .expect("session");
```

Always set `socket_path` to the path the host chose: `BridgeConfig::default`
derives a unique path per call, which could never meet the host that spawned
this process.

[`BridgeConfig`], [`BridgeError`] and [`Result`] are re-exported from
`tutti-plugin`; the wire protocol and its version are defined there too.

## How a session runs

The host connects twice. The first connection is the handshake: plugin load,
format negotiation and shared-memory setup. The second carries per-block audio
traffic for the rest of the session; the server raises that thread to realtime
priority (best effort; failure is logged) and sends plugin events to the host
after each block. Each connection opens with a `Ready` message carrying the
protocol version, and the host refuses a version it does not know.

Values crossing this boundary are plain numbers (a raw `f64` sample rate, for
example), as they are at the plugin formats' C ABIs.

If the host dies before connecting, the server notices on its own and exits.
It compares its current parent with the host PID recorded at startup (which
the host passes in `TUTTI_PLUGIN_HOST_PID`) on Unix, and checks whether the
parent process is still alive on Windows.

## Features

| Feature | Enables |
|---|---|
| `vst2` | VST2 plugins, through `tutti-vst2-host` |
| `vst3` | VST3 plugins, through `tutti-vst3-host` |
| `clap` | CLAP plugins, through `tutti-clap-host` |
| `au` | Audio Unit plugins, through `tutti-au-host` (macOS only; elsewhere it compiles to no AU support) |
| `all` | All four formats |

All four formats are on by default.

## Related crates

- [`tutti-plugin`](https://docs.rs/tutti-plugin): the host side, which spawns
  and talks to this process.
- [`tutti-plugin-types`](https://docs.rs/tutti-plugin-types): the plugin
  capability traits the format adapters here implement.
- The format hosts: [`tutti-vst2-host`](https://docs.rs/tutti-vst2-host),
  [`tutti-vst3-host`](https://docs.rs/tutti-vst3-host),
  [`tutti-clap-host`](https://docs.rs/tutti-clap-host) and
  [`tutti-au-host`](https://docs.rs/tutti-au-host).

## License

MIT OR Apache-2.0

[`PluginServer`]: crate::PluginServer
[`PluginServer::run`]: crate::PluginServer::run
[`BridgeConfig`]: crate::BridgeConfig
[`BridgeError`]: crate::BridgeError
[`Result`]: crate::Result
[`tutti_plugin::server`]: tutti_plugin::server
