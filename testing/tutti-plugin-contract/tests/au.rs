//! The contract against AU, from the shared plugin's Audio Unit shell
//! (registered in-process; there is no served run, an in-process registration
//! being visible to this process only).
#![cfg(target_os = "macos")]

tutti_plugin_contract::contract!(
    tutti_plugin_contract::Au,
    gaps = 3,
    hostings = [InProcess, OutOfProcess]
);
