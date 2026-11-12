//! The contract against AU, from Apple's AUDelay (there is no AU build of the
//! shared plugin).
#![cfg(target_os = "macos")]

tutti_plugin_contract::contract!(
    tutti_plugin_contract::Au,
    gaps = 24,
    hostings = [InProcess, OutOfProcess]
);
