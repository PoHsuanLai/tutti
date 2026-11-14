//! The Links and the Columns of a Resource's Property Data (M2-103 §13, §14.5).

use super::super::limits::{PropertyName, ResourceId, ResourceName};
use super::super::tree::{PeJson, PeObject};
use super::fields::{Fields, PeDataError, PeDataFault, PeExtensions, RoleName, boolean, text};
use super::kind::PeResource;

/// How a Link is used (`role`, M2-103 §13.1, M2-117 §3.4.1).
#[derive(Clone, Debug, PartialEq)]
pub enum LinkRole {
    /// `cluster`: the linked Resource is for the Channels of a Cluster, not its
    /// Basic Channel (M2-117 §3.4.1).
    Cluster,
    /// `simpleAction`: a button that Sets a Resource with fixed data (§13.1.1).
    Simple(SimpleAction),
    /// `selectorAction`: a choice from a List Resource that Sets a Resource
    /// (§13.1.2).
    Selector(SelectorAction),
    /// Any other role: one a later version defines, or the manufacturer's own
    /// `x-` role (§13).
    Other(RoleName),
}

/// The kind of confirmation box of an Action (`confirmType`, M2-103 Table 68).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfirmType {
    /// `okcancel`: OK and Cancel.
    OkCancel,
    /// `yesno`: Yes and No.
    YesNo,
}

impl ConfirmType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::OkCancel => "okcancel",
            Self::YesNo => "yesno",
        }
    }

    fn new(text: &str) -> Option<Self> {
        [Self::OkCancel, Self::YesNo]
            .into_iter()
            .find(|c| c.as_str() == text)
    }
}

/// The icon of a confirmation box (`confirmIcon`, M2-103 Table 68).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfirmIcon {
    /// `exclamation`.
    Exclamation,
    /// `warning`.
    Warning,
    /// `information`.
    Information,
    /// `question`.
    Question,
}

impl ConfirmIcon {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exclamation => "exclamation",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Question => "question",
        }
    }

    fn new(text: &str) -> Option<Self> {
        [
            Self::Exclamation,
            Self::Warning,
            Self::Information,
            Self::Question,
        ]
        .into_iter()
        .find(|c| c.as_str() == text)
    }
}

/// A question put to the user before an Action is done (M2-103 Table 68).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    /// The message of the box (`confirmMsg`).
    pub message: String,
    /// Its buttons (`confirmType`).
    pub kind: Option<ConfirmType>,
    /// Its icon (`confirmIcon`).
    pub icon: Option<ConfirmIcon>,
}

/// A `simpleAction` Link: the user triggers it, and the Initiator Sets the
/// Resource of the Link with `propData` (M2-103 §13.1.1, Table 68).
#[derive(Clone, Debug, PartialEq)]
pub struct SimpleAction {
    /// The Property Data to Set, which matches the schema of the Resource
    /// (`propData`, required).
    pub prop_data: PeJson,
    /// The question to ask first, if any.
    pub confirmation: Option<Confirmation>,
}

/// A `selectorAction` Link: the user picks an entry of the List Resource of the
/// Link, and the Initiator Sets the action Resource with one of its properties
/// (M2-103 §13.1.2, Table 72).
#[derive(Clone, Debug, PartialEq)]
pub struct SelectorAction {
    /// The Resource to Set (`actionResource`, required).
    pub action_resource: PeResource,
    /// The entry of it (`actionResId`).
    pub action_res_id: Option<ResourceId>,
    /// The property of the chosen entry that is the Property Data to Set
    /// (`selectedProperty`, required).
    pub selected_property: PropertyName,
}

impl SelectorAction {
    /// The Property Data to Set for the entry of the List Resource the user chose,
    /// or `None` when the entry lacks the property.
    #[must_use]
    pub fn selection<'a>(&self, entry: &'a PeObject) -> Option<&'a PeJson> {
        entry.get(self.selected_property.as_str())
    }
}

