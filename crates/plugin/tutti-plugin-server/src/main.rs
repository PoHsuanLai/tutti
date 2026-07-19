//! Plugin server binary. Spawned by DAW to host plugins in isolation.

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
