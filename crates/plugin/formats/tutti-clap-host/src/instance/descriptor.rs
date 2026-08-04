//! Shared descriptor-loading code used by both `probe` and
//! `load_with_library`. Everything through "get the first plugin descriptor
//! from the factory" lives here; the two entry points then diverge.

use super::entry::{entry_registry_acquire, EntryGuard};
use crate::cstr_to_string;
use crate::error::{ClapError, LoadStage, Result};
use crate::types::PluginInfo;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::{clap_version, clap_version_is_compatible, CLAP_VERSION_MAJOR};
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
    // Before `init`, not after: an entry we cannot read is one whose `init`
    // pointer we cannot trust to be at the offset we are about to call it from.
    check_version(
        entry.clap_version,
        "clap_entry",
        LoadStage::Opening,
        bundle_path,
    )?;
    let entry_guard = init_entry(entry, bundle_path)?;
    let (factory_ptr, factory) = plugin_factory(entry, bundle_path)?;
    let descriptor = first_descriptor(factory, factory_ptr, bundle_path)?;
    check_version(
        descriptor.clap_version,
        "plugin descriptor",
        LoadStage::Factory,
        bundle_path,
    )?;
    let info = descriptor_to_info(descriptor);

    Ok(LoadedDescriptor {
        entry_guard,
        factory_ptr,
        factory,
        info,
    })
}

/// Reject a `clap_version` this host cannot read structs against.
///
/// Two bounds, and the SDK supplies only the lower one.
///
/// **Floor** — `clap_version_is_compatible` (`version.h:38-40`) is `major >= 1`,
/// and the struct comment says why: `0.X.Y` was "the development stage, API and
/// ABI are not stable". Nothing about a 0.x layout is promised to match 1.x, so
/// reading one through 1.2 bindings misreads memory rather than missing a
/// feature — a wrong field at a wrong offset, taken for a function pointer and
/// called.
///
/// **Ceiling** — `major > CLAP_VERSION_MAJOR` is rejected here, on top of the
/// SDK function. That predicate is written from the *plugin's* side, where the
/// question is "is this host's version one I was designed against?", and a
/// plugin has no future majors to worry about. A host reading a plugin faces
/// the mirror question, and a major bump is precisely the announcement that the
/// layout changed. `clap_version_is_compatible` alone would accept a 2.0
/// descriptor and hand it to `descriptor_to_info`, which reads seven `*const
/// c_char` at 1.x offsets.
///
/// Minor and revision are not bounded in either direction: CLAP adds within a
/// major by appending fields and extension ids, so a 1.9 plugin read by a 1.2
/// host sees a prefix it understands, and a 1.0 plugin read here simply lacks
/// the later extensions — which every `get_extension` call already handles by
/// returning null.
///
/// Checked at two sites because they are two separate claims. `clap_entry`'s is
/// the DSO's, made before any call into it; the descriptor's is one plugin's,
/// and a bundle may ship several. The spec initializes both to `CLAP_VERSION`
/// and never says one implies the other. The two report different
/// [`LoadStage`]s, so a scanner's log names which claim was rejected.
fn check_version(
    version: clap_version,
    what: &str,
    stage: LoadStage,
    bundle_path: &Path,
) -> Result<()> {
    if clap_version_is_compatible(version) && version.major <= CLAP_VERSION_MAJOR {
        return Ok(());
    }
    Err(fail(
        bundle_path,
        stage,
        format!(
            "{what} declares CLAP {}.{}.{}, which this host cannot read its \
             structs against: it is built for major {CLAP_VERSION_MAJOR} \
             (version.h sets the floor at major >= 1 — 0.x is the development \
             stage, with no stable ABI)",
            version.major, version.minor, version.revision,
        ),
    ))
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
