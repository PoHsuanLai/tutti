//! The Links and the Columns of a Resource's Property Data (M2-103 §13, §14.5).

use super::super::limits::{PropertyName, ResourceId, ResourceName};
use super::super::tree::{PeJson, PeObject};
use super::fields::{Fields, LinkRole, PeDataError, PeDataFault, PeExtensions, boolean, text};
use super::kind::PeResource;

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
        if let Some(role) = &self.role {
            object.insert("role", role.as_str().into());
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
        let role = f
            .optional("role", "a string", text)?
            .map(|t| {
                LinkRole::new(t).ok_or_else(|| f.fail("role", PeDataFault::NotValid("link role")))
            })
            .transpose()?;
        Ok(Self {
            resource,
            res_id,
            title,
            role,
            extensions: f.finish()?,
        })
    }
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
