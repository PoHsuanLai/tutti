//! The inquiries of a Resource as types that refuse what the spec forbids:
//! Get, Set and Start Subscription for one Resource, with the entry of it, its
//! page and its encoding (M2-103 §7.2, §8.5, §8.6.2, §10, §11).

use super::super::body::BodyLimit;
use super::super::header::PeEncoding;
use super::super::header::PePage;
use super::super::limits::{MediaType, ResourceId};
use super::super::partial::PartialUpdate;
use super::super::reassemble::PeFlowControl;
use super::super::tree::PeObject;
use super::data::PeData;
use super::entry::ResourceEntry;
use super::fields::PeDataError;
use super::kind::{CanSet, PeResource, ResIdUse};
use super::link::{LinkRole, PeLink};

/// An inquiry that the rules of Property Exchange refuse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RequestRefused {
    /// The Resource has entries, and an inquiry for it names one with `resId`
    /// (M2-103 §8.5).
    #[error("{0} needs a resId")]
    ResIdRequired(String),
    /// The Resource has no entries, so an inquiry for it carries no `resId`.
    #[error("{0} takes no resId")]
    ResIdUnused(String),
    /// Only a List Resource is paginated (M2-103 §8.6).
    #[error("{0} is not a List Resource, which pagination is for")]
    NotPaginable(String),
    /// The Property Data of a Set belongs to another Resource.
    #[error("the data is of {data} and the Set is for {target}")]
    DataOfAnotherResource {
        /// The Resource of the data.
        data: String,
        /// The Resource the Set is for.
        target: String,
    },
    /// The ResourceList has no entry for the Resource.
    #[error("{0} is not in the ResourceList")]
    NotListed(String),
    /// The entry of the ResourceList does not let the Resource be got.
    #[error("{0} cannot be got")]
    CannotGet(String),
    /// The entry does not let the Resource be set, or set in part.
    #[error("{resource} cannot be set {scope}")]
    CannotSet {
        /// The Resource.
        resource: String,
        /// What was asked: `in whole` or `in part`.
        scope: &'static str,
    },
    /// The entry does not let the Resource be subscribed to.
    #[error("{0} cannot be subscribed to")]
    CannotSubscribe(String),
    /// The entry does not declare pagination.
    #[error("{0} does not paginate")]
    NotDeclaredPaginable(String),
    /// The entry does not declare the encoding.
    #[error("{resource} does not take {encoding}")]
    EncodingNotDeclared {
        /// The Resource.
        resource: String,
        /// The encoding.
        encoding: &'static str,
    },
    /// The entry does not declare Flow Control for Get.
    #[error("{0} does not use Flow Control on Get")]
    FlowControlNotDeclared(String),
    /// The entry does not list the media type of the data.
    #[error("{resource} does not take {media_type}")]
    MediaTypeNotDeclared {
        /// The Resource.
        resource: String,
        /// The media type.
        media_type: String,
    },
    /// The entry says whether `resId` is used, and the inquiry differs.
    #[error("the entry of {resource} says requireResId is {required}")]
    ResIdDisagrees {
        /// The Resource.
        resource: String,
        /// What the entry says.
        required: bool,
    },
}

/// A Resource, and the entry of it an inquiry names, as the rules of the
/// Resource have them (M2-103 §8.5): a Resource that has entries is named with
/// one, and one that has none is named without.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceTarget {
    resource: PeResource,
    res_id: Option<ResourceId>,
}

impl ResourceTarget {
    /// The Resource without an entry.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::ResIdRequired`] for a Resource that has entries.
    pub fn new(resource: PeResource) -> Result<Self, RequestRefused> {
        if resource.res_id_use() == ResIdUse::Required {
            return Err(RequestRefused::ResIdRequired(resource.as_str().to_owned()));
        }
        Ok(Self {
            resource,
            res_id: None,
        })
    }

    /// The entry `res_id` of the Resource.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::ResIdUnused`] for a Resource that has no entries.
    pub fn entry(resource: PeResource, res_id: ResourceId) -> Result<Self, RequestRefused> {
        if resource.res_id_use() == ResIdUse::Unused {
            return Err(RequestRefused::ResIdUnused(resource.as_str().to_owned()));
        }
        Ok(Self {
            resource,
            res_id: Some(res_id),
        })
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        &self.resource
    }

    /// The entry of the Resource.
    #[must_use]
    pub fn res_id(&self) -> Option<&ResourceId> {
        self.res_id.as_ref()
    }

    /// Whether the target follows what the ResourceList entry says of `resId`.
    fn agrees_with(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        if entry.require_res_id() != self.res_id.is_some() {
            return Err(RequestRefused::ResIdDisagrees {
                resource: self.resource.as_str().to_owned(),
                required: entry.require_res_id(),
            });
        }
        Ok(())
    }
}

