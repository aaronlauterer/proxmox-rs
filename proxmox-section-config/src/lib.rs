//! Configuration file helper class
//!
//! The `SectionConfig` class implements a configuration parser using
//! API Schemas. A Configuration may contain several sections, each
//! identified by an unique ID. Each section has a type, which is
//! associcatied with a Schema.
//!
//! The file syntax is configurable, but defaults to the following syntax:
//!
//! ```text
//! <section1_type>: <section1_id>
//!     <key1> <value1>
//!     <key2> <value2>
//!     ...
//!
//! <section2_type>: <section2_id>
//!     <key1> <value1>
//!     ...
//! ```

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, format_err, Error};
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use serde_json::{json, Value};

use proxmox_lang::try_block;
use proxmox_schema::format::{dump_properties, wrap_text, ParameterDisplayStyle};
use proxmox_schema::*;

pub mod typed;

/// Used for additional properties when the schema allows them.
const ADDITIONAL_PROPERTY_SCHEMA: Schema = StringSchema::new("Additional property").schema();

/// Associates a section type name with a `Schema`.
pub struct SectionConfigPlugin {
    type_name: String,
    properties: &'static (dyn ObjectSchemaType + Send + Sync + 'static),
    id_property: Option<String>,
    type_key: Option<&'static str>,
}

impl SectionConfigPlugin {
    pub fn new(
        type_name: String,
        id_property: Option<String>,
        properties: &'static (dyn ObjectSchemaType + Send + Sync + 'static),
    ) -> Self {
        Self {
            type_name,
            properties,
            id_property,
            type_key: None,
        }
    }

    /// Override the type key for this plugin.
    pub const fn with_type_key(mut self, type_key: &'static str) -> Self {
        self.type_key = Some(type_key);
        self
    }

    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    pub fn id_property(&self) -> Option<&str> {
        self.id_property.as_deref()
    }

    pub fn properties(&self) -> &(dyn ObjectSchemaType + Send + Sync + 'static) {
        self.properties
    }

    pub fn get_id_schema(&self) -> Option<&Schema> {
        match &self.id_property {
            Some(id_prop) => {
                if let Some((_, schema)) = self.properties.lookup(id_prop) {
                    Some(schema)
                } else {
                    None
                }
            }
            None => None,
        }
    }
}

/// Configuration parser and writer using API Schemas
pub struct SectionConfig {
    plugins: HashMap<String, SectionConfigPlugin>,

    id_schema: &'static Schema,
    parse_section_header: fn(&str) -> Option<(String, String)>,
    parse_section_content: fn(&str) -> Option<(String, String)>,
    format_section_header:
        fn(type_name: &str, section_id: &str, data: &Value) -> Result<String, Error>,
    format_section_content:
        fn(type_name: &str, section_id: &str, key: &str, value: &Value) -> Result<String, Error>,

    allow_unknown_sections: bool,
    type_key: Option<&'static str>,
}

enum ParseState<'a> {
    BeforeHeader,
    InsideSection(&'a SectionConfigPlugin, String, Value),
    InsideUnknownSection(String, String, Value),
}

/// Interface to manipulate configuration data
#[derive(Debug, Clone)]
pub struct SectionConfigData {
    pub sections: HashMap<String, (String, Value)>,
    pub order: Vec<String>,
}

impl Default for SectionConfigData {
    fn default() -> Self {
        Self::new()
    }
}