/// A Link: the Resource, and the entry of it, that gives more about the object
/// the Link is in (M2-103 Table 64).
#[derive(Clone, Debug, PartialEq)]
pub struct PeLink {
    /// The Resource linked to.
    pub resource: PeResource,
    /// The entry of the Resource (`resId`).
    pub res_id: Option<ResourceId>,
    /// The text to show for the Link.
    pub title: Option<String>,
    /// How the Link is used.
    pub role: Option<LinkRole>,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeLink {
    /// A Link to `resource` with no other property.
    #[must_use]
    pub fn new(resource: PeResource) -> Self {
        Self {
            resource,
            res_id: None,
            title: None,
            role: None,
            extensions: PeExtensions::default(),
        }
    }

    /// The Link to the entry `res_id` of `resource`.
    #[must_use]
    pub fn entry(resource: PeResource, res_id: ResourceId) -> Self {
        Self {
            res_id: Some(res_id),
            ..Self::new(resource)
        }
    }

    pub(super) fn to_json(&self) -> PeJson {
        let mut object = PeObject::new();
        object.insert("resource", self.resource.as_str().into());
        if let Some(id) = &self.res_id {
            object.insert("resId", id.as_str().into());
        }
        if let Some(title) = &self.title {
            object.insert("title", title.as_str().into());
        }
        match &self.role {
            None => {}
            Some(LinkRole::Cluster) => object.insert("role", "cluster".into()),
            Some(LinkRole::Other(name)) => object.insert("role", name.as_str().into()),
            Some(LinkRole::Simple(action)) => {
                object.insert("role", "simpleAction".into());
                object.insert("propData", action.prop_data.clone());
                if let Some(c) = &action.confirmation {
                    object.insert("confirmMsg", c.message.as_str().into());
                    if let Some(kind) = c.kind {
                        object.insert("confirmType", kind.as_str().into());
                    }
                    if let Some(icon) = c.icon {
                        object.insert("confirmIcon", icon.as_str().into());
                    }
                }
            }
            Some(LinkRole::Selector(action)) => {
                object.insert("role", "selectorAction".into());
                object.insert("actionResource", action.action_resource.as_str().into());
                if let Some(id) = &action.action_res_id {
                    object.insert("actionResId", id.as_str().into());
                }
                object.insert("selectedProperty", action.selected_property.as_str().into());
            }
        }
        self.extensions.write(&mut object);
        PeJson::Object(object)
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "link")?;
        let name = f.required("resource", "a string", text)?.to_owned();
        let resource = ResourceName::new(&name)
            .map(|n| PeResource::from_name(&n))
            .ok_or_else(|| f.fail("resource", PeDataFault::NotValid("resource name")))?;
        let res_id = f
            .optional("resId", "a string", text)?
            .map(|t| {
                ResourceId::new(t)
                    .ok_or_else(|| f.fail("resId", PeDataFault::NotValid("resource id")))
            })
            .transpose()?;
        let title = f.optional("title", "a string", |j| text(j).map(str::to_owned))?;
        let role_text = f.optional("role", "a string", |j| text(j).map(str::to_owned))?;
        let role = match role_text.as_deref() {
            None => None,
            Some("cluster") => Some(LinkRole::Cluster),
            Some("simpleAction") => Some(LinkRole::Simple(read_simple(&mut f)?)),
            Some("selectorAction") => Some(LinkRole::Selector(read_selector(&mut f)?)),
            Some(other) => {
                Some(LinkRole::Other(RoleName::new(other).ok_or_else(|| {
                    f.fail("role", PeDataFault::NotValid("link role"))
                })?))
            }
        };
        Ok(Self {
            resource,
            res_id,
            title,
            role,
            extensions: f.finish()?,
        })
    }
}