/// Get Property Data for a Resource, or the Start of a Subscription to it, which
/// has the same header (M2-103 §7.2, §11.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceGet {
    target: ResourceTarget,
    page: Option<PePage>,
    encoding: PeEncoding,
    flow: PeFlowControl,
    limit: Option<BodyLimit>,
}

impl ResourceGet {
    /// A Get of the whole Property Data, in ASCII, with no Flow Control.
    #[must_use]
    pub fn new(target: ResourceTarget) -> Self {
        Self {
            target,
            page: None,
            encoding: PeEncoding::Ascii,
            flow: PeFlowControl::Off,
            limit: None,
        }
    }

    /// Ends the Get, telling the Responder to stop, if the reply's Property Data
    /// grows past `limit`.
    #[must_use]
    pub fn limited(mut self, limit: BodyLimit) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The most the reply may be.
    #[must_use]
    pub fn limit(&self) -> Option<BodyLimit> {
        self.limit
    }

    /// Asks for a page of a List Resource: `offset` and `limit` together
    /// (M2-103 §8.6.2).
    ///
    /// # Errors
    ///
    /// [`RequestRefused::NotPaginable`] for a Resource that is no List Resource.
    pub fn paged(mut self, page: PePage) -> Result<Self, RequestRefused> {
        if !self.target.resource.is_list() {
            return Err(RequestRefused::NotPaginable(
                self.target.resource.as_str().to_owned(),
            ));
        }
        self.page = Some(page);
        Ok(self)
    }

    /// Asks for the reply in `encoding`.
    #[must_use]
    pub fn encoded(mut self, encoding: PeEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// Asks for the reply under Flow Control, when the Resource declares it.
    #[must_use]
    pub fn with_flow_control(mut self) -> Self {
        self.flow = PeFlowControl::On;
        self
    }

    /// The Resource and its entry.
    #[must_use]
    pub fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// The page asked for.
    #[must_use]
    pub fn page(&self) -> Option<PePage> {
        self.page
    }

    /// The encoding asked for.
    #[must_use]
    pub fn encoding(&self) -> PeEncoding {
        self.encoding
    }

    /// Whether the reply is asked for under Flow Control.
    #[must_use]
    pub fn flow(&self) -> PeFlowControl {
        self.flow
    }

    /// Checks the inquiry against the entry of the ResourceList.
    ///
    /// # Errors
    ///
    /// The [`RequestRefused`] that names what the entry does not allow.
    pub fn check_get(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        let name = || self.target.resource.as_str().to_owned();
        if !entry.can_get() {
            return Err(RequestRefused::CannotGet(name()));
        }
        self.check_header(entry)
    }

    /// Checks the Start of a Subscription against the entry of the ResourceList.
    ///
    /// # Errors
    ///
    /// The [`RequestRefused`] that names what the entry does not allow.
    pub fn check_subscribe(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        if !entry.can_subscribe() {
            return Err(RequestRefused::CannotSubscribe(
                self.target.resource.as_str().to_owned(),
            ));
        }
        self.check_header(entry)
    }

    fn check_header(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        let name = || self.target.resource.as_str().to_owned();
        self.target.agrees_with(entry)?;
        if self.page.is_some() && !entry.can_paginate() {
            return Err(RequestRefused::NotDeclaredPaginable(name()));
        }
        if !entry.encodings().contains(&self.encoding) {
            return Err(RequestRefused::EncodingNotDeclared {
                resource: name(),
                encoding: self.encoding.as_str(),
            });
        }
        if self.flow == PeFlowControl::On && !entry.fc_on_get() {
            return Err(RequestRefused::FlowControlNotDeclared(name()));
        }
        Ok(())
    }
}

/// What a Set sends: the whole Property Data, or the changes of a Partial Set
/// (M2-103 §10).
#[derive(Clone, Debug, PartialEq)]
pub enum SetBody {
    /// Method 1: the whole Property Data a Get would return.
    Full(PeData),
    /// Method 2: `setPartial`, each change a JSON Pointer and a scalar.
    Partial(PartialUpdate),
}

/// Set Property Data for a Resource (M2-103 §10).
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceSet {
    target: ResourceTarget,
    body: SetBody,
    encoding: PeEncoding,
}

impl ResourceSet {
    /// A Full Set of `data`, in ASCII, to the Resource the data is of.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::DataOfAnotherResource`] when `target` is another
    /// Resource.
    pub fn full(target: ResourceTarget, data: PeData) -> Result<Self, RequestRefused> {
        if data.resource() != *target.resource() {
            return Err(RequestRefused::DataOfAnotherResource {
                data: data.resource().as_str().to_owned(),
                target: target.resource().as_str().to_owned(),
            });
        }
        Ok(Self {
            target,
            body: SetBody::Full(data),
            encoding: PeEncoding::Ascii,
        })
    }

