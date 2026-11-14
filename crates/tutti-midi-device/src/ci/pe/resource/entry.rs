//! ResourceList (M2-103 §14): the entries that say how each Resource of a device
//! can be used.

use super::super::header::PeEncoding;
use super::super::limits::{MediaType, ResourceName};
use super::super::tree::{PeJson, PeObject};
use super::fields::{Fields, PeDataError, PeDataFault, PeExtensions, list, text};
use super::kind::{CanSet, PeResource};
use super::link::{PeColumn, flag};

/// A ResourceList entry that the rules of Property Exchange refuse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntryError {
    /// The ResourceList does not list itself (M2-103 §14).
    #[error("ResourceList has no entry for itself")]
    ListsItself,
    /// A Resource a specification defines keeps its defaults of `schema`,
    /// `requireResId`, `canPaginate`, `canSet` and `canGet` (M2-103 §14.3).
    #[error("{property} of {resource} is not for an entry to override")]
    NotOverridable {
        /// The Resource.
        resource: String,
        /// The property.
        property: &'static str,
    },
    /// Only a List Resource paginates and has columns (M2-103 §14.2.1).
    #[error("{0} is not a List Resource")]
    NotAList(String),
    /// A manufacturer-specific JSON Resource gives a schema with at least a
    /// `title` (M2-103 §14.4).
    #[error("the schema of {0} has no title")]
    SchemaWithoutTitle(String),
    /// Flow Control on Get needs `canGet`, and on Set needs `canSet`.
    #[error("{0} sets Flow Control for a transaction it does not take")]
    FlowControlWithoutTransaction(String),
    /// An encoding or a media type listed twice.
    #[error("{0} is listed twice")]
    Duplicate(String),
    /// A list of encodings or of media types with nothing in it.
    #[error("an empty list of {0}")]
    Empty(&'static str),
}

/// What a device says of one Resource it has: how it is read, set and
/// subscribed to, and how its Property Data is encoded (M2-103 §14.2). A
/// property not given has the default of the Resource (§14.3), and only a
/// property that differs is written.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceEntry {
    resource: PeResource,
    can_get: bool,
    fc_on_get: bool,
    can_set: CanSet,
    fc_on_set: bool,
    can_subscribe: bool,
    require_res_id: bool,
    can_paginate: bool,
    media_types: Option<Vec<MediaType>>,
    encodings: Option<Vec<PeEncoding>>,
    schema: Option<PeJson>,
    columns: Vec<PeColumn>,
    extensions: PeExtensions,
}

impl ResourceEntry {
    /// An entry for `resource` with every default.
    ///
    /// # Errors
    ///
    /// [`EntryError::ListsItself`] for `ResourceList`, and
    /// [`EntryError::SchemaWithoutTitle`] for a manufacturer-specific Resource,
    /// which has no schema here: make it with [`custom`](Self::custom).
    pub fn new(resource: PeResource) -> Result<Self, EntryError> {
        match &resource {
            PeResource::ResourceList => return Err(EntryError::ListsItself),
            PeResource::Custom(c) => {
                return Err(EntryError::SchemaWithoutTitle(c.as_str().to_owned()));
            }
            _ => {}
        }
        Ok(Self::bare(resource))
    }

    fn bare(resource: PeResource) -> Self {
        let d = resource.defaults();
        Self {
            can_get: d.can_get,
            fc_on_get: false,
            can_set: d.can_set,
            fc_on_set: false,
            can_subscribe: d.can_subscribe,
            require_res_id: d.require_res_id,
            can_paginate: d.can_paginate,
            media_types: None,
            encodings: None,
            schema: None,
            columns: Vec::new(),
            extensions: PeExtensions::default(),
            resource,
        }
    }

    /// An entry for a manufacturer-specific Resource, which gives a JSON
    /// Schema (M2-103 §14.4).
    ///
    /// # Errors
    ///
    /// [`EntryError::SchemaWithoutTitle`] unless `schema` is an object with a
    /// `title` string.
    pub fn custom(resource: PeResource, schema: PeJson) -> Result<Self, EntryError> {
        Self::bare(resource).with_schema(schema)
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        &self.resource
    }

