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
    /// Every plugin this bundle advertises, selected one included.
    ///
    /// Carried so a caller can see that a bundle holds more than the plugin it
    /// just loaded. Without it a multi-plugin bundle is indistinguishable from
    /// a single-plugin one at every level above this.
    pub siblings: Vec<PluginInfo>,
}

pub(super) fn load_descriptor<'lib>(
    library: &'lib libloading::Library,
    bundle_path: &Path,
    plugin_id: Option<&str>,
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
    let descriptors = all_descriptors(factory, factory_ptr, bundle_path)?;
    let descriptor = select_descriptor(&descriptors, plugin_id, bundle_path)?;
    // Only the selected descriptor's version is checked. A sibling with an
    // unreadable version is not this load's problem — it is listed so a caller
    // can see it exists, and would be rejected on its own load attempt.
    check_version(
        descriptor.clap_version,
        "plugin descriptor",
        LoadStage::Factory,
        bundle_path,
    )?;
    let info = descriptor_to_info(descriptor);
    let siblings = descriptors.iter().map(|d| descriptor_to_info(d)).collect();

    Ok(LoadedDescriptor {
        entry_guard,
        factory_ptr,
        factory,
        info,
        siblings,
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

/// Every descriptor the factory advertises, in factory order.
///
/// A `.clap` bundle is a factory, not a plugin: `get_plugin_count` exists
/// precisely because one file may ship a synth plus companion effects. This
/// used to take index 0 and discard the count, so every plugin after the first
/// in a bundle was unreachable — with no error, because index 0 loads fine.
///
/// A descriptor that comes back null is skipped rather than failing the whole
/// bundle: one broken entry should not make its siblings unloadable. An empty
/// result is still an error, since a factory advertising no readable plugin has
/// nothing to load.
fn all_descriptors<'lib>(
    factory: &'lib clap_plugin_factory,
    factory_ptr: *const clap_plugin_factory,
    bundle_path: &Path,
) -> Result<Vec<&'lib clap_plugin_descriptor>> {
    let count_fn = factory.get_plugin_count.ok_or_else(|| {
        fail(
            bundle_path,
            LoadStage::Factory,
            "No get_plugin_count function",
        )
    })?;
    let count = unsafe { count_fn(factory_ptr) };
    if count == 0 {
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

    let mut descriptors = Vec::new();
    for index in 0..count {
        let desc_ptr = unsafe { get_desc(factory_ptr, index) };
        if desc_ptr.is_null() {
            continue;
        }
        descriptors.push(unsafe { &*desc_ptr });
    }

    if descriptors.is_empty() {
        return Err(fail(
            bundle_path,
            LoadStage::Factory,
            format!("Factory advertises {count} plugins but returned no descriptor"),
        ));
    }

    Ok(descriptors)
}

/// Pick the descriptor to load: the one whose id matches `wanted`, or the first
/// if no id was named.
///
/// Defaulting to the first keeps every single-plugin bundle — which is most of
/// them — loading exactly as before, so naming an id is only necessary for the
/// bundles that actually have a choice to make.
///
/// A named id that no descriptor carries is an error listing what the bundle
/// does contain. Falling back to the first would silently load a *different
/// plugin* than the one asked for, which is worse than failing: a session
/// restoring "the compressor" would come back with the synth and no complaint.
fn select_descriptor<'lib>(
    descriptors: &[&'lib clap_plugin_descriptor],
    wanted: Option<&str>,
    bundle_path: &Path,
) -> Result<&'lib clap_plugin_descriptor> {
    let Some(wanted) = wanted else {
        return Ok(descriptors[0]);
    };

    descriptors
        .iter()
        .copied()
        .find(|d| !d.id.is_null() && unsafe { cstr_to_string(d.id) } == wanted)
        .ok_or_else(|| {
            let available: Vec<String> = descriptors
                .iter()
                .map(|d| {
                    if d.id.is_null() {
                        "<null id>".to_string()
                    } else {
                        unsafe { cstr_to_string(d.id) }
                    }
                })
                .collect();
            fail(
                bundle_path,
                LoadStage::Factory,
                format!(
                    "No plugin with id '{wanted}' in this bundle; it contains: {}",
                    available.join(", ")
                ),
            )
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptor carrying just the id `select_descriptor` matches on.
    ///
    /// The other fields stay zeroed: selection reads `id` and nothing else, and
    /// a fuller fixture would imply the function looks at more than it does.
    ///
    /// # Safety
    /// `clap_plugin_descriptor` is POD (a version struct and `*const c_char`s),
    /// so an all-zero value is valid. The returned descriptor borrows `id`, so
    /// the `CStr` must outlive it.
    fn desc_with_id(id: &CStr) -> clap_plugin_descriptor {
        let mut d: clap_plugin_descriptor = unsafe { std::mem::zeroed() };
        d.id = id.as_ptr();
        d
    }

    /// Naming no id loads the bundle's first plugin.
    ///
    /// This is what keeps every single-plugin bundle — which is nearly all of
    /// them — loading exactly as it did before ids were selectable.
    #[test]
    fn no_id_selects_the_first_descriptor() {
        let (da, db) = (
            desc_with_id(c"com.example.synth"),
            desc_with_id(c"com.example.fx"),
        );
        let descriptors = vec![&da, &db];

        let picked = select_descriptor(&descriptors, None, Path::new("/x.clap")).unwrap();
        assert_eq!(unsafe { cstr_to_string(picked.id) }, "com.example.synth");
    }

    /// A named id selects that plugin, including one that is not first.
    ///
    /// The whole finding: `get_plugin_descriptor(factory, 0)` was hard-coded,
    /// so a bundle shipping a synth plus companion effects exposed only the
    /// synth, and the effects were unreachable with no error — index 0 loads
    /// fine, so nothing looked wrong.
    #[test]
    fn a_named_id_selects_a_plugin_that_is_not_the_first() {
        let (da, db) = (
            desc_with_id(c"com.example.synth"),
            desc_with_id(c"com.example.fx"),
        );
        let descriptors = vec![&da, &db];

        let picked = select_descriptor(&descriptors, Some("com.example.fx"), Path::new("/x.clap"))
            .expect("the second plugin in the bundle must be selectable");
        assert_eq!(unsafe { cstr_to_string(picked.id) }, "com.example.fx");
    }

    /// An id no descriptor carries is an error, not a silent fallback.
    ///
    /// Falling back to the first would load a *different plugin* than the one
    /// asked for: a session restoring "the compressor" would come back with the
    /// synth and no complaint. The message lists what the bundle does hold, so
    /// the caller can correct the id.
    #[test]
    fn an_unknown_id_fails_rather_than_loading_something_else() {
        let (da, db) = (
            desc_with_id(c"com.example.synth"),
            desc_with_id(c"com.example.fx"),
        );
        let descriptors = vec![&da, &db];

        let err = select_descriptor(
            &descriptors,
            Some("com.example.missing"),
            Path::new("/x.clap"),
        )
        .expect_err("an unknown id must not silently load the first plugin");

        let msg = err.to_string();
        assert!(msg.contains("com.example.missing"), "names the wanted id");
        assert!(msg.contains("com.example.synth"), "lists what is available");
        assert!(msg.contains("com.example.fx"), "lists every available id");
    }

    /// A descriptor with a null id is skipped, not dereferenced.
    ///
    /// `id` is a raw `*const c_char` straight from the plugin; a factory that
    /// leaves it null must not take the host down mid-selection.
    #[test]
    fn a_null_id_is_skipped_rather_than_dereferenced() {
        let mut null_id: clap_plugin_descriptor = unsafe { std::mem::zeroed() };
        null_id.id = std::ptr::null();
        let good = desc_with_id(c"com.example.fx");
        let descriptors = vec![&null_id, &good];

        let picked = select_descriptor(&descriptors, Some("com.example.fx"), Path::new("/x.clap"))
            .expect("a null-id sibling must not stop a valid id from matching");
        assert_eq!(unsafe { cstr_to_string(picked.id) }, "com.example.fx");
    }
}
