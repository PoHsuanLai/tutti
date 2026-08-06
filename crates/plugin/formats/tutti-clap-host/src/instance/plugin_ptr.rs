//! RAII wrapper for `*const clap_plugin` that calls `destroy()` on drop.
//!
//! Named for what it owns — a raw CLAP pointer — rather than `PluginHandle`,
//! which is `tutti_plugin`'s main-thread *control surface* (parameters, editor,
//! state) and a different thing entirely. The two never met in one scope, since
//! this type is `pub(crate)`, so the compiler never had to choose between them;
//! the cost was to a reader grepping across the plugin crates and getting two
//! unrelated answers.

use clap_sys::plugin::clap_plugin;

pub(crate) struct PluginPtr {
    ptr: *const clap_plugin,
}

impl PluginPtr {
    /// Wrap a raw plugin pointer. Takes ownership — the handle will call
    /// `plugin.destroy()` on drop.
    pub fn new(ptr: *const clap_plugin) -> Self {
        Self { ptr }
    }

    /// Raw pointer, for passing to CLAP functions that take a plugin pointer.
    pub fn as_ptr(&self) -> *const clap_plugin {
        self.ptr
    }

    /// Borrow the underlying `clap_plugin` struct.
    ///
    /// # Safety
    /// Caller must ensure the plugin has not been destroyed yet and is
    /// being called from a thread permitted by the CLAP spec.
    pub unsafe fn as_ref(&self) -> &clap_plugin {
        &*self.ptr
    }
}

impl Drop for PluginPtr {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        unsafe {
            if let Some(destroy) = (*self.ptr).destroy {
                destroy(self.ptr);
            }
        }
    }
}