    /// Whether Get reads the Resource.
    #[must_use]
    pub fn can_get(&self) -> bool {
        self.can_get
    }

    /// Whether a Get may use Flow Control.
    #[must_use]
    pub fn fc_on_get(&self) -> bool {
        self.fc_on_get
    }

    /// What Set may do.
    #[must_use]
    pub fn can_set(&self) -> CanSet {
        self.can_set
    }

    /// Whether a Set must use Flow Control.
    #[must_use]
    pub fn fc_on_set(&self) -> bool {
        self.fc_on_set
    }

    /// Whether the Resource can be subscribed to.
    #[must_use]
    pub fn can_subscribe(&self) -> bool {
        self.can_subscribe
    }

    /// Whether an inquiry names an entry with `resId`.
    #[must_use]
    pub fn require_res_id(&self) -> bool {
        self.require_res_id
    }

    /// Whether a Get may page the List Resource.
    #[must_use]
    pub fn can_paginate(&self) -> bool {
        self.can_paginate
    }

    /// The media types of the Property Data: `application/json` unless the
    /// entry lists others.
    #[must_use]
    pub fn media_types(&self) -> Vec<MediaType> {
        self.media_types
            .clone()
            .unwrap_or_else(|| MediaType::new("application/json").into_iter().collect())
    }

    /// The encodings of the Property Data: ASCII unless the entry lists others.
    #[must_use]
    pub fn encodings(&self) -> &[PeEncoding] {
        self.encodings.as_deref().unwrap_or(&[PeEncoding::Ascii])
    }

    /// The JSON Schema, when the entry gives one.
    #[must_use]
    pub fn schema(&self) -> Option<&PeJson> {
        self.schema.as_ref()
    }

    /// The Columns of a List Resource.
    #[must_use]
    pub fn columns(&self) -> &[PeColumn] {
        &self.columns
    }

    /// The manufacturer-specific properties.
    #[must_use]
    pub fn extensions(&self) -> &PeExtensions {
        &self.extensions
    }

    /// Takes the manufacturer-specific properties.
    #[must_use]
    pub fn with_extensions(mut self, extensions: PeExtensions) -> Self {
        self.extensions = extensions;
        self
    }

    fn refuse_override(&self, property: &'static str) -> EntryError {
        EntryError::NotOverridable {
            resource: self.resource.as_str().to_owned(),
            property,
        }
    }

    /// Declares `canGet`.
    ///
    /// # Errors
    ///
    /// [`EntryError::NotOverridable`] for a value other than the default of a
    /// Resource a specification defines.
    pub fn with_can_get(mut self, can_get: bool) -> Result<Self, EntryError> {
        if !self.resource.is_overridable() && can_get != self.resource.defaults().can_get {
            return Err(self.refuse_override("canGet"));
        }
        self.can_get = can_get;
        self.checked()
    }

    /// Declares `canSet`.
    ///
    /// # Errors
    ///
    /// [`EntryError::NotOverridable`] as for [`with_can_get`](Self::with_can_get).
    pub fn with_can_set(mut self, can_set: CanSet) -> Result<Self, EntryError> {
        if !self.resource.is_overridable() && can_set != self.resource.defaults().can_set {
            return Err(self.refuse_override("canSet"));
        }
        self.can_set = can_set;
        self.checked()
    }

    /// Declares `requireResId`.
    ///
    /// # Errors
    ///
    /// [`EntryError::NotOverridable`] as for [`with_can_get`](Self::with_can_get).
    pub fn with_require_res_id(mut self, require: bool) -> Result<Self, EntryError> {
        if !self.resource.is_overridable() && require != self.resource.defaults().require_res_id {
            return Err(self.refuse_override("requireResId"));
        }
        self.require_res_id = require;
        Ok(self)
    }

