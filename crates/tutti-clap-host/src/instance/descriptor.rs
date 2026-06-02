//! Shared descriptor-loading code used by both `probe` and
//! `load_with_library`. Everything through "get the first plugin descriptor
//! from the factory" lives here; the two entry points then diverge.

use super::entry_registry_acquire;
use super::EntryGuard;
use crate::cstr_to_string;
use crate::error::{ClapError, LoadStage, Result};
use crate::types::PluginInfo;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::plugin::clap_plugin_descriptor;
use std::ffi::{CStr, CString};
use std::path::Path;

pub(super) struct LoadedDescriptor<'lib> {
    pub entry_guard: EntryGuard,
    pub factory_ptr: *const clap_plugin_factory,
    pub factory: &'lib clap_plugin_factory,
    pub info: PluginInfo,
}

pub(super) fn load_descriptor<'lib>(
    library: &'lib libloading::Library,
    bundle_path: &Path,
) -> Result<LoadedDescriptor<'lib>> {
    let entry = entry_struct(library, bundle_path)?;
    let entry_guard = init_entry(entry, bundle_path)?;
    let (factory_ptr, factory) = plugin_factory(entry, bundle_path)?;
    let descriptor = first_descriptor(factory, factory_ptr, bundle_path)?;
    let info = descriptor_to_info(descriptor);

    Ok(LoadedDescriptor {
        entry_guard,
        factory_ptr,
        factory,
        info,
    })
}

fn fail(path: &Path, stage: LoadStage, reason: impl Into<String>) -> ClapError {
    ClapError::LoadFailed {
        path: path.to_path_buf(),
        stage,
        reason: reason.into(),
    }
}

fn entry_struct<'lib>(
    library: &'lib libloading::Library,
    bundle_path: &Path,
) -> Result<&'lib clap_plugin_entry> {
    unsafe {
        let sym = library
            .get::<*const clap_plugin_entry>(b"clap_entry\0")
            .map_err(|e| {
                fail(
                    bundle_path,
                    LoadStage::Opening,
                    format!("No clap_entry symbol: {e}"),
                )
            })?;
        Ok(&*(*sym))
    }
}

fn init_entry(entry: &clap_plugin_entry, bundle_path: &Path) -> Result<EntryGuard> {
    let init_fn = entry
        .init
        .ok_or_else(|| fail(bundle_path, LoadStage::Opening, "No init function"))?;

    let path_cstr = CString::new(bundle_path.to_string_lossy().as_ref()).map_err(|e| {
        fail(
            bundle_path,
            LoadStage::Opening,
            format!("Invalid path: {e}"),
        )
    })?;

    entry_registry_acquire(bundle_path, init_fn, &path_cstr)
        .map_err(|reason| fail(bundle_path, LoadStage::Opening, reason))
}

fn plugin_factory<'lib>(
    entry: &'lib clap_plugin_entry,
    bundle_path: &Path,
) -> Result<(*const clap_plugin_factory, &'lib clap_plugin_factory)> {
    let get_factory = entry
        .get_factory
        .ok_or_else(|| fail(bundle_path, LoadStage::Factory, "No get_factory function"))?;

    let factory_ptr =
        unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
    if factory_ptr.is_null() {
        return Err(fail(bundle_path, LoadStage::Factory, "No plugin factory"));
    }

    Ok((factory_ptr, unsafe { &*factory_ptr }))
}

fn first_descriptor<'lib>(
    factory: &'lib clap_plugin_factory,
    factory_ptr: *const clap_plugin_factory,
    bundle_path: &Path,
) -> Result<&'lib clap_plugin_descriptor> {
    let count_fn = factory.get_plugin_count.ok_or_else(|| {
        fail(
            bundle_path,
            LoadStage::Factory,
            "No get_plugin_count function",
        )
    })?;
    if unsafe { count_fn(factory_ptr) } == 0 {
        return Err(fail(
            bundle_path,
            LoadStage::Factory,
            "No plugins in factory",
        ));
    }

    let get_desc = factory.get_plugin_descriptor.ok_or_else(|| {
        fail(
            bundle_path,
            LoadStage::Factory,
            "No get_plugin_descriptor function",
        )
    })?;

    let desc_ptr = unsafe { get_desc(factory_ptr, 0) };
    if desc_ptr.is_null() {
        return Err(fail(
            bundle_path,
            LoadStage::Factory,
            "No plugin descriptor",
        ));
    }

    Ok(unsafe { &*desc_ptr })
}

fn descriptor_to_info(descriptor: &clap_plugin_descriptor) -> PluginInfo {
    let features = if descriptor.features.is_null() {
        Vec::new()
    } else {
        const MAX_FEATURES: usize = 256;
        let mut features = Vec::new();
        let mut ptr = descriptor.features;
        unsafe {
            while !(*ptr).is_null() && features.len() < MAX_FEATURES {
                features.push(CStr::from_ptr(*ptr).to_string_lossy().into_owned());
                ptr = ptr.add(1);
            }
        }
        features
    };

    unsafe {
        PluginInfo::new(
            cstr_to_string(descriptor.id),
            cstr_to_string(descriptor.name),
        )
        .vendor(cstr_to_string(descriptor.vendor))
        .version(cstr_to_string(descriptor.version))
        .url(cstr_to_string(descriptor.url))
        .description(cstr_to_string(descriptor.description))
        .features(features)
    }
}
