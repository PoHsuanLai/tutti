//! [`ResourceStore`]: the Resources a Responder serves, and their data.

use super::super::limits::ResourceId;
use super::data::PeData;
use super::entry::{PeResourceList, ResourceEntry};
use super::kind::PeResource;
use super::request::ResourceTarget;

/// A value the store refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// No entry was declared for the Resource.
    #[error("{0} has no entry declared")]
    NotDeclared(String),
    /// The entry says whether the Resource has a `resId`, and the value differs.
    #[error("the entry of {resource} says requireResId is {required}")]
    ResIdDisagrees {
        /// The Resource.
        resource: String,
        /// What the entry says.
        required: bool,
    },
}

#[derive(Clone, Debug)]
struct Stored {
    entry: ResourceEntry,
    values: Vec<(Option<ResourceId>, PeData)>,
}

/// The Resources of a device, each with its ResourceList entry and its data.
///
/// ResourceList itself is not stored: [`resource_list`](Self::resource_list)
/// is made from the entries (M2-103 §14).
#[derive(Clone, Debug, Default)]
pub struct ResourceStore {
    resources: Vec<Stored>,
}

impl ResourceStore {
    /// A store with nothing in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares a Resource, replacing the entry of one declared before and
    /// keeping its data.
    pub fn declare(&mut self, entry: ResourceEntry) {
        match self
            .resources
            .iter_mut()
            .find(|s| s.entry.resource() == entry.resource())
        {
            Some(stored) => stored.entry = entry,
            None => self.resources.push(Stored {
                entry,
                values: Vec::new(),
            }),
        }
    }

    /// Sets the data of the Resource it is of, or of the entry `res_id` of it.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotDeclared`] for a Resource with no entry, and
    /// [`StoreError::ResIdDisagrees`] when `res_id` is given and the entry says
    /// there is none, or the reverse.
    pub fn put(&mut self, res_id: Option<ResourceId>, data: PeData) -> Result<(), StoreError> {
        let resource = data.resource();
        let stored = self
            .resources
            .iter_mut()
            .find(|s| *s.entry.resource() == resource)
            .ok_or_else(|| StoreError::NotDeclared(resource.as_str().to_owned()))?;
        if stored.entry.require_res_id() != res_id.is_some() {
            return Err(StoreError::ResIdDisagrees {
                resource: resource.as_str().to_owned(),
                required: stored.entry.require_res_id(),
            });
        }
        match stored.values.iter_mut().find(|(id, _)| *id == res_id) {
            Some((_, old)) => *old = data,
            None => stored.values.push((res_id, data)),
        }
        Ok(())
    }

    /// The entry of the ResourceList for `resource`.
    #[must_use]
    pub fn entry(&self, resource: &PeResource) -> Option<&ResourceEntry> {
        self.resources
            .iter()
            .map(|s| &s.entry)
            .find(|e| e.resource() == resource)
    }

    /// The data of a Resource, or of an entry of it.
    #[must_use]
    pub fn get(&self, target: &ResourceTarget) -> Option<&PeData> {
        self.resources
            .iter()
            .find(|s| s.entry.resource() == target.resource())?
            .values
            .iter()
            .find(|(id, _)| id.as_ref() == target.res_id())
            .map(|(_, data)| data)
    }

    /// The Property Data of ResourceList: the entries, in the order declared.
    #[must_use]
    pub fn resource_list(&self) -> PeResourceList {
        self.resources
            .iter()
            .fold(PeResourceList::new(), |list, s| list.with(s.entry.clone()))
    }
}