    /// Declares `canPaginate`.
    ///
    /// # Errors
    ///
    /// [`EntryError::NotOverridable`] as for [`with_can_get`](Self::with_can_get), and
    /// [`EntryError::NotAList`] for a Resource that is no List Resource.
    pub fn with_can_paginate(mut self, paginate: bool) -> Result<Self, EntryError> {
        if !self.resource.is_overridable() && paginate != self.resource.defaults().can_paginate {
            return Err(self.refuse_override("canPaginate"));
        }
        if paginate && !self.resource.is_list() {
            return Err(EntryError::NotAList(self.resource.as_str().to_owned()));
        }
        self.can_paginate = paginate;
        Ok(self)
    }

    /// Declares `canSubscribe`.
    #[must_use]
    pub fn with_can_subscribe(mut self, subscribe: bool) -> Self {
        self.can_subscribe = subscribe;
        self
    }

    /// Declares `fcOnGet`.
    ///
    /// # Errors
    ///
    /// [`EntryError::FlowControlWithoutTransaction`] when the Resource cannot be got.
    pub fn with_fc_on_get(mut self, flow: bool) -> Result<Self, EntryError> {
        self.fc_on_get = flow;
        self.checked()
    }

    /// Declares `fcOnSet`.
    ///
    /// # Errors
    ///
    /// [`EntryError::FlowControlWithoutTransaction`] when the Resource cannot be set.
    pub fn with_fc_on_set(mut self, flow: bool) -> Result<Self, EntryError> {
        self.fc_on_set = flow;
        self.checked()
    }

    /// Declares `encodings`.
    ///
    /// # Errors
    ///
    /// [`EntryError::Empty`] for none and [`EntryError::Duplicate`] for one twice.
    pub fn with_encodings(mut self, encodings: Vec<PeEncoding>) -> Result<Self, EntryError> {
        if encodings.is_empty() {
            return Err(EntryError::Empty("encodings"));
        }
        if let Some(twice) = encodings
            .iter()
            .enumerate()
            .find(|(at, e)| encodings[..*at].contains(e))
        {
            return Err(EntryError::Duplicate(twice.1.as_str().to_owned()));
        }
        self.encodings = Some(encodings);
        Ok(self)
    }

    /// Declares `mediaTypes`.
    ///
    /// # Errors
    ///
    /// [`EntryError::Empty`] for none and [`EntryError::Duplicate`] for one twice.
    pub fn with_media_types(mut self, media_types: Vec<MediaType>) -> Result<Self, EntryError> {
        if media_types.is_empty() {
            return Err(EntryError::Empty("media types"));
        }
        if let Some(twice) = media_types
            .iter()
            .enumerate()
            .find(|(at, m)| media_types[..*at].contains(m))
        {
            return Err(EntryError::Duplicate(twice.1.as_str().to_owned()));
        }
        self.media_types = Some(media_types);
        Ok(self)
    }

    /// Declares `columns`.
    ///
    /// # Errors
    ///
    /// [`EntryError::NotAList`] for a Resource that is no List Resource.
    pub fn with_columns(mut self, columns: Vec<PeColumn>) -> Result<Self, EntryError> {
        if !columns.is_empty() && !self.resource.is_list() {
            return Err(EntryError::NotAList(self.resource.as_str().to_owned()));
        }
        self.columns = columns;
        Ok(self)
    }

    /// Declares `schema`.
    ///
    /// # Errors
    ///
    /// [`EntryError::NotOverridable`] for a Resource a specification defines,
    /// whose schema is the specification's, and [`EntryError::SchemaWithoutTitle`]
    /// unless the schema is an object with a `title` string.
    pub fn with_schema(mut self, schema: PeJson) -> Result<Self, EntryError> {
        if !self.resource.is_overridable() {
            return Err(self.refuse_override("schema"));
        }
        let titled = schema
            .as_object()
            .and_then(|o| o.get("title"))
            .and_then(PeJson::as_str)
            .is_some();
        if !titled {
            return Err(EntryError::SchemaWithoutTitle(
                self.resource.as_str().to_owned(),
            ));
        }
        self.schema = Some(schema);
        Ok(self)
    }

