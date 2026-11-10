//! `clap.context-menu`: a plugin's menu for a target, read through a builder.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use clap_sys::ext::context_menu::{
    CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU, CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_END_SUBMENU, CLAP_CONTEXT_MENU_ITEM_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_SEPARATOR, CLAP_CONTEXT_MENU_ITEM_TITLE,
    CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL, CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
    clap_context_menu_builder, clap_context_menu_check_entry, clap_context_menu_entry,
    clap_context_menu_item_title, clap_context_menu_submenu, clap_context_menu_target,
};

use crate::guard::guard;
use crate::plugin::Plugin;
use tutti_plugin_types::MainThread;

/// What a context menu is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTarget {
    /// The plugin as a whole.
    Global,
    /// One parameter, by id.
    Param(u32),
}

impl MenuTarget {
    fn raw(self) -> clap_context_menu_target {
        match self {
            Self::Global => clap_context_menu_target {
                kind: CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL,
                id: 0,
            },
            Self::Param(id) => clap_context_menu_target {
                kind: CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
                id,
            },
        }
    }
}

/// One item of a plugin's context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    /// A clickable entry.
    Entry {
        /// The label.
        label: String,
        /// Whether it can be clicked.
        is_enabled: bool,
        /// The id passed back to perform it.
        action_id: u32,
    },
    /// An entry that can be checked.
    CheckEntry {
        /// The label.
        label: String,
        /// Whether it can be clicked.
        is_enabled: bool,
        /// Whether it is checked.
        is_checked: bool,
        /// The id passed back to perform it.
        action_id: u32,
    },
    /// A separator line.
    Separator,
    /// A title row.
    Title {
        /// The title.
        title: String,
        /// Whether it is enabled.
        is_enabled: bool,
    },
    /// The start of a submenu.
    BeginSubmenu {
        /// The submenu's label.
        label: String,
        /// Whether it is enabled.
        is_enabled: bool,
    },
    /// The end of the innermost submenu.
    EndSubmenu,
}

/// A nul-terminated string field the plugin passed, lossily.
///
/// # Safety
/// `ptr` is null or nul-terminated and readable.
unsafe fn text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: the contract.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// `clap_context_menu_builder.add_item`: appends to the `Vec<MenuItem>` the
/// builder's `ctx` names.
extern "C" fn add_item(
    builder: *const clap_context_menu_builder,
    item_kind: u32,
    item_data: *const c_void,
) -> bool {
    guard(false, || {
        if builder.is_null() {
            return false;
        }
        // SAFETY: the builder is the one `populate` built on its own stack,
        // valid for the call.
        let ctx = unsafe { (*builder).ctx };
        if ctx.is_null() {
            return false;
        }
        // SAFETY: `ctx` is the `&mut Vec<MenuItem>` of `populate`, alive and
        // otherwise unused while the plugin runs inside it.
        let items = unsafe { &mut *ctx.cast::<Vec<MenuItem>>() };
        // SAFETY (each arm): the plugin passes a struct of the kind
        // `item_kind` names (`context-menu.h`), readable for the call;
        // checked non-null; its strings are nul-terminated.
        let item = match item_kind {
            CLAP_CONTEXT_MENU_ITEM_ENTRY => {
                if item_data.is_null() {
                    return false;
                }
                // SAFETY: the plugin passes a struct of the kind `item_kind` names, readable for the call (`context-menu.h`).
                let e = unsafe { &*item_data.cast::<clap_context_menu_entry>() };
                MenuItem::Entry {
                    // SAFETY: its `label` is null or nul-terminated.
                    label: unsafe { text(e.label) },
                    is_enabled: e.is_enabled,
                    action_id: e.action_id,
                }
            }
            CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY => {
                if item_data.is_null() {
                    return false;
                }
                // SAFETY: as the entry above.
                let e = unsafe { &*item_data.cast::<clap_context_menu_check_entry>() };
                MenuItem::CheckEntry {
                    // SAFETY: as above.
                    label: unsafe { text(e.label) },
                    is_enabled: e.is_enabled,
                    is_checked: e.is_checked,
                    action_id: e.action_id,
                }
            }
            CLAP_CONTEXT_MENU_ITEM_SEPARATOR => MenuItem::Separator,
            CLAP_CONTEXT_MENU_ITEM_TITLE => {
                if item_data.is_null() {
                    return false;
                }
                // SAFETY: as above.
                let t = unsafe { &*item_data.cast::<clap_context_menu_item_title>() };
                MenuItem::Title {
                    // SAFETY: as above.
                    title: unsafe { text(t.title) },
                    is_enabled: t.is_enabled,
                }
            }
            CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU => {
                if item_data.is_null() {
                    return false;
                }
                // SAFETY: as above.
                let s = unsafe { &*item_data.cast::<clap_context_menu_submenu>() };
                MenuItem::BeginSubmenu {
                    // SAFETY: as above.
                    label: unsafe { text(s.label) },
                    is_enabled: s.is_enabled,
                }
            }
            CLAP_CONTEXT_MENU_ITEM_END_SUBMENU => MenuItem::EndSubmenu,
            _ => return false,
        };
        items.push(item);
        true
    })
}

