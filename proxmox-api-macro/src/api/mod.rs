//! `#[api]` macro and API schema core module.
//!
//! This contains the `Schema` type, which represents the schema items we have in our api library,
//! but as seen within the `#[api]` macro context.
//!
//! The main job here is to parse our `util::JSONObject` into a `Schema`.
//!
//! The handling of methods vs type definitions happens in their corresponding submodules.

use std::convert::{TryFrom, TryInto};

use anyhow::Error;

use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use syn::parse::{Parse, ParseStream, Parser};
use syn::spanned::Spanned;
use syn::{Expr, ExprPath, Ident};

use crate::util::{FieldName, JSONObject, JSONValue, Maybe};

mod attributes;
mod enums;
mod method;
mod structs;

pub struct IntType {
    pub name: &'static str,
    pub minimum: Option<&'static str>,
    pub maximum: Option<&'static str>,
}

#[rustfmt::skip]
pub const INTTYPES: &[IntType] = &[
    IntType { name: "Integer", minimum: None,                maximum: None,               },
    IntType { name: "i8",      minimum: Some("-0x80"),       maximum: Some("0x7f"),       },
    IntType { name: "i16",     minimum: Some("-0x8000"),     maximum: Some("0x7fff"),     },
    IntType { name: "i32",     minimum: Some("-0x80000000"), maximum: Some("0x7fffffff"), },
    IntType { name: "i64",     minimum: None,                maximum: None,               },
    IntType { name: "isize",   minimum: None,                maximum: None,               },
    IntType { name: "u8",      minimum: Some("0"),           maximum: Some("0xff"),       },
    IntType { name: "u16",     minimum: Some("0"),           maximum: Some("0xffff"),     },
    IntType { name: "u32",     minimum: Some("0"),           maximum: Some("0xffffffff"), },
    IntType { name: "u64",     minimum: Some("0"),           maximum: None,               },
    IntType { name: "usize",   minimum: Some("0"),           maximum: None,               },
];
pub const NUMBERNAMES: &[&str] = &["Number", "f32", "f64"];

/// The main `Schema` type.
///
/// We have 2 fixed keys: `type` and `description`. The remaining keys depend on the `type`.
/// Generally, we create the following mapping:
///
/// ```text
/// {
///     type: Object,
///     description: "text",
///     foo: bar, // "unknown", will be added as a builder-pattern method
///     properties: { ... }
/// }
/// ```
///
/// to:
///
/// ```text
/// {
///     ObjectSchema::new("text", &[ ... ]).foo(bar)
/// }
/// ```
#[derive(Clone)]
pub struct Schema {
    span: Span,

    /// Common in all schema entry types:
    pub description: Maybe<syn::LitStr>,

    /// The specific schema type (Object, String, ...)
    pub item: SchemaItem,

    /// The remaining key-value pairs the `SchemaItem` parser did not extract will be appended as
    /// builder-pattern method calls to this schema.
    properties: Vec<(Ident, syn::Expr)>,
}

/// We parse this in 2 steps: first we parse a `JSONValue`, then we "parse" that further.
impl Parse for Schema {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let obj: JSONObject = input.parse()?;
        Self::try_from(obj)
    }
}

/// Shortcut:
impl TryFrom<JSONValue> for Schema {
    type Error = syn::Error;

    fn try_from(value: JSONValue) -> Result<Self, syn::Error> {
        Self::try_from(value.into_object("a schema definition")?)
    }
}

/// To go from a `JSONObject` to a `Schema` we first extract the description, as it is a common
/// element in all schema entries, then we parse the specific `SchemaItem`, and collect all the
/// remaining "unused" keys as "constraints"/"properties" which will be appended as builder-pattern
/// method calls when translating the object to a schema definition.
impl TryFrom<JSONObject> for Schema {
    type Error = syn::Error;

