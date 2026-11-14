//! DeviceInfo and JSONSchema (M2-105 §3, §5).

use tutti_ump::{DeviceIdentity, ManufacturerId, U7};

use super::super::tree::{PeJson, PeObject};
use super::fields::{
    Fields, PeDataError, PeDataFault, PeExtensions, PeText, ints, list, required_text, text,
    u7_array,
};
use super::link::PeLink;

/// The Property Data of DeviceInfo: who the device is (M2-105 Table 5).
///
/// The ids are the ones Discovery carries, and shall match them (§3.2), which
/// [`identity`](Self::identity) lets a caller check against the Discovery Reply.
#[derive(Clone, Debug, PartialEq)]
pub struct PeDeviceInfo {
    /// The manufacturer, family, model and version ids.
    pub identity: DeviceIdentity,
    /// The name of the manufacturer.
    pub manufacturer: PeText,
    /// The name of the product family.
    pub family: PeText,
    /// The name of the model.
    pub model: PeText,
    /// The version, a string the manufacturer defines.
    pub version: PeText,
    /// A unique serial number, when the device has one.
    pub serial_number: Option<String>,
    /// Links to the Resources that affect the whole device (§3.3).
    pub links: Vec<PeLink>,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeDeviceInfo {
    pub(super) fn to_json(&self) -> PeJson {
        let id = &self.identity;
        let mut o = PeObject::new();
        o.insert(
            "manufacturerId",
            ints(&id.manufacturer.sysex_bytes().map(U7::get)),
        );
        o.insert("manufacturer", self.manufacturer.as_str().into());
        o.insert("familyId", ints(&id.family.map(U7::get)));
        o.insert("family", self.family.as_str().into());
        o.insert("modelId", ints(&id.model.map(U7::get)));
        o.insert("model", self.model.as_str().into());
        o.insert("versionId", ints(&id.revision.map(U7::get)));
        o.insert("version", self.version.as_str().into());
        if let Some(serial) = &self.serial_number {
            o.insert("serialNumber", serial.as_str().into());
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

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "DeviceInfo")?;
        let manufacturer_id =
            f.required("manufacturerId", "an array of 3 data bytes", u7_array::<3>)?;
        let manufacturer = ManufacturerId::from_sysex_bytes(manufacturer_id)
            .ok_or_else(|| f.fail("manufacturerId", PeDataFault::NotValid("manufacturer id")))?;
        let family = f.required("familyId", "an array of 2 data bytes", u7_array::<2>)?;
        let model = f.required("modelId", "an array of 2 data bytes", u7_array::<2>)?;
        let revision = f.required("versionId", "an array of 4 data bytes", u7_array::<4>)?;
        let manufacturer_name = required_text(&mut f, "manufacturer")?;
        let family_name = required_text(&mut f, "family")?;
        let model_name = required_text(&mut f, "model")?;
        let version = required_text(&mut f, "version")?;
        let serial_number =
            f.optional("serialNumber", "a string", |j| text(j).map(str::to_owned))?;
        let links = list(&mut f, "links", |j| {
            PeLink::from_json(j).map_err(|e| e.fault)
        })?;
        Ok(Self {
            identity: DeviceIdentity {
                manufacturer,
                family,
                model,
                revision,
            },
            manufacturer: manufacturer_name,
            family: family_name,
            model: model_name,
            version,
            serial_number,
            links,
            extensions: f.finish()?,
        })
    }
}

/// The Property Data of JSONSchema: a JSON Schema, an object (M2-105 §5).
#[derive(Clone, Debug, PartialEq)]
pub struct PeJsonSchema(PeObject);

impl PeJsonSchema {
    /// The schema `json` is, or `None` unless it is an object.
    #[must_use]
    pub fn new(json: PeJson) -> Option<Self> {
        match json {
            PeJson::Object(object) => Some(Self(object)),
            _ => None,
        }
    }

    /// The schema.
    #[must_use]
    pub fn as_object(&self) -> &PeObject {
        &self.0
    }

    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Object(self.0.clone())
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        Self::new(json.clone())
            .ok_or_else(|| PeDataError::new("JSONSchema", PeDataFault::NotAnObject))
    }
}
