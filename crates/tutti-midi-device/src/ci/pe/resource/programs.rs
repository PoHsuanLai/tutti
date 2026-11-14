//! ProgramList (M2-107 §2) and ModeList (M2-106 §2).

use super::super::tree::{PeJson, PeObject};
use super::channels::list_type;
use super::fields::{
    Fields, ModeId, PeBankProgram, PeDataError, PeDataFault, PeExtensions, PeText, list,
    required_text, text,
};
use super::link::PeLink;

/// A Program of a Program Collection (M2-107 Table 2).
#[derive(Clone, Debug, PartialEq)]
pub struct PeProgramEntry {
    /// The name of the Program.
    pub title: PeText,
    /// The Bank Select and Program Change that recall it, 0-based.
    pub bank_program: PeBankProgram,
    /// The categories that suit it (`category`).
    pub categories: Vec<String>,
    /// The tags that describe it.
    pub tags: Vec<String>,
    /// Links to Resources about the Program, Actions among them (M2-103 §13.1.1,
    /// Table 69).
    pub links: Vec<PeLink>,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeProgramEntry {
    /// A Program with no category and no tag.
    #[must_use]
    pub fn new(title: PeText, bank_program: PeBankProgram) -> Self {
        Self {
            title,
            bank_program,
            categories: Vec::new(),
            tags: Vec::new(),
            links: Vec::new(),
            extensions: PeExtensions::default(),
        }
    }

    fn to_json(&self) -> PeJson {
        let mut o = PeObject::new();
        o.insert("title", self.title.as_str().into());
        o.insert("bankPC", self.bank_program.to_json());
        if !self.categories.is_empty() {
            o.insert("category", strings(&self.categories));
        }
        if !self.tags.is_empty() {
            o.insert("tags", strings(&self.tags));
        }
        if !self.links.is_empty() {
            o.insert(
                "links",
                PeJson::Array(self.links.iter().map(PeLink::to_json).collect()),
            );
        }
        self.extensions.write(&mut o);
        PeJson::Object(o)
    }

    fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "ProgramList entry")?;
        let title = required_text(&mut f, "title")?;
        let bank_program = f.required(
            "bankPC",
            "an array of 3 data bytes",
            PeBankProgram::from_json,
        )?;
        let categories = list(&mut f, "category", string_item)?;
        let tags = list(&mut f, "tags", string_item)?;
        let links = list(&mut f, "links", |j| {
            PeLink::from_json(j).map_err(|e| e.fault)
        })?;
        Ok(Self {
            title,
            bank_program,
            categories,
            tags,
            links,
            extensions: f.finish()?,
        })
    }
}

fn strings(items: &[String]) -> PeJson {
    PeJson::Array(items.iter().map(|s| s.as_str().into()).collect())
}

fn string_item(json: &PeJson) -> Result<String, PeDataFault> {
    json.as_str()
        .map(str::to_owned)
        .ok_or(PeDataFault::WrongType("a string"))
}

list_type! {
    /// The Property Data of ProgramList: the Programs of one Program Collection
    /// (M2-107 §2.2).
    PeProgramList of PeProgramEntry
}

impl PeProgramList {
    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(self.0.iter().map(PeProgramEntry::to_json).collect())
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        items
            .iter()
            .map(PeProgramEntry::from_json)
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

/// A Mode of the device (M2-106 Table 3).
#[derive(Clone, Debug, PartialEq)]
pub struct PeModeEntry {
    /// The id `CurrentMode` carries.
    pub mode_id: ModeId,
    /// The name of the Mode.
    pub title: PeText,
    /// A description, in CommonMark (M2-103 §6.1.3).
    pub description: Option<String>,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeModeEntry {
    /// A Mode with no description.
    #[must_use]
    pub fn new(mode_id: ModeId, title: PeText) -> Self {
        Self {
            mode_id,
            title,
            description: None,
            extensions: PeExtensions::default(),
        }
    }

    fn to_json(&self) -> PeJson {
        let mut o = PeObject::new();
        o.insert("modeId", self.mode_id.as_str().into());
        o.insert("title", self.title.as_str().into());
        if let Some(d) = &self.description {
            o.insert("description", d.as_str().into());
        }
        self.extensions.write(&mut o);
        PeJson::Object(o)
    }

    fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "ModeList entry")?;
        let id = f.required("modeId", "a string", text)?.to_owned();
        let mode_id =
            ModeId::new(&id).ok_or_else(|| f.fail("modeId", PeDataFault::NotValid("mode id")))?;
        let title = required_text(&mut f, "title")?;
        let description = f.optional("description", "a string", |j| text(j).map(str::to_owned))?;
        Ok(Self {
            mode_id,
            title,
            description,
            extensions: f.finish()?,
        })
    }
}

list_type! {
    /// The Property Data of ModeList (M2-106 §2.2).
    PeModeList of PeModeEntry
}

impl PeModeList {
    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(self.0.iter().map(PeModeEntry::to_json).collect())
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        items
            .iter()
            .map(PeModeEntry::from_json)
            .collect::<Result<_, _>>()
            .map(Self)
    }
}
