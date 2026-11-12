//! A VST2 **shell** plugin: one file, two plugins.
//!
//! The protocol (`aeffectx.h`; `audioMasterCurrentId`, `effShellGetNextPlugin`): the host
//! loads the file once with `audioMasterCurrentId` answering `0`, and the plugin that comes
//! up is the shell itself (`kPlugCategShell`), which names its sub-plugins one by one at
//! `effShellGetNextPlugin` (the `uniqueID` returned, the name written, `0` at the end). The
//! host loads a sub-plugin by running `VSTPluginMain` again with `audioMasterCurrentId`
//! answering that `uniqueID`, and the plugin that comes up is that one.
//!
//! The sub-plugins differ in what a host reads of them (`uniqueID`, name, category, vendor),
//! so a host that loaded the wrong one, or described them all as the shell, shows. An id the
//! shell does not know creates [`ALPHA_ID`]'s plugin, as a shell with a default does: the
//! host has to check what came up.
//!
//! Nothing configures it: no environment variable, no switch.

extern crate tutti_vst2_author as vst;

use vst::api::{AEffect, HostCallbackProc};
use vst::host::Host as _;
use vst::plugin::{Category, HostCallback, Info, Plugin};

/// The shell's own `uniqueID` ("SHEL"): the plugin the host gets when it names none.
pub const SHELL_ID: i32 = i32::from_be_bytes(*b"SHEL");
/// The first sub-plugin ("SHAL"): an effect.
pub const ALPHA_ID: i32 = i32::from_be_bytes(*b"SHAL");
/// The second sub-plugin ("SHBE"): an instrument.
pub const BETA_ID: i32 = i32::from_be_bytes(*b"SHBE");

/// The shell's name, as `effGetEffectName` answers it.
pub const SHELL_NAME: &str = "Tutti VST2 Shell";
/// The first sub-plugin's name.
pub const ALPHA_NAME: &str = "Shell Alpha";
/// The second sub-plugin's name.
pub const BETA_NAME: &str = "Shell Beta";

/// What `effShellGetNextPlugin` walks, in order: each sub-plugin's `uniqueID` and name.
pub const SUB_PLUGINS: [(i32, &str); 2] = [(ALPHA_ID, ALPHA_NAME), (BETA_ID, BETA_NAME)];

struct Shell {
    /// What the host answered `audioMasterCurrentId`: `0` for the shell itself.
    current: i32,
    /// How many sub-plugins `effShellGetNextPlugin` has named.
    named: usize,
}

impl Plugin for Shell {
    fn new(host: HostCallback) -> Self {
        Self {
            current: host.get_plugin_id(),
            named: 0,
        }
    }

    fn get_info(&self) -> Info {
        let (name, unique_id, category) = match self.current {
            0 => (SHELL_NAME, SHELL_ID, Category::Shell),
            BETA_ID => (BETA_NAME, BETA_ID, Category::Synth),
            // Alpha, and the default of an id this shell does not know.
            _ => (ALPHA_NAME, ALPHA_ID, Category::Effect),
        };
        Info {
            name: name.to_string(),
            vendor: "Tutti".to_string(),
            unique_id,
            version: 1,
            inputs: 2,
            outputs: 2,
            category,
            ..Info::default()
        }
    }

    fn shell_next_plugin(&mut self) -> Option<(i32, String)> {
        // Only the shell itself names sub-plugins; a sub-plugin that is asked answers 0.
        if self.current != 0 {
            return None;
        }
        let (id, name) = *SUB_PLUGINS.get(self.named)?;
        self.named += 1;
        Some((id, name.to_string()))
    }
}

/// The VST2 entry point.
///
/// # Safety
///
/// `callback` must be the host's `audioMaster` function.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn VSTPluginMain(callback: HostCallbackProc) -> *mut AEffect {
    vst::main::<Shell>(callback)
}
