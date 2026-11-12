//! The contract against VST3 (Rust plugin), and what VST3 alone asks of a
//! host, against both VST3 plugins (`spec`).

mod spec;

tutti_plugin_contract::contract!(
    tutti_plugin_contract::Vst3,
    gaps = 1,
    hostings = [InProcess, OutOfProcess]
);
