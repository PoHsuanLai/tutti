//! Shared plugin-load phase label for the format-specific host crates.
//!
//! Each host crate (`tutti-vst2-host`, `tutti-vst3-host`, `tutti-clap-host`,
//! `tutti-plugin`) tags its load/init error variants with the phase that
//! failed, so callers can distinguish "file not found" from "plugin rejected
//! the sample rate" without parsing free-form messages.
//!
//! This is the superset of every format's phases; a given host uses only the
//! subset its ABI has. The doc comments record what each phase means per
//! format.

/// Labels the phase of plugin loading in which an error was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStage {
    /// Resolving the plugin bundle / file on disk before opening it.
    /// (VST3, and the higher-level `tutti-plugin` scan.)
    Scanning,
    /// Opening the dynamic library: `dlopen`/`LoadLibrary`, reading
    /// `clap_entry`, calling `VSTPluginMain`, or resolving a `.vst`/`.vst3`
    /// bundle to its inner binary.
    Opening,
    /// Retrieving the plugin factory from the entry: `GetPluginFactory` /
    /// walking `IPluginFactory` classes (VST3), the CLAP factory, or the
    /// VST2 `PluginLoader`.
    Factory,
    /// Creating the plugin instance from the factory:
    /// `IPluginFactory::createInstance` (VST3) / `clap factory create`.
    Instantiation,
    /// Initializing the instance: `IPluginBase::initialize` (VST3),
    /// `clap_plugin.init()` (CLAP), or the VST2 init/resume sequence.
    Initialization,
    /// Configuring audio processing: `IAudioProcessor::setupProcessing`
    /// (VST3). Not all formats expose a distinct setup phase.
    Setup,
    /// Activating the instance: `IComponent::setActive(1)` /
    /// `IAudioProcessor::setProcessing(1)` (VST3), `clap_plugin.activate()`.
    Activation,
}

impl core::fmt::Display for LoadStage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scanning => write!(f, "scanning"),
            Self::Opening => write!(f, "opening library"),
            Self::Factory => write!(f, "getting factory"),
            Self::Instantiation => write!(f, "creating instance"),
            Self::Initialization => write!(f, "initializing processor"),
            Self::Setup => write!(f, "setting up audio"),
            Self::Activation => write!(f, "activating"),
        }
    }
}