    fn checked(self) -> Result<Self, EntryError> {
        let flow_without =
            (self.fc_on_get && !self.can_get) || (self.fc_on_set && self.can_set == CanSet::None);
        if flow_without {
            return Err(EntryError::FlowControlWithoutTransaction(
                self.resource.as_str().to_owned(),
            ));
        }
        Ok(self)
    }

    pub(super) fn to_json(&self) -> PeJson {
        let d = self.resource.defaults();
        let mut o = PeObject::new();
        o.insert("resource", self.resource.as_str().into());
        let flag_if = |o: &mut PeObject, name: &str, value: bool, default: bool| {
            if value != default {
                o.insert(name, PeJson::Bool(value));
            }
        };
        flag_if(&mut o, "canGet", self.can_get, d.can_get);
        flag_if(&mut o, "fcOnGet", self.fc_on_get, false);
        if self.can_set != d.can_set {
            o.insert("canSet", self.can_set.as_str().into());
        }
        flag_if(&mut o, "fcOnSet", self.fc_on_set, false);
        flag_if(&mut o, "canSubscribe", self.can_subscribe, d.can_subscribe);
        flag_if(
            &mut o,
            "requireResId",
            self.require_res_id,
            d.require_res_id,
        );
        flag_if(&mut o, "canPaginate", self.can_paginate, d.can_paginate);
        if let Some(types) = &self.media_types {
            o.insert(
                "mediaTypes",
                PeJson::Array(types.iter().map(|m| m.as_str().into()).collect()),
            );
        }
        if let Some(encodings) = &self.encodings {
            o.insert(
                "encodings",
                PeJson::Array(encodings.iter().map(|e| e.as_str().into()).collect()),
            );
        }
        if let Some(schema) = &self.schema {
            o.insert("schema", schema.clone());
        }
        if !self.columns.is_empty() {
            o.insert(
                "columns",
                PeJson::Array(self.columns.iter().map(PeColumn::to_json).collect()),
            );
        }
        self.extensions.write(&mut o);
        PeJson::Object(o)
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "resource entry")?;
        let name = f.required("resource", "a string", text)?.to_owned();
        let resource = ResourceName::new(&name)
            .map(|n| PeResource::from_name(&n))
            .ok_or_else(|| f.fail("resource", PeDataFault::NotValid("resource name")))?;
        let refused = |f: &Fields<'_>, name: &'static str, e: EntryError| {
            f.fail(name, PeDataFault::NotValid(entry_what(&e)))
        };
        let mut entry = Self::bare(resource);
        let can_get = flag(&mut f, "canGet", entry.can_get)?;
        entry = entry
            .with_can_get(can_get)
            .map_err(|e| refused(&f, "canGet", e))?;
        let can_set = f
            .optional("canSet", "a string", text)?
            .map(|t| {
                CanSet::new(t).ok_or_else(|| f.fail("canSet", PeDataFault::NotValid("canSet")))
            })
            .transpose()?
            .unwrap_or(entry.can_set);
        entry = entry
            .with_can_set(can_set)
            .map_err(|e| refused(&f, "canSet", e))?;
        let fc_get = flag(&mut f, "fcOnGet", false)?;
        entry = entry
            .with_fc_on_get(fc_get)
            .map_err(|e| refused(&f, "fcOnGet", e))?;
        let fc_set = flag(&mut f, "fcOnSet", false)?;
        entry = entry
            .with_fc_on_set(fc_set)
            .map_err(|e| refused(&f, "fcOnSet", e))?;
        entry.can_subscribe = flag(&mut f, "canSubscribe", entry.can_subscribe)?;
        let require = flag(&mut f, "requireResId", entry.require_res_id)?;
        entry = entry
            .with_require_res_id(require)
            .map_err(|e| refused(&f, "requireResId", e))?;
        let paginate = flag(&mut f, "canPaginate", entry.can_paginate)?;
        entry = entry
            .with_can_paginate(paginate)
            .map_err(|e| refused(&f, "canPaginate", e))?;
        entry.read_rest(f)
    }

    fn read_rest(mut self, mut f: Fields<'_>) -> Result<Self, PeDataError> {
        let types = list(&mut f, "mediaTypes", |j| {
            j.as_str()
                .and_then(MediaType::new)
                .ok_or(PeDataFault::NotValid("media type"))
        })?;
        if !types.is_empty() {
            self = self
                .with_media_types(types)
                .map_err(|e| f.fail("mediaTypes", PeDataFault::NotValid(entry_what(&e))))?;
        }
        let encodings = list(&mut f, "encodings", |j| {
            j.as_str()
                .and_then(encoding_of)
                .ok_or(PeDataFault::NotValid("encoding"))
        })?;
        if !encodings.is_empty() {
            self = self
                .with_encodings(encodings)
                .map_err(|e| f.fail("encodings", PeDataFault::NotValid(entry_what(&e))))?;
        }
        if let Some(schema) =
            f.optional("schema", "an object", |j| j.as_object().map(|_| j.clone()))?
        {
            self = self
                .with_schema(schema)
                .map_err(|e| f.fail("schema", PeDataFault::NotValid(entry_what(&e))))?;
        }
        let columns = list(&mut f, "columns", |j| {
            PeColumn::from_json(j).map_err(|e| e.fault)
        })?;
        self = self
            .with_columns(columns)
            .map_err(|e| f.fail("columns", PeDataFault::NotValid(entry_what(&e))))?;
        self.extensions = f.finish()?;
        Ok(self)
    }
}