impl SectionConfigData {
    /// Creates a new instance without any data.
    pub fn new() -> Self {
        Self {
            sections: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Set data for the specified section.
    ///
    /// Please not that this does not verify the data with the
    /// schema. Instead, verification is done inside `write()`.
    pub fn set_data<T: Serialize>(
        &mut self,
        section_id: &str,
        type_name: &str,
        config: T,
    ) -> Result<(), Error> {
        let json = serde_json::to_value(config)?;
        self.sections
            .insert(section_id.to_string(), (type_name.to_string(), json));
        Ok(())
    }

    /// Lookup section data as json `Value`.
    pub fn lookup_json(&self, type_name: &str, id: &str) -> Result<Value, Error> {
        match self.sections.get(id) {
            Some((section_type_name, config)) => {
                if type_name != section_type_name {
                    bail!(
                        "got unexpected type '{}' for {} '{}'",
                        section_type_name,
                        type_name,
                        id
                    );
                }
                Ok(config.clone())
            }
            None => {
                bail!("no such {} '{}'", type_name, id);
            }
        }
    }

    /// Lookup section data as native rust data type.
    pub fn lookup<T: DeserializeOwned>(&self, type_name: &str, id: &str) -> Result<T, Error> {
        let config = self.lookup_json(type_name, id)?;
        let data = T::deserialize(config)?;
        Ok(data)
    }

    /// Record section ordering
    ///
    /// Sections are written in the recorder order.
    pub fn record_order(&mut self, section_id: &str) {
        self.order.push(section_id.to_string());
    }

    /// API helper to represent configuration data as array.
    ///
    /// The array representation is useful to display configuration
    /// data with GUI frameworks like ExtJS.
    pub fn convert_to_array(
        &self,
        id_prop: &str,
        digest: Option<&[u8; 32]>,
        skip: &[&str],
    ) -> Value {
        let mut list: Vec<Value> = vec![];

        let digest: Value = match digest {
            Some(v) => hex::encode(v).into(),
            None => Value::Null,
        };

        for (section_id, (_, data)) in &self.sections {
            let mut item = data.clone();
            for prop in skip {
                item.as_object_mut().unwrap().remove(*prop);
            }
            item.as_object_mut()
                .unwrap()
                .insert(id_prop.into(), section_id.clone().into());
            if !digest.is_null() {
                item.as_object_mut()
                    .unwrap()
                    .insert("digest".into(), digest.clone());
            }
            list.push(item);
        }

        list.into()
    }

    /// API helper to represent configuration data as typed array.
    ///
    /// The array representation is useful to display configuration
    /// data with GUI frameworks like ExtJS.
    pub fn convert_to_typed_array<T: DeserializeOwned>(
        &self,
        type_name: &str,
    ) -> Result<Vec<T>, Error> {
        let mut list: Vec<T> = vec![];

        for (section_type, data) in self.sections.values() {
            if section_type == type_name {
                list.push(T::deserialize(data.clone())?);
            }
        }

        Ok(list)
    }
}

impl SectionConfig {
    /// Create a new `SectionConfig` using the default syntax.
    ///
    /// To make it usable, you need to register one or more plugins
    /// with `register_plugin()`.
    pub fn new(id_schema: &'static Schema) -> Self {
        Self {
            plugins: HashMap::new(),
            id_schema,
            parse_section_header: Self::default_parse_section_header,
            parse_section_content: Self::default_parse_section_content,
            format_section_header: Self::default_format_section_header,
            format_section_content: Self::default_format_section_content,
            allow_unknown_sections: false,
            type_key: None,
        }
    }

    /// Create a new `SectionConfig` using the systemd config file syntax.
    pub fn with_systemd_syntax(id_schema: &'static Schema) -> Self {
        Self {
            plugins: HashMap::new(),
            id_schema,
            parse_section_header: Self::systemd_parse_section_header,
            parse_section_content: Self::systemd_parse_section_content,
            format_section_header: Self::systemd_format_section_header,
            format_section_content: Self::systemd_format_section_content,
            allow_unknown_sections: false,
            type_key: None,
        }
    }

    /// Create a new `SectionConfig` using a custom syntax.
    pub fn with_custom_syntax(
        id_schema: &'static Schema,
        parse_section_header: fn(&str) -> Option<(String, String)>,
        parse_section_content: fn(&str) -> Option<(String, String)>,
        format_section_header: fn(
            type_name: &str,
            section_id: &str,
            data: &Value,
        ) -> Result<String, Error>,
        format_section_content: fn(
            type_name: &str,
            section_id: &str,
            key: &str,
            value: &Value,
        ) -> Result<String, Error>,
    ) -> Self {
        Self {
            plugins: HashMap::new(),
            id_schema,
            parse_section_header,
            parse_section_content,
            format_section_header,
            format_section_content,
            allow_unknown_sections: false,
            type_key: None,
        }
    }

    pub const fn allow_unknown_sections(mut self, allow_unknown_sections: bool) -> Self {
        self.allow_unknown_sections = allow_unknown_sections;
        self
    }

    /// The default type key for all and unknown section types.
    pub const fn with_type_key(mut self, type_key: &'static str) -> Self {
        self.type_key = Some(type_key);
        self
    }

    /// Register a plugin, which defines the `Schema` for a section type.
    pub fn register_plugin(&mut self, plugin: SectionConfigPlugin) {
        self.plugins.insert(plugin.type_name.clone(), plugin);
    }

    /// Access plugin config
    pub fn plugins(&self) -> &HashMap<String, SectionConfigPlugin> {
        &self.plugins
    }

    /// Write the configuration data to a String.
    ///
    /// This verifies the whole data using the schemas defined in the
    /// plugins. Please note that `filename` is only used to improve
    /// error messages.
    pub fn write<P: AsRef<Path>>(
        &self,
        filename: P,
        config: &SectionConfigData,
    ) -> Result<String, Error> {
        self.write_do(config)
            .map_err(|e: Error| format_err!("writing {:?} failed: {}", filename.as_ref(), e))
    }

    fn write_do(&self, config: &SectionConfigData) -> Result<String, Error> {
        let mut list = Vec::new();

        let mut done = HashSet::new();

        for section_id in &config.order {
            if !config.sections.contains_key(section_id) {
                continue;
            };
            list.push(section_id);
            done.insert(section_id);
        }

        for section_id in config.sections.keys() {
            if done.contains(section_id) {
                continue;
            };
            list.push(section_id);
        }

        let mut raw = String::new();

        for section_id in list {
            let (type_name, section_config) = config.sections.get(section_id).unwrap();

            match self.plugins.get(type_name) {
                Some(plugin) => {
                    let id_schema = plugin.get_id_schema().unwrap_or(self.id_schema);
                    if let Err(err) = id_schema.parse_simple_value(section_id) {
                        bail!("syntax error in section identifier: {}", err.to_string());
                    }
                    if section_id.chars().any(|c| c.is_control()) {
                        bail!("detected unexpected control character in section ID.");
                    }
                    if let Err(err) = plugin.properties.verify_json(section_config) {
                        bail!("verify section '{}' failed - {}", section_id, err);
                    }

                    if !raw.is_empty() {
                        raw += "\n"
                    }

                    raw += &(self.format_section_header)(type_name, section_id, section_config)?;

                    for (key, value) in section_config.as_object().unwrap() {
                        if plugin.id_property.as_deref() == Some(key)
                            || plugin.type_key == Some(key)
                            || (plugin.type_key.is_none() && self.type_key == Some(key))
                        {
                            continue; // id and type are part of the section header
                        }
                        raw += &(self.format_section_content)(type_name, section_id, key, value)?;
                    }
                }
                None if self.allow_unknown_sections => {
                    if section_id.chars().any(|c| c.is_control()) {
                        bail!("detected unexpected control character in section ID.");
                    }

                    if !raw.is_empty() {
                        raw += "\n"
                    }

                    raw += &(self.format_section_header)(type_name, section_id, section_config)?;

                    for (key, value) in section_config.as_object().unwrap() {
                        raw += &(self.format_section_content)(type_name, section_id, key, value)?;
                    }
                }
                None => {
                    bail!("unknown section type '{type_name}'");
                }
            }
        }

        Ok(raw)
    }

    /// Parse configuration data.
    ///
    /// This verifies the whole data using the schemas defined in the
    /// plugins. Please note that `filename` is only used to improve
    /// error messages.
    pub fn parse<P: AsRef<Path>>(
        &self,
        filename: P,
        raw: &str,
    ) -> Result<SectionConfigData, Error> {
        let mut state = ParseState::BeforeHeader;

        let test_required_properties = |value: &Value,
                                        schema: &(dyn ObjectSchemaType + Send + Sync),
                                        id_property: &Option<String>|
         -> Result<(), Error> {
            for (name, optional, _prop_schema) in schema.properties() {
                if let Some(id_property) = id_property {
                    if name == id_property {
                        // the id_property is the section header, skip for requirement check
                        continue;
                    }
                }
                if !*optional && value[name] == Value::Null {
                    return Err(format_err!(
                        "property '{}' is missing and it is not optional.",
                        name
                    ));
                }
            }
            Ok(())
        };

        let mut line_no = 0;

        try_block!({
            let mut result = SectionConfigData::new();

            try_block!({
                for line in raw.lines() {
                    line_no += 1;

                    match state {
                        ParseState::BeforeHeader => {
                            if line.trim().is_empty() {
                                continue;
                            }

                            if let Some((section_type, section_id)) =
                                (self.parse_section_header)(line)
                            {
                                //println!("OKLINE: type: {} ID: {}", section_type, section_id);

                                if let Some(plugin) = self.plugins.get(&section_type) {
                                    let section_data =
                                        if let Some(type_key) = plugin.type_key.or(self.type_key) {
                                            json!({type_key: section_type})
                                        } else {
                                            json!({})
                                        };

                                    let id_schema =
                                        plugin.get_id_schema().unwrap_or(self.id_schema);
                                    if let Err(err) = id_schema.parse_simple_value(&section_id) {
                                        bail!(
                                            "syntax error in section identifier: {}",
                                            err.to_string()
                                        );
                                    }
                                    state =
                                        ParseState::InsideSection(plugin, section_id, section_data);
                                } else if self.allow_unknown_sections {
                                    let section_data = if let Some(type_key) = self.type_key {
                                        json!({type_key: section_type})
                                    } else {
                                        json!({})
                                    };
                                    state = ParseState::InsideUnknownSection(
                                        section_type,
                                        section_id,
                                        section_data,
                                    );
                                } else {
                                    bail!("unknown section type '{}'", section_type);
                                }
                            } else {
                                bail!("syntax error (expected header)");
                            }
                        }
                        ParseState::InsideSection(plugin, ref mut section_id, ref mut config) => {
                            if line.trim().is_empty() {
                                // finish section
                                test_required_properties(
                                    config,
                                    plugin.properties,
                                    &plugin.id_property,
                                )?;
                                if let Some(id_property) = &plugin.id_property {
                                    config[id_property] = Value::from(section_id.clone());
                                }
                                result.set_data(section_id, &plugin.type_name, config.take())?;
                                result.record_order(section_id);

                                state = ParseState::BeforeHeader;
                                continue;
                            }
                            if let Some((key, value)) = (self.parse_section_content)(line) {
                                //println!("CONTENT: key: {} value: {}", key, value);

                                let schema = plugin.properties.lookup(&key);
                                let (is_array, prop_schema) = match schema {
                                    Some((_optional, Schema::Array(ArraySchema { items, .. }))) => {
                                        (true, items)
                                    }
                                    Some((_optional, ref prop_schema)) => (false, prop_schema),
                                    None => match plugin.properties.additional_properties() {
                                        true => (false, &&ADDITIONAL_PROPERTY_SCHEMA),
                                        false => bail!("unknown property '{}'", key),
                                    },
                                };

                                let value = match prop_schema.parse_simple_value(&value) {
                                    Ok(value) => value,
                                    Err(err) => {
                                        bail!("property '{}': {}", key, err.to_string());
                                    }
                                };

                                #[allow(clippy::collapsible_if)] // clearer
                                if is_array {
                                    if config[&key] == Value::Null {
                                        config[key] = json!([value]);
                                    } else {
                                        config[key].as_array_mut().unwrap().push(value);
                                    }
                                } else if config[&key] == Value::Null {
                                    config[key] = value;
                                } else {
                                    bail!("duplicate property '{}'", key);
                                }
                            } else {
                                bail!("syntax error (expected section properties)");
                            }
                        }
                        ParseState::InsideUnknownSection(
                            ref section_type,
                            ref mut section_id,
                            ref mut config,
                        ) => {
                            if line.trim().is_empty() {
                                // finish section
                                result.set_data(section_id, section_type, config.take())?;
                                result.record_order(section_id);

                                state = ParseState::BeforeHeader;
                                continue;
                            }
                            if let Some((key, value)) = (self.parse_section_content)(line) {
                                match &mut config[&key] {
                                    Value::Null => config[key] = json!(value),
                                    // Assume it's an array schema in order to handle actual array
                                    // schemas as good as we can.
                                    Value::String(current) => config[key] = json!([current, value]),
                                    Value::Array(array) => array.push(json!(value)),
                                    other => bail!("got unexpected Value {:?}", other),
                                }
                            } else {
                                bail!("syntax error (expected section properties)");
                            }
                        }
                    }
                }

                match state {
                    ParseState::BeforeHeader => {}
                    ParseState::InsideSection(plugin, ref mut section_id, ref mut config) => {
                        // finish section
                        test_required_properties(config, plugin.properties, &plugin.id_property)?;
                        if let Some(id_property) = &plugin.id_property {
                            config[id_property] = Value::from(section_id.clone());
                        }
                        result.set_data(section_id, &plugin.type_name, config)?;
                        result.record_order(section_id);
                    }
                    ParseState::InsideUnknownSection(
                        ref section_type,
                        ref mut section_id,
                        ref mut config,
                    ) => {
                        // finish section
                        result.set_data(section_id, section_type, config)?;
                        result.record_order(section_id);
                    }
                }

                Ok(())
            })
            .map_err(|e| format_err!("line {} - {}", line_no, e))?;

            Ok(result)
        })
        .map_err(|e: Error| format_err!("parsing {:?} failed: {}", filename.as_ref(), e))
    }

    fn default_format_section_header(
        type_name: &str,
        section_id: &str,
        _data: &Value,
    ) -> Result<String, Error> {
        Ok(format!("{}: {}\n", type_name, section_id))
    }

    fn default_format_section_content(
        _type_name: &str,
        section_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<String, Error> {
        if let Value::Array(array) = value {
            let mut list = String::new();
            for item in array {
                let line = Self::default_format_section_content(_type_name, section_id, key, item)?;
                if !line.is_empty() {
                    list.push_str(&line);
                }
            }
            return Ok(list);
        }

        let text = match value {
            Value::Null => return Ok(String::new()), // return empty string (delete)
            Value::Bool(v) => v.to_string(),
            Value::String(v) => v.to_string(),
            Value::Number(v) => v.to_string(),
            _ => {
                bail!(
                    "got unsupported type in section '{}' key '{}'",
                    section_id,
                    key
                );
            }
        };

        if text.chars().any(|c| c.is_control()) {
            bail!(
                "detected unexpected control character in section '{}' key '{}'",
                section_id,
                key
            );
        }

        Ok(format!("\t{} {}\n", key, text))
    }

    fn default_parse_section_content(line: &str) -> Option<(String, String)> {
        if line.is_empty() {
            return None;
        }

        let first_char = line.chars().next().unwrap();

        if !first_char.is_whitespace() {
            return None;
        }

        let mut kv_iter = line.trim_start().splitn(2, |c: char| c.is_whitespace());

        let key = match kv_iter.next() {
            Some(v) => v.trim(),
            None => return None,
        };

        if key.is_empty() {
            return None;
        }

        let value = match kv_iter.next() {
            Some(v) => v.trim(),
            None => return None,
        };

        Some((key.into(), value.into()))
    }

    fn default_parse_section_header(line: &str) -> Option<(String, String)> {
        if line.is_empty() {
            return None;
        };

        let first_char = line.chars().next().unwrap();

        if !first_char.is_alphabetic() {
            return None;
        }

        let mut head_iter = line.splitn(2, ':');

        let section_type = match head_iter.next() {
            Some(v) => v.trim(),
            None => return None,
        };

        if section_type.is_empty() {
            return None;
        }

        let section_id = match head_iter.next() {
            Some(v) => v.trim(),
            None => return None,
        };

        Some((section_type.into(), section_id.into()))
    }

    fn systemd_format_section_header(
        type_name: &str,
        section_id: &str,
        _data: &Value,
    ) -> Result<String, Error> {
        if type_name != section_id {
            bail!("gut unexpected section type");
        }

        Ok(format!("[{}]\n", section_id))
    }

    fn systemd_format_section_content(
        _type_name: &str,
        section_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<String, Error> {
        if let Value::Array(array) = value {
            let mut list = String::new();
            for item in array {
                let line = Self::systemd_format_section_content(_type_name, section_id, key, item)?;
                if !line.is_empty() {
                    list.push_str(&line);
                }
            }
            return Ok(list);
        }

        let text = match value {
            Value::Null => return Ok(String::new()), // return empty string (delete)
            Value::Bool(v) => v.to_string(),
            Value::String(v) => v.to_string(),
            Value::Number(v) => v.to_string(),
            _ => {
                bail!(
                    "got unsupported type in section '{}' key '{}'",
                    section_id,
                    key
                );
            }
        };

        if text.chars().any(|c| c.is_control()) {
            bail!(
                "detected unexpected control character in section '{}' key '{}'",
                section_id,
                key
            );
        }

        Ok(format!("{}={}\n", key, text))
    }

    fn systemd_parse_section_content(line: &str) -> Option<(String, String)> {
        let line = line.trim_end();

        if line.is_empty() {
            return None;
        }

        if line.starts_with(char::is_whitespace) {
            return None;
        }

        let kvpair: Vec<&str> = line.splitn(2, '=').collect();

        if kvpair.len() != 2 {
            return None;
        }

        let key = kvpair[0].trim_end();
        let value = kvpair[1].trim_start();

        Some((key.into(), value.into()))
    }

    fn systemd_parse_section_header(line: &str) -> Option<(String, String)> {
        let line = line.trim_end();

        if line.is_empty() {
            return None;
        };

        if !line.starts_with('[') || !line.ends_with(']') {
            return None;
        }

        let section = line[1..line.len() - 1].trim();

        if section.is_empty() {
            return None;
        }

        Some((section.into(), section.into()))
    }
}

// cargo test test_section_config1 -- --nocapture
#[test]
fn test_section_config1() {
    let filename = "storage.cfg";

    //let mut file = File::open(filename).expect("file not found");
    //let mut contents = String::new();
    //file.read_to_string(&mut contents).unwrap();

    const PROPERTIES: ObjectSchema = ObjectSchema::new(
        "lvmthin properties",
        &[
            (
                "content",
                true,
                &StringSchema::new("Storage content types.").schema(),
            ),
            (
                "thinpool",
                false,
                &StringSchema::new("LVM thin pool name.").schema(),
            ),
            (
                "vgname",
                false,
                &StringSchema::new("LVM volume group name.").schema(),
            ),
        ],
    );

    let plugin = SectionConfigPlugin::new("lvmthin".to_string(), None, &PROPERTIES);

    const ID_SCHEMA: Schema = StringSchema::new("Storage ID schema.")
        .min_length(3)
        .schema();
    let mut config = SectionConfig::new(&ID_SCHEMA);
    config.register_plugin(plugin);

    let raw = r"

lvmthin: local-lvm
        thinpool data
        vgname pve5
        content rootdir,images

lvmthin: local-lvm2
        thinpool data
        vgname pve5
        content rootdir,images
";

    let res = config.parse(filename, raw);
    println!("RES: {:?}", res);
    let raw = config.write(filename, &res.unwrap());
    println!("CONFIG:\n{}", raw.unwrap());
}

// cargo test test_section_config2 -- --nocapture
#[test]
fn test_section_config2() {
    let filename = "user.cfg";

    const ID_SCHEMA: Schema = StringSchema::new("default id schema.")
        .min_length(3)
        .schema();
    let mut config = SectionConfig::new(&ID_SCHEMA);

    const USER_PROPERTIES: ObjectSchema = ObjectSchema::new(
        "user properties",
        &[
            (
                "email",
                false,
                &StringSchema::new("The e-mail of the user").schema(),
            ),
            (
                "userid",
                true,
                &StringSchema::new("The id of the user (name@realm).")
                    .min_length(3)
                    .schema(),
            ),
        ],
    );

    const GROUP_PROPERTIES: ObjectSchema = ObjectSchema::new(
        "group properties",
        &[
            ("comment", false, &StringSchema::new("Comment").schema()),
            (
                "groupid",
                true,
                &StringSchema::new("The id of the group.")
                    .min_length(3)
                    .schema(),
            ),
        ],
    );

    let plugin = SectionConfigPlugin::new(
        "user".to_string(),
        Some("userid".to_string()),
        &USER_PROPERTIES,
    );
    config.register_plugin(plugin);

    let plugin = SectionConfigPlugin::new(
        "group".to_string(),
        Some("groupid".to_string()),
        &GROUP_PROPERTIES,
    );
    config.register_plugin(plugin);

    let raw = r"

user: root@pam
        email root@example.com

group: mygroup
        comment a very important group
";

    let res = config.parse(filename, raw);
    println!("RES: {:?}", res);
    let raw = config.write(filename, &res.unwrap());
    println!("CONFIG:\n{}", raw.unwrap());
}

#[test]
fn test_section_config_with_all_of_schema() {
    let filename = "storage.cfg";

    const PART1: Schema = ObjectSchema::new(
        "properties 1",
        &[(
            "content",
            true,
            &StringSchema::new("Storage content types.").schema(),
        )],
    )
    .schema();

    const PART2: Schema = ObjectSchema::new(
        "properties 2",
        &[(
            "thinpool",
            false,
            &StringSchema::new("LVM thin pool name.").schema(),
        )],
    )
    .schema();

    const PROPERTIES: AllOfSchema = AllOfSchema::new("properties", &[&PART1, &PART2]);

    let plugin = SectionConfigPlugin::new("lvmthin".to_string(), None, &PROPERTIES);

    const ID_SCHEMA: Schema = StringSchema::new("Storage ID schema.")
        .min_length(3)
        .schema();
    let mut config = SectionConfig::new(&ID_SCHEMA);
    config.register_plugin(plugin);

    let raw = r"lvmthin: local-lvm
	content rootdir,images
	thinpool data

lvmthin: local-lvm2
	content rootdir,images
	thinpool data
";

    let res = config.parse(filename, raw);
    println!("RES: {:?}", res);
    let created = config
        .write(filename, &res.unwrap())
        .expect("failed to write config");
    println!("CONFIG:\n{}", raw);

    assert_eq!(raw, created);
}

#[test]
fn test_section_config_with_additional_properties() {
    let filename = "user.cfg";

    const ID_SCHEMA: Schema = StringSchema::new("default id schema.")
        .min_length(3)
        .schema();
    let mut config = SectionConfig::new(&ID_SCHEMA);
    let mut config_with_additional = SectionConfig::new(&ID_SCHEMA);

    const PROPERTIES: [(&str, bool, &proxmox_schema::Schema); 2] = [
        (
            "email",
            false,
            &StringSchema::new("The e-mail of the user").schema(),
        ),
        (
            "userid",
            true,
            &StringSchema::new("The id of the user (name@realm).")
                .min_length(3)
                .schema(),
        ),
    ];

    const USER_PROPERTIES: ObjectSchema = ObjectSchema::new("user properties", &PROPERTIES);

    const USER_PROPERTIES_WITH_ADDITIONAL: ObjectSchema =
        ObjectSchema::new("user properties with additional", &PROPERTIES)
            .additional_properties(true);

    let plugin = SectionConfigPlugin::new(
        "user".to_string(),
        Some("userid".to_string()),
        &USER_PROPERTIES,
    );
    config.register_plugin(plugin);

    let plugin = SectionConfigPlugin::new(
        "user".to_string(),
        Some("userid".to_string()),
        &USER_PROPERTIES_WITH_ADDITIONAL,
    );
    config_with_additional.register_plugin(plugin);

    let raw = r"

user: root@pam
        email root@example.com
        shinynewoption somevalue
";

    let res = config_with_additional.parse(filename, raw);
    println!("RES: {:?}", res);
    let written = config_with_additional.write(filename, &res.unwrap());
    println!("CONFIG:\n{}", written.unwrap());

    assert!(config.parse(filename, raw).is_err());
    // SectionConfigData doesn't have Clone and it would only be needed here currently.
    let res = config_with_additional.parse(filename, raw);
    assert!(config.write(filename, &res.unwrap()).is_err());
}

#[test]
fn test_section_config_with_unknown_section_types() {
    let filename = "user.cfg";

    const ID_SCHEMA: Schema = StringSchema::new("default id schema.")
        .min_length(3)
        .schema();
    let mut config = SectionConfig::new(&ID_SCHEMA).allow_unknown_sections(true);

    const PROPERTIES: [(&str, bool, &proxmox_schema::Schema); 2] = [
        (
            "email",
            false,
            &StringSchema::new("The e-mail of the user").schema(),
        ),
        (
            "userid",
            true,
            &StringSchema::new("The id of the user (name@realm).")
                .min_length(3)
                .schema(),
        ),
    ];

    const USER_PROPERTIES: ObjectSchema = ObjectSchema::new("user properties", &PROPERTIES);

    let plugin = SectionConfigPlugin::new(
        "user".to_string(),
        Some("userid".to_string()),
        &USER_PROPERTIES,
    );
    config.register_plugin(plugin);

    let raw = r"

user: root@pam
        email root@example.com

token: asdf@pbs!asdftoken
        enable true
        expire 0
";

    let check = |res: SectionConfigData| {
        let (_, token_config) = res.sections.get("root@pam").unwrap();
        assert_eq!(
            *token_config.get("email").unwrap(),
            json!("root@example.com")
        );

        let (token_id, token_config) = res.sections.get("asdf@pbs!asdftoken").unwrap();
        assert_eq!(token_id, "token");
        assert_eq!(*token_config.get("enable").unwrap(), json!("true"));
        assert_eq!(*token_config.get("expire").unwrap(), json!("0"));
    };

    let res = config.parse(filename, raw).unwrap();
    println!("RES: {:?}", res);
    let written = config.write(filename, &res);
    println!("CONFIG:\n{}", written.as_ref().unwrap());

    check(res);

    let res = config.parse(filename, &written.unwrap()).unwrap();
    println!("RES second time: {:?}", res);

    check(res);

    let config = config.allow_unknown_sections(false);

    assert!(config.parse(filename, raw).is_err());
}

#[test]
fn test_section_config_array() {
    let filename = "sync.cfg";

    const PROPERTIES: ObjectSchema = ObjectSchema::new(
        "Dummy sync job properties",
        &[
            (
                "group-filter",
                true,
                &ArraySchema::new(
                    "Group filter array schema",
                    &StringSchema::new("Group filter entry schema.").schema(),
                )
                .schema(),
            ),
            (
                "schedule",
                true,
                &StringSchema::new("Remote schema.").schema(),
            ),
        ],
    );

    let plugin = SectionConfigPlugin::new("sync".to_string(), None, &PROPERTIES);

    const ID_SCHEMA: Schema = StringSchema::new("ID schema.").min_length(3).schema();

    let mut config = SectionConfig::new(&ID_SCHEMA);
    config.register_plugin(plugin);

    let config_unknown = SectionConfig::new(&ID_SCHEMA).allow_unknown_sections(true);

    let raw = r"

sync: s-4a1011e8-40e2
        group-filter group:vm/144
        schedule monthly

sync: s-5b2122f9-51f3
        group-filter group:vm/100
        schedule hourly
        group-filter group:vm/102

sync: s-6c32330a-6204
        group-filter group:vm/103
        group-filter group:vm/104
        group-filter group:vm/105
";

    let check = |res: SectionConfigData| {
        let (_, second_section) = res.sections.get("s-5b2122f9-51f3").unwrap();
        assert_eq!(*second_section.get("schedule").unwrap(), json!("hourly"));
        assert_eq!(
            *second_section.get("group-filter").unwrap(),
            json!(["group:vm/100", "group:vm/102"]),
        );

        let (_, third_section) = res.sections.get("s-6c32330a-6204").unwrap();
        assert_eq!(
            *third_section.get("group-filter").unwrap(),
            json!(["group:vm/103", "group:vm/104", "group:vm/105"]),
        );
        assert!(third_section.get("schedule").is_none());
    };

    let res = config.parse(filename, raw).unwrap();
    println!("RES: {:?}", res);
    let written = config.write(filename, &res).unwrap();
    println!("CONFIG:\n{}", written);

    check(res);

    let res = config.parse(filename, &written).unwrap();
    println!("RES (second time): {:?}", res);

    check(res);

    let res_unknown = config_unknown.parse(filename, raw).unwrap();
    println!("RES (unknown): {:?}", res_unknown);
    let written_unknown = config_unknown.write(filename, &res_unknown).unwrap();
    println!("CONFIG (unknown):\n{}", written_unknown);

    check(res_unknown);

    assert_eq!(written, written_unknown);

    let raw = r"

sync: fail
        schedule hourly
        schedule monthly
";

    assert!(config.parse(filename, raw).is_err());
}

/// Generate ReST Documentation for ``SectionConfig``
pub fn dump_section_config(config: &SectionConfig) -> String {
    let mut res = String::new();

    let mut plugins: Vec<&String> = config.plugins().keys().collect();
    plugins.sort_unstable();

    let plugin_count = config.plugins().len();

    for name in plugins {
        let plugin = config.plugins().get(name).unwrap();
        let properties = plugin.properties();
        let skip = match plugin.id_property() {
            Some(id) => vec![id],
            None => Vec::new(),
        };

        if plugin_count > 1 {
            use std::fmt::Write as _;

            let description = wrap_text("", "", properties.description(), 80);
            let _ = write!(res, "\n**Section type** \'``{name}``\':  {description}\n\n");
        }

        res.push_str(&dump_properties(
            properties,
            "",
            ParameterDisplayStyle::Config,
            &skip,
        ));
    }

    res
}