/// `clap_context_menu_builder.supports`: the six kinds `add_item` takes.
extern "C" fn supports(_builder: *const clap_context_menu_builder, item_kind: u32) -> bool {
    guard(false, || {
        matches!(
            item_kind,
            CLAP_CONTEXT_MENU_ITEM_ENTRY
                | CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY
                | CLAP_CONTEXT_MENU_ITEM_SEPARATOR
                | CLAP_CONTEXT_MENU_ITEM_TITLE
                | CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU
                | CLAP_CONTEXT_MENU_ITEM_END_SUBMENU
        )
    })
}

/// Runs `f` with a builder whose `ctx` is `items`, for a caller that drives
/// the builder callbacks itself (a test).
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn builder_for_test<R>(
    items: Option<&mut Vec<MenuItem>>,
    f: impl FnOnce(*const clap_context_menu_builder) -> R,
) -> R {
    let builder = clap_context_menu_builder {
        ctx: items.map_or(ptr::null_mut(), |v| ptr::from_mut(v).cast::<c_void>()),
        add_item: Some(add_item),
        supports: Some(supports),
    };
    f(&builder)
}

impl Plugin {
    /// `context-menu.populate`: the plugin's items for `target`, `None` when
    /// it has none to offer.
    #[must_use]
    pub fn context_menu_populate(
        &self,
        _main: &MainThread,
        target: MenuTarget,
    ) -> Option<Vec<MenuItem>> {
        let populate = self.context_menu_ext()?.populate?;
        let target = target.raw();
        let mut items = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: ptr::from_mut(&mut items).cast::<c_void>(),
            add_item: Some(add_item),
            supports: Some(supports),
        };
        // SAFETY: the plugin's own `populate` with the instance, a target and
        // a builder both valid for the call; `items` is not touched until it
        // returns.
        let ok = unsafe { populate(self.raw(), &target, &builder) };
        ok.then_some(items)
    }

    /// `context-menu.perform`.
    pub fn context_menu_perform(
        &self,
        _main: &MainThread,
        target: MenuTarget,
        action_id: u32,
    ) -> bool {
        let Some(perform) = self.context_menu_ext().and_then(|e| e.perform) else {
            return false;
        };
        let target = target.raw();
        // SAFETY: the plugin's own `perform` with the instance and a target
        // valid for the call.
        unsafe { perform(self.raw(), &target, action_id) }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MenuItem as ContextMenuItem, add_item as context_menu_builder_add_item,
        supports as context_menu_builder_supports,
    };
    use clap_sys::ext::context_menu::{
        CLAP_CONTEXT_MENU_ITEM_ENTRY, CLAP_CONTEXT_MENU_ITEM_SEPARATOR, clap_context_menu_builder,
        clap_context_menu_entry,
    };
    use std::ffi::c_void;

    #[test]
    fn test_context_menu_builder_null_builder() {
        {
            let result = context_menu_builder_add_item(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(!result);
        }
    }

    #[test]
    fn test_context_menu_builder_null_ctx() {
        let builder = clap_context_menu_builder {
            ctx: std::ptr::null_mut(),
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(!result);
        }
    }

    #[test]
    fn test_context_menu_builder_separator() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(result);
        }
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], ContextMenuItem::Separator));
    }

    #[test]
    fn test_context_menu_builder_entry_null_data() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_ENTRY,
                std::ptr::null(),
            );
            assert!(!result);
        }
        assert!(items.is_empty());
    }

    #[test]
    fn test_context_menu_builder_entry_with_data() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };

        let label = std::ffi::CString::new("Test Entry").unwrap();
        let entry = clap_context_menu_entry {
            label: label.as_ptr(),
            is_enabled: true,
            action_id: 42,
        };

        {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_ENTRY,
                &entry as *const clap_context_menu_entry as *const c_void,
            );
            assert!(result);
        }
        assert_eq!(items.len(), 1);
        match &items[0] {
            ContextMenuItem::Entry {
                label,
                is_enabled,
                action_id,
            } => {
                assert_eq!(label, "Test Entry");
                assert!(*is_enabled);
                assert_eq!(*action_id, 42);
            }
            _ => panic!("Expected Entry"),
        }
    }

    #[test]
    fn test_context_menu_builder_unknown_type() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        {
            let result = context_menu_builder_add_item(&builder, 9999, std::ptr::null());
            assert!(!result);
        }
        assert!(items.is_empty());
    }

    #[test]
    fn test_context_menu_builder_supports() {
        {
            assert!(context_menu_builder_supports(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_ENTRY
            ));
            assert!(context_menu_builder_supports(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR
            ));
            assert!(!context_menu_builder_supports(std::ptr::null(), 9999));
        }
    }
}
