//! The `plugin-server` binary — one process per hosted plugin.
//!
//! Spawned by the host (`tutti-plugin`), never run by hand: it takes the
//! host-chosen socket path as its sole argument and serves exactly one host,
//! then exits. Isolation is the point — a plugin that crashes takes this process
//! down and not the DAW.

use std::env;
use tutti_plugin_server::{BridgeConfig, PluginServer, Result};

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let socket_path = env::args()
        .nth(1)
        .expect("Socket path required as first argument");

    let config = BridgeConfig {
        socket_path: socket_path.into(),
        ..Default::default()
    };

    PluginServer::new(config)?.run()
}
