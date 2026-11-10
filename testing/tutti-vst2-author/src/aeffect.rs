//! The plugin-side accessors on `AEffect`: the plugin object and its `PluginCache` that
//! `main` leaves in the struct's `object` and `user` words.

use std::sync::Arc;

use tutti_vst2_ffi::{
    api::AEffect,
    editor::Editor,
    plugin::{Info, Plugin, PluginParameters},
};

use crate::cache::PluginCache;

/// The plugin-side view of an `AEffect` this crate created.
///
/// `AEffect` is the C struct the *host* owns and hands back to us on every
/// call, so these accessors have to conjure `&mut` out of `&self`
/// (`clippy::mut_from_ref`). That is upstream's design; the invariant that makes
/// it sound is the documented one on each method: the host serialises its calls
/// per instance.
#[allow(clippy::mut_from_ref)]
pub trait AEffectExt {
    /// Return handle to Plugin object. Only works for plugins created using this library.
    ///
    /// # Safety
    /// `self.object` must point at the `Box<Box<dyn Plugin>>` this library
    /// installed, and the caller must not invoke this concurrently with any
    /// other accessor on the same `AEffect`: the returned `&mut` aliases the
    /// same plugin every other entry point reaches.
    // Suppresses warning about returning a reference to a box
    #[allow(clippy::borrowed_box)]
    unsafe fn get_plugin(&self) -> &mut Box<dyn Plugin>;

    /// Return handle to Info object. Only works for plugins created using this library.
    ///
    /// # Safety
    /// `self.user` must point at the `PluginCache` this library installed.
    unsafe fn get_info(&self) -> &Info;

    /// Return handle to PluginParameters object. Only works for plugins created using this library.
    ///
    /// # Safety
    /// `self.user` must point at the `PluginCache` this library installed.
    unsafe fn get_params(&self) -> &Arc<dyn PluginParameters>;

    /// Return handle to Editor object. Only works for plugins created using this library.
    ///
    /// # Safety
    /// `self.user` must point at the `PluginCache` this library installed, and
    /// the caller must not invoke this concurrently with any other accessor on
    /// the same `AEffect`.
    unsafe fn get_editor(&self) -> &mut Option<Box<dyn Editor>>;

    /// Drop the Plugin object. Only works for plugins created using this library.
    ///
    /// # Safety
    /// Must be called exactly once, from `effClose`, after which neither
    /// `self.object` nor `self.user` may be dereferenced again.
    unsafe fn drop_plugin(&mut self);
}

#[allow(clippy::mut_from_ref)]
impl AEffectExt for AEffect {
    unsafe fn get_plugin(&self) -> &mut Box<dyn Plugin> {
        // SAFETY: the caller upholds this fn's `# Safety` contract: `object` is the
        // leaked `Box<Box<dyn Plugin>>` this library installed, with no concurrent accessor.
        unsafe { &mut *(self.object as *mut Box<dyn Plugin>) }
    }

    unsafe fn get_info(&self) -> &Info {
        // SAFETY: the caller upholds this fn's `# Safety` contract: `user` is the
        // `PluginCache` this library installed and it has not been dropped.
        unsafe { &(*(self.user as *mut PluginCache)).info }
    }

    unsafe fn get_params(&self) -> &Arc<dyn PluginParameters> {
        // SAFETY: the caller upholds this fn's `# Safety` contract: `user` is the
        // `PluginCache` this library installed and it has not been dropped.
        unsafe { &(*(self.user as *mut PluginCache)).params }
    }

    unsafe fn get_editor(&self) -> &mut Option<Box<dyn Editor>> {
        // SAFETY: the caller upholds this fn's `# Safety` contract: `user` is the
        // `PluginCache` this library installed, with no concurrent accessor.
        unsafe { &mut (*(self.user as *mut PluginCache)).editor }
    }

    unsafe fn drop_plugin(&mut self) {
        // SAFETY: the caller upholds this fn's `# Safety` contract: `object` came from
        // `Box::into_raw` of a `Box<Box<dyn Plugin>>` and is freed exactly once here.
        drop(unsafe { Box::from_raw(self.object as *mut Box<dyn Plugin>) });
        // SAFETY: as above for `user`, which came from `Box::into_raw` of a `PluginCache`.
        drop(unsafe { Box::from_raw(self.user as *mut PluginCache) });
    }
}