    /// A Partial Set of `update`, in ASCII.
    #[must_use]
    pub fn partial(target: ResourceTarget, update: PartialUpdate) -> Self {
        Self {
            target,
            body: SetBody::Partial(update),
            encoding: PeEncoding::Ascii,
        }
    }

    /// Sends the Property Data in `encoding`.
    #[must_use]
    pub fn encoded(mut self, encoding: PeEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// The Resource and its entry.
    #[must_use]
    pub fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// What is sent.
    #[must_use]
    pub fn body(&self) -> &SetBody {
        &self.body
    }

    /// The encoding of the Property Data.
    #[must_use]
    pub fn encoding(&self) -> PeEncoding {
        self.encoding
    }

    /// Checks the Set against the entry of the ResourceList.
    ///
    /// # Errors
    ///
    /// The [`RequestRefused`] that names what the entry does not allow.
    pub fn check(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        let name = self.target.resource().as_str().to_owned();
        let (needed, scope) = match self.body {
            SetBody::Full(_) => (CanSet::Full, "in whole"),
            SetBody::Partial(_) => (CanSet::Partial, "in part"),
        };
        // `partial` includes `full` (M2-103 §14.2).
        let allowed = match entry.can_set() {
            CanSet::None => false,
            CanSet::Full => needed == CanSet::Full,
            CanSet::Partial => true,
        };
        if !allowed {
            return Err(RequestRefused::CannotSet {
                resource: name,
                scope,
            });
        }
        self.target.agrees_with(entry)?;
        if !entry.encodings().contains(&self.encoding) {
            return Err(RequestRefused::EncodingNotDeclared {
                resource: name,
                encoding: self.encoding.as_str(),
            });
        }
        if let SetBody::Full(data) = &self.body {
            let json = MediaType::new("application/json");
            let media_type = data.media_type().or(json.as_ref());
            if let Some(media_type) = media_type
                && !entry.media_types().contains(media_type)
            {
                return Err(RequestRefused::MediaTypeNotDeclared {
                    resource: name,
                    media_type: media_type.as_str().to_owned(),
                });
            }
        }
        Ok(())
    }

    /// The Set a `simpleAction` Link asks for: its Resource and entry, and its
    /// `propData` as the Property Data (M2-103 §13.1.1).
    ///
    /// # Errors
    ///
    /// [`ActionError`] for a Link that is no `simpleAction` and for `propData`
    /// that is not the Resource's.
    pub fn from_simple_action(link: &PeLink) -> Result<Self, ActionError> {
        let Some(LinkRole::Simple(action)) = &link.role else {
            return Err(ActionError::NotThatAction("simpleAction"));
        };
        let data = PeData::decode(&link.resource, &action.prop_data)?;
        Ok(Self::full(
            link_target(&link.resource, link.res_id.as_ref())?,
            data,
        )?)
    }

    /// The Set a `selectorAction` Link asks for once the user chose `chosen`, an
    /// entry of the List Resource of the Link: the action Resource, with the
    /// `selectedProperty` of the entry as the Property Data (M2-103 §13.1.2).
    ///
    /// # Errors
    ///
    /// [`ActionError`] for a Link that is no `selectorAction`, an entry that lacks
    /// the property, and a value that is not the action Resource's.
    pub fn from_selection(link: &PeLink, chosen: &PeObject) -> Result<Self, ActionError> {
        let Some(LinkRole::Selector(action)) = &link.role else {
            return Err(ActionError::NotThatAction("selectorAction"));
        };
        let value = action.selection(chosen).ok_or_else(|| {
            ActionError::NoSelectedProperty(action.selected_property.as_str().to_owned())
        })?;
        let data = PeData::decode(&action.action_resource, value)?;
        let target = link_target(&action.action_resource, action.action_res_id.as_ref())?;
        Ok(Self::full(target, data)?)
    }
}

fn link_target(
    resource: &PeResource,
    res_id: Option<&ResourceId>,
) -> Result<ResourceTarget, RequestRefused> {
    match res_id {
        Some(id) => ResourceTarget::entry(resource.clone(), id.clone()),
        None => ResourceTarget::new(resource.clone()),
    }
}

/// A Link whose Action cannot be made into a Set.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ActionError {
    /// The Link has not the role.
    #[error("the Link is not a {0} Link")]
    NotThatAction(&'static str),
    /// The chosen entry has no property `selectedProperty` names.
    #[error("the chosen entry has no {0}")]
    NoSelectedProperty(String),
    /// The data is not the Resource's.
    #[error(transparent)]
    Data(#[from] PeDataError),
    /// The Set breaks a rule of the Resource.
    #[error(transparent)]
    Refused(#[from] RequestRefused),
}
