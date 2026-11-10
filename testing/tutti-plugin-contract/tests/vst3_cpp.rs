//! The contract against VST3 (C++ plugin).

tutti_plugin_contract::contract!(
    tutti_plugin_contract::Vst3Cpp,
    gaps = 1,
    hostings = [InProcess, OutOfProcess]
);