    fn try_from(mut obj: JSONObject) -> Result<Self, syn::Error> {
        let description = Maybe::explicit(
            obj.remove("description")
                .map(|v| v.try_into())
                .transpose()?,
        );

        Ok(Self {
            span: obj.span(),
            description,
            item: SchemaItem::try_extract_from(&mut obj)?,
            properties: obj
                .into_iter()
                .map(|(key, value)| Ok((key.into_ident(), value.try_into()?)))
                .collect::<Result<_, syn::Error>>()?,
        })
    }
}

impl Schema {
    fn blank(span: Span) -> Self {
        Self {
            span,
            description: Maybe::None,
            item: SchemaItem::Inferred(span),
            properties: Vec::new(),
        }
    }

    fn empty_object(span: Span) -> Self {
        Self {
            span,
            description: Maybe::None,
            item: SchemaItem::Object(SchemaObject::new(span)),
            properties: Vec::new(),
        }
    }

    /// Create the token stream for a reference schema (`ExternType` or `ExternSchema`).
    fn to_schema_reference(&self) -> Option<TokenStream> {
        match &self.item {
            SchemaItem::ExternType(path) => Some(
                quote_spanned! { path.span() => &<#path as ::proxmox_schema::ApiType>::API_SCHEMA },
            ),
            SchemaItem::ExternSchema(path) => Some(quote_spanned! { path.span() => &#path }),
            _ => None,
        }
    }

    fn to_typed_schema(&self, ts: &mut TokenStream) -> Result<(), Error> {
        self.item.to_schema(
            ts,
            self.description.as_ref(),
            self.span,
            &self.properties,
            true,
        )
    }

    fn to_schema(&self, ts: &mut TokenStream) -> Result<(), Error> {
        self.item.to_schema(
            ts,
            self.description.as_ref(),
            self.span,
            &self.properties,
            false,
        )
    }

    fn as_object(&self) -> Option<&SchemaObject> {
        match &self.item {
            SchemaItem::Object(obj) => Some(obj),
            _ => None,
        }
    }

    fn as_object_mut(&mut self) -> Option<&mut SchemaObject> {
        match &mut self.item {
            SchemaItem::Object(obj) => Some(obj),
            _ => None,
        }
    }

    fn find_obj_property_by_ident(&self, key: &str) -> Option<&ObjectEntry> {
        self.as_object()
            .and_then(|obj| obj.find_property_by_ident(key))
    }

    fn find_obj_property_by_ident_mut(&mut self, key: &str) -> Option<&mut ObjectEntry> {
        self.as_object_mut()
            .and_then(|obj| obj.find_property_by_ident_mut(key))
    }

    fn remove_obj_property_by_ident(&mut self, key: &str) -> bool {
        self.as_object_mut()
            .expect("tried to remove object property on non-object schema")
            .remove_property_by_ident(key)
    }

    // FIXME: Should we turn the property list into a map? We used to have no need to find keys in
    // it, but we do now...
    fn find_schema_property(&self, key: &str) -> Option<&syn::Expr> {
        for prop in &self.properties {
            if prop.0 == key {
                return Some(&prop.1);
            }
        }
        None
    }

    pub fn add_default_property(&mut self, key: &str, value: syn::Expr) {
        if self.find_schema_property(key).is_none() {
            self.properties
                .push((Ident::new(key, Span::call_site()), value));
        }
    }
}

#[derive(Clone)]
pub enum SchemaItem {
    Null(Span),
    Boolean(Span),
    Integer(Span),
    Number(Span),
    String(Span),
    Object(SchemaObject),
    Array(SchemaArray),
    ExternType(ExprPath),
    ExternSchema(Expr),
    Inferred(Span),
}

impl SchemaItem {
    pub fn span(&self) -> Span {
        match self {
            SchemaItem::Null(span) => *span,
            SchemaItem::Boolean(span) => *span,
            SchemaItem::Integer(span) => *span,
            SchemaItem::Number(span) => *span,
            SchemaItem::String(span) => *span,
            SchemaItem::Object(inner) => inner.span,
            SchemaItem::Array(inner) => inner.span,
            SchemaItem::ExternType(inner) => inner.span(),
            SchemaItem::ExternSchema(inner) => inner.span(),
            SchemaItem::Inferred(span) => *span,
        }
    }

    /// If there's a `type` specified, parse it as that type. Otherwise check for keys which
    /// uniqueply identify the type, such as "properties" for type `Object`.
    fn try_extract_from(obj: &mut JSONObject) -> Result<Self, syn::Error> {
        if let Some(ext) = obj.remove("schema").map(Expr::try_from).transpose()? {
            return Ok(SchemaItem::ExternSchema(ext));
        }

        let ty = obj.remove("type").map(ExprPath::try_from).transpose()?;
        let ty = match ty {
            Some(ty) => ty,
            None => {
                if obj.contains_key("properties") {
                    return Ok(SchemaItem::Object(SchemaObject::try_extract_from(obj)?));
                } else if obj.contains_key("items") {
                    return Ok(SchemaItem::Array(SchemaArray::try_extract_from(obj)?));
                } else {
                    return Ok(SchemaItem::Inferred(obj.span()));
                }
            }
        };

        if !ty.attrs.is_empty() {
            bail!(ty => "unexpected attributes on type path");
        }

        if ty.qself.is_some() || ty.path.segments.len() != 1 {
            return Ok(SchemaItem::ExternType(ty));
        }

        let name = &ty
            .path
            .segments
            .first()
            .ok_or_else(|| format_err!(&ty.path => "invalid empty path"))?
            .ident;

        if name == "Null" {
            Ok(SchemaItem::Null(ty.span()))
        } else if name == "Boolean" || name == "bool" {
            Ok(SchemaItem::Boolean(ty.span()))
        } else if INTTYPES.iter().any(|n| name == n.name) {
            Ok(SchemaItem::Integer(ty.span()))
        } else if NUMBERNAMES.iter().any(|n| name == n) {
            Ok(SchemaItem::Number(ty.span()))
        } else if name == "String" {
            Ok(SchemaItem::String(ty.span()))
        } else if name == "Object" {
            Ok(SchemaItem::Object(SchemaObject::try_extract_from(obj)?))
        } else if name == "Array" {
            Ok(SchemaItem::Array(SchemaArray::try_extract_from(obj)?))
        } else {
            Ok(SchemaItem::ExternType(ty))
        }
    }

    fn to_inner_schema(
        &self,
        ts: &mut TokenStream,
        description: Maybe<&syn::LitStr>,
        span: Span,
        properties: &[(Ident, syn::Expr)],
    ) -> Result<bool, Error> {
        let check_description =
            move || description.ok_or_else(|| format_err!(span, "missing description"));

        match self {
            SchemaItem::Null(span) => {
                let description = check_description()?;
                ts.extend(quote_spanned! { *span =>
                    ::proxmox_schema::NullSchema::new(#description)
                });
            }
            SchemaItem::Boolean(span) => {
                let description = check_description()?;
                ts.extend(quote_spanned! { *span =>
                    ::proxmox_schema::BooleanSchema::new(#description)
                });
            }
            SchemaItem::Integer(span) => {
                let description = check_description()?;
                ts.extend(quote_spanned! { *span =>
                    ::proxmox_schema::IntegerSchema::new(#description)
                });
            }
            SchemaItem::Number(span) => {
                let description = check_description()?;
                ts.extend(quote_spanned! { *span =>
                    ::proxmox_schema::NumberSchema::new(#description)
                });
            }
            SchemaItem::String(span) => {
                let description = check_description()?;
                ts.extend(quote_spanned! { *span =>
                    ::proxmox_schema::StringSchema::new(#description)
                });
            }
            SchemaItem::Object(obj) => {
                let description = check_description()?;
                let mut elems = TokenStream::new();
                obj.to_schema_inner(&mut elems)?;
                ts.extend(quote_spanned! { obj.span =>
                    ::proxmox_schema::ObjectSchema::new(#description, &[#elems])
                });
                if obj
                    .additional_properties
                    .as_ref()
                    .is_some_and(|a| a.to_bool())
                {
                    ts.extend(quote_spanned! { obj.span => .additional_properties(true) });
                }
            }
            SchemaItem::Array(array) => {
                let description = check_description()?;
                let mut items = TokenStream::new();
                array.to_schema(&mut items)?;
                ts.extend(quote_spanned! { array.span =>
                    ::proxmox_schema::ArraySchema::new(#description, &#items)
                });
            }
            SchemaItem::ExternType(path) => {
                if !properties.is_empty() {
                    error!(&properties[0].0 =>
                        "additional properties not allowed on external type");
                }
                if let Maybe::Explicit(description) = description {
                    error!(description => "description not allowed on external type");
                }

                ts.extend(quote_spanned! { path.span() => <#path as ::proxmox_schema::ApiType>::API_SCHEMA });
                return Ok(true);
            }
            SchemaItem::ExternSchema(path) => {
                if !properties.is_empty() {
                    error!(&properties[0].0 => "additional properties not allowed on schema ref");
                }
                if let Maybe::Explicit(description) = description {
                    error!(description => "description not allowed on external type");
                }

                ts.extend(quote_spanned! { path.span() => #path });
                return Ok(true);
            }
            SchemaItem::Inferred(span) => {
                bail!(*span, "failed to guess 'type' in schema definition");
            }
        }

        // Then append all the remaining builder-pattern properties:
        for prop in properties {
            let key = &prop.0;
            let value = &prop.1;
            ts.extend(quote! { .#key(#value) });
        }

        Ok(false)
    }

    fn to_schema(
        &self,
        ts: &mut TokenStream,
        description: Maybe<&syn::LitStr>,
        span: Span,
        properties: &[(Ident, syn::Expr)],
        typed: bool,
    ) -> Result<(), Error> {
        if typed {
            let _: bool = self.to_inner_schema(ts, description, span, properties)?;
            return Ok(());
        }

        let mut inner_ts = TokenStream::new();
        if self.to_inner_schema(&mut inner_ts, description, span, properties)? {
            ts.extend(inner_ts);
        } else {
            ts.extend(quote! { #inner_ts .schema() });
        }
        Ok(())
    }

    pub fn check_object_mut(&mut self) -> Result<&mut SchemaObject, syn::Error> {
        match self {
            SchemaItem::Object(obj) => Ok(obj),
            _ => bail!(self.span(), "expected object schema, found something else"),
        }
    }
}

#[derive(Clone)]
pub enum OptionType {
    /// All regular api types just have simple boolean expressions for whether the fields in an
    /// object struct are optional. The only exception is updaters where this depends on the
    /// updater type.
    Bool(bool),

    /// An updater type uses its "base" type's field's updaters to determine whether the field is
    /// supposed to be an option.
    #[allow(dead_code)]
    Updater(Box<syn::Type>),
}

impl OptionType {
    pub fn expect_bool(&self) -> bool {
        match self {
            OptionType::Bool(b) => *b,
            _ => panic!(
                "internal error: unexpected Updater dependent 'optional' value in macro context"
            ),
        }
    }
}

impl From<bool> for OptionType {
    fn from(b: bool) -> Self {
        OptionType::Bool(b)
    }
}

impl From<syn::Type> for OptionType {
    fn from(ty: syn::Type) -> Self {
        OptionType::Updater(Box::new(ty))
    }
}

impl ToTokens for OptionType {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match self {
            OptionType::Bool(b) => b.to_tokens(tokens),
            OptionType::Updater(_) => tokens.extend(quote! { true }),
        }
    }
}

#[derive(Clone)]
pub struct ObjectEntry {
    pub name: FieldName,
    pub optional: OptionType,
    pub schema: Schema,

    /// This is only valid for methods. Methods should reset this to false after dealing with it,
    /// as encountering this during schema serialization will always cause an error.
    pub flatten: Option<Span>,

    /// This is used for structs. We mark flattened fields because we need them to be "skipped"
    /// when serializing inner the object schema.
    pub flatten_in_struct: bool,
}

impl ObjectEntry {
    pub fn new(name: FieldName, optional: bool, schema: Schema) -> Self {
        Self {
            name,
            optional: optional.into(),
            schema,
            flatten: None,
            flatten_in_struct: false,
        }
    }

    pub fn with_flatten(mut self, flatten: Option<Span>) -> Self {
        self.flatten = flatten;
        self
    }
}

#[derive(Clone)]
/// Contains a sorted list of properties:
pub struct SchemaObject {
    span: Span,
    properties_: Vec<ObjectEntry>,
    additional_properties: Option<AdditionalProperties>,
}

#[derive(Clone)]
pub enum AdditionalProperties {
    /// `additional_properties: false`.
    No,
    /// `additional_properties: true`.
    Ignored,
    /// `additional_properties: "field_name"`.
    Field(syn::LitStr),
}

impl TryFrom<JSONValue> for AdditionalProperties {
    type Error = syn::Error;

    fn try_from(value: JSONValue) -> Result<Self, Self::Error> {
        let span = value.span();
        if let JSONValue::Expr(syn::Expr::Lit(expr_lit)) = value {
            match expr_lit.lit {
                syn::Lit::Str(s) => return Ok(Self::Field(s)),
                syn::Lit::Bool(b) => {
                    return Ok(if b.value() { Self::Ignored } else { Self::No });
                }
                _ => (),
            }
        }
        bail!(
            span,
            "invalid value for additional_properties, expected boolean or field name"
        );
    }
}

impl AdditionalProperties {
    pub fn to_option_string(&self) -> Option<String> {
        match self {
            Self::Field(name) => Some(name.value()),
            _ => None,
        }
    }

    pub fn to_bool(&self) -> bool {
        !matches!(self, Self::No)
    }
}

impl SchemaObject {
    pub fn new(span: Span) -> Self {
        Self {
            span,
            properties_: Vec::new(),
            additional_properties: None,
        }
    }

    /// Check whether there are any kind of fields defined in the struct, regardless of whether
    /// they're flattened or not.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.properties_.is_empty()
    }

    /// Check whether this object has any fields which aren't being flattened.
    #[inline]
    pub fn has_non_flattened_fields(&self) -> bool {
        // be explicit about how to treat an empty list:
        if self.properties_.is_empty() {
            return false;
        }

        self.properties_.iter().any(|prop| !prop.flatten_in_struct)
    }

    #[inline]
    fn properties_mut(&mut self) -> &mut [ObjectEntry] {
        &mut self.properties_
    }

    fn drain_filter<F>(&mut self, mut func: F) -> Vec<ObjectEntry>
    where
        F: FnMut(&ObjectEntry) -> bool,
    {
        let mut out = Vec::new();

        let mut i = 0;
        while i != self.properties_.len() {
            if !func(&self.properties_[i]) {
                out.push(self.properties_.remove(i));
            } else {
                i += 1;
            }
        }

        out
    }

    fn sort_properties(&mut self) {
        self.properties_.sort_by(|a, b| (a.name).cmp(&b.name));
    }

    fn try_extract_from(obj: &mut JSONObject) -> Result<Self, syn::Error> {
        let mut this = Self {
            span: obj.span(),
            additional_properties: obj
                .remove("additional_properties")
                .map(AdditionalProperties::try_from)
                .transpose()?,
            properties_: obj
                .remove_required_element("properties")?
                .into_object("object field definition")?
                .into_iter()
                .try_fold(
                    Vec::new(),
                    |mut properties, (key, value)| -> Result<_, syn::Error> {
                        let mut schema: JSONObject =
                            value.into_object("schema definition for field")?;

                        let optional: bool = schema
                            .remove("optional")
                            .map(|opt| -> Result<bool, syn::Error> {
                                let v: syn::LitBool = opt.try_into()?;
                                Ok(v.value)
                            })
                            .transpose()?
                            .unwrap_or(false);

                        let flatten: Option<Span> = schema
                            .remove_entry("flatten")
                            .map(|(field, value)| -> Result<(Span, bool), syn::Error> {
                                let v: syn::LitBool = value.try_into()?;
                                Ok((field.span(), v.value))
                            })
                            .transpose()?
                            .and_then(|(span, value)| if value { Some(span) } else { None });

                        properties.push(
                            ObjectEntry::new(key, optional, schema.try_into()?)
                                .with_flatten(flatten),
                        );

                        Ok(properties)
                    },
                )?,
        };
        this.sort_properties();
        Ok(this)
    }

    fn to_schema_inner(&self, ts: &mut TokenStream) -> Result<(), Error> {
        for element in self.properties_.iter() {
            if element.flatten_in_struct {
                continue;
            }

            if let Some(span) = element.flatten {
                error!(
                    span,
                    "`flatten` attribute is only available on method parameters, \
                     use #[serde(flatten)] in structs"
                );
            }

            let key = element.name.as_str();
            let optional = &element.optional;
            let mut schema = TokenStream::new();
            element.schema.to_schema(&mut schema)?;
            ts.extend(quote! { (#key, #optional, &#schema), });
        }
        Ok(())
    }

    fn find_property_by_ident(&self, key: &str) -> Option<&ObjectEntry> {
        self.properties_
            .iter()
            .find(|p| p.name.as_ident_str() == key)
    }

    fn find_property_by_ident_mut(&mut self, key: &str) -> Option<&mut ObjectEntry> {
        self.properties_
            .iter_mut()
            .find(|p| p.name.as_ident_str() == key)
    }

    fn remove_property_by_ident(&mut self, key: &str) -> bool {
        match self
            .properties_
            .iter()
            .position(|p| p.name.as_ident_str() == key)
        {
            Some(idx) => {
                self.properties_.remove(idx);
                true
            }
            None => false,
        }
    }

    fn extend_properties(&mut self, new_fields: Vec<ObjectEntry>) {
        self.properties_.extend(new_fields);
        self.sort_properties();
    }
}

#[derive(Clone)]
pub struct SchemaArray {
    span: Span,
    item: Box<Schema>,
}

impl SchemaArray {
    fn try_extract_from(obj: &mut JSONObject) -> Result<Self, syn::Error> {
        Ok(Self {
            span: obj.span(),
            item: Box::new(obj.remove_required_element("items")?.try_into()?),
        })
    }

    fn to_schema(&self, ts: &mut TokenStream) -> Result<(), Error> {
        self.item.to_schema(ts)
    }
}

/// Parse `input`, `returns` and `protected` attributes out of an function annotated
/// with an `#[api]` attribute and produce a `const ApiMethod` named after the function.
///
/// See the top level macro documentation for a complete example.
pub(crate) fn api(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    let attribs = JSONObject::parse_inner.parse2(attr)?;
    let item: syn::Item = syn::parse2(item)?;

    match item {
        syn::Item::Fn(item) => method::handle_method(attribs, item),
        syn::Item::Struct(item) => structs::handle_struct(attribs, item),
        syn::Item::Enum(item) => enums::handle_enum(attribs, item),
        _ => bail!(item => "api macro only works on functions"),
    }
}

/// Directly convert a json schema into a `Schema` expression.
pub(crate) fn json_schema(item: TokenStream) -> Result<TokenStream, Error> {
    let attribs = JSONObject::parse_inner.parse2(item)?;
    let schema: Schema = attribs.try_into()?;

    let mut ts = TokenStream::new();
    schema.to_schema(&mut ts)?;
    Ok(ts)
}