fn read_simple(f: &mut Fields<'_>) -> Result<SimpleAction, PeDataError> {
    let prop_data = f.required("propData", "any JSON", |j| Some(j.clone()))?;
    let message = f.optional("confirmMsg", "a string", |j| text(j).map(str::to_owned))?;
    let kind = f
        .optional("confirmType", "a string", text)?
        .map(|t| {
            ConfirmType::new(t)
                .ok_or_else(|| f.fail("confirmType", PeDataFault::NotValid("confirm type")))
        })
        .transpose()?;
    let icon = f
        .optional("confirmIcon", "a string", text)?
        .map(|t| {
            ConfirmIcon::new(t)
                .ok_or_else(|| f.fail("confirmIcon", PeDataFault::NotValid("confirm icon")))
        })
        .transpose()?;
    let confirmation = match (message, kind, icon) {
        (Some(message), kind, icon) => Some(Confirmation {
            message,
            kind,
            icon,
        }),
        (None, None, None) => None,
        (None, _, _) => return Err(f.fail("confirmMsg", PeDataFault::Missing)),
    };
    Ok(SimpleAction {
        prop_data,
        confirmation,
    })
}

fn read_selector(f: &mut Fields<'_>) -> Result<SelectorAction, PeDataError> {
    let action = f.required("actionResource", "a string", text)?.to_owned();
    let action_resource = ResourceName::new(&action)
        .map(|n| PeResource::from_name(&n))
        .ok_or_else(|| f.fail("actionResource", PeDataFault::NotValid("resource name")))?;
    let action_res_id = f
        .optional("actionResId", "a string", text)?
        .map(|t| {
            ResourceId::new(t)
                .ok_or_else(|| f.fail("actionResId", PeDataFault::NotValid("resource id")))
        })
        .transpose()?;
    let selected = f.required("selectedProperty", "a string", text)?.to_owned();
    let selected_property = PropertyName::new(&selected)
        .ok_or_else(|| f.fail("selectedProperty", PeDataFault::NotValid("property name")))?;
    Ok(SelectorAction {
        action_resource,
        action_res_id,
        selected_property,
    })
}

/// What a Column of a table shows (M2-103 §14.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnSource {
    /// `property`: a property of each object of the List Resource.
    Property(PropertyName),
    /// `link`: the Link to this Resource of each object.
    Link(PeResource),
}

/// A Column of the table a List Resource describes (`columns`, M2-103 Table 80).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeColumn {
    /// What the Column shows.
    pub source: ColumnSource,
    /// The heading of the Column.
    pub title: Option<String>,
}

impl PeColumn {
    pub(super) fn to_json(&self) -> PeJson {
        let mut object = PeObject::new();
        match &self.source {
            ColumnSource::Property(name) => object.insert("property", name.as_str().into()),
            ColumnSource::Link(resource) => object.insert("link", resource.as_str().into()),
        }
        if let Some(title) = &self.title {
            object.insert("title", title.as_str().into());
        }
        PeJson::Object(object)
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "column")?;
        let property = f.optional("property", "a string", text)?.map(str::to_owned);
        let link = f.optional("link", "a string", text)?.map(str::to_owned);
        let title = f.optional("title", "a string", |j| text(j).map(str::to_owned))?;
        let source = match (property, link) {
            (Some(p), None) => PropertyName::new(&p)
                .map(ColumnSource::Property)
                .ok_or_else(|| f.fail("property", PeDataFault::NotValid("property name")))?,
            (None, Some(l)) => ResourceName::new(&l)
                .map(|n| ColumnSource::Link(PeResource::from_name(&n)))
                .ok_or_else(|| f.fail("link", PeDataFault::NotValid("resource name")))?,
            (None, None) => return Err(f.fail("property", PeDataFault::Missing)),
            (Some(_), Some(_)) => return Err(f.fail("link", PeDataFault::Conflicts("property"))),
        };
        // The properties a later version adds are dropped.
        f.finish()?;
        Ok(Self { source, title })
    }
}

/// A `bool` property that has a default, written only when it differs.
pub(super) fn flag(
    fields: &mut Fields<'_>,
    name: &'static str,
    default: bool,
) -> Result<bool, PeDataError> {
    Ok(fields
        .optional(name, "a boolean", boolean)?
        .unwrap_or(default))
}