/// What an [`EntryError`] refuses, in the words of [`PeDataFault::NotValid`].
fn entry_what(error: &EntryError) -> &'static str {
    match error {
        EntryError::ListsItself => "entry: ResourceList does not list itself",
        EntryError::NotOverridable { .. } => "entry: a default that is not to be overridden",
        EntryError::NotAList(_) => "entry: a List Resource property of a Resource that is none",
        EntryError::SchemaWithoutTitle(_) => "entry: a schema with a title",
        EntryError::FlowControlWithoutTransaction(_) => "entry: Flow Control and its transaction",
        EntryError::Duplicate(_) => "entry: a list without repeats",
        EntryError::Empty(_) => "entry: a list that is not empty",
    }
}

/// The encoding a ResourceList names. The spec spells Mcoded7 with a capital C
/// once (M2-103 Table 82), so the names are read without regard to case.
fn encoding_of(text: &str) -> Option<PeEncoding> {
    [
        PeEncoding::Ascii,
        PeEncoding::Mcoded7,
        PeEncoding::ZlibMcoded7,
    ]
    .into_iter()
    .find(|e| e.as_str().eq_ignore_ascii_case(text))
}

/// The Property Data of ResourceList: an entry for each Resource of the device,
/// not counting ResourceList (M2-103 §14).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeResourceList(Vec<ResourceEntry>);

impl PeResourceList {
    /// A list with no entry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an entry, replacing the one for the same Resource.
    #[must_use]
    pub fn with(mut self, entry: ResourceEntry) -> Self {
        match self.0.iter_mut().find(|e| e.resource == entry.resource) {
            Some(old) => *old = entry,
            None => self.0.push(entry),
        }
        self
    }

    /// The entries, in order.
    #[must_use]
    pub fn entries(&self) -> &[ResourceEntry] {
        &self.0
    }

    /// The entry for `resource`.
    #[must_use]
    pub fn entry(&self, resource: &PeResource) -> Option<&ResourceEntry> {
        self.0.iter().find(|e| e.resource == *resource)
    }

    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(self.0.iter().map(ResourceEntry::to_json).collect())
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        let mut out = Self::new();
        for item in items {
            let entry = ResourceEntry::from_json(item)?;
            if out.entry(&entry.resource).is_some() {
                return Err(PeDataError::new(
                    entry.resource.as_str(),
                    PeDataFault::Conflicts("another entry of the same Resource"),
                ));
            }
            if entry.resource == PeResource::ResourceList {
                return Err(PeDataError::new(
                    "ResourceList",
                    PeDataFault::NotValid("entry: ResourceList does not list itself"),
                ));
            }
            out.0.push(entry);
        }
        Ok(out)
    }
}
