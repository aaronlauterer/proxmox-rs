//! `#[api]` macro for `struct` types.
//!
//! This module implements struct handling.
//!
//! We distinguish between 3 types at the moment:
//! 1) Unit structs (`struct Foo;`),which don't do much really and aren't very useful for the API
//!    currently)
//! 2) Newtypes (`struct Foo(T)`), a 1-tuple, which is supposed to be a wrapper for a type `T` and
//!    therefore should implicitly deserialize/serialize to `T`. Currently we only support simple
//!    types for which we "know" the schema type used in the API.
//! 3) Object structs (`struct Foo { ... }`), which declare an `ObjectSchema`.

use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};

use anyhow::Error;

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote_spanned;

use super::attributes::UpdaterFieldAttributes;
use super::Schema;
use crate::api::{self, ObjectEntry, SchemaItem};
use crate::serde;
use crate::util::{self, FieldName, JSONObject, Maybe};

pub fn handle_struct(attribs: JSONObject, stru: syn::ItemStruct) -> Result<TokenStream, Error> {
    match &stru.fields {
        // unit structs, not sure about these?
        syn::Fields::Unit => handle_unit_struct(attribs, stru),
        syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            handle_newtype_struct(attribs, stru)
        }
        syn::Fields::Unnamed(fields) => bail!(
            fields.paren_token.span.open(),
            "api macro does not support tuple structs"
        ),
        syn::Fields::Named(_) => handle_regular_struct(attribs, stru),
    }
}

fn get_struct_description(schema: &mut Schema, stru: &syn::ItemStruct) -> Result<(), Error> {
    if schema.description.is_none() {
        let (doc_comment, doc_span) = util::get_doc_comments(&stru.attrs)?;
        util::derive_descriptions(schema, None, &doc_comment, doc_span)?;
    }

    Ok(())
}

fn handle_unit_struct(attribs: JSONObject, stru: syn::ItemStruct) -> Result<TokenStream, Error> {
    // unit structs, not sure about these?

    let mut schema: Schema = if attribs.is_empty() {
        Schema::empty_object(Span::call_site())
    } else {
        attribs.try_into()?
    };

    get_struct_description(&mut schema, &stru)?;

    let name = &stru.ident;
    let mut schema = finish_schema(schema, &stru, name)?;
    schema.extend(quote_spanned! { name.span() =>
        impl ::proxmox_schema::UpdaterType for #name {
            type Updater = Option<Self>;
        }
    });

    Ok(schema)
}

fn finish_schema(
    schema: Schema,
    stru: &syn::ItemStruct,
    name: &Ident,
) -> Result<TokenStream, Error> {
    let schema = {
        let mut ts = TokenStream::new();
        schema.to_schema(&mut ts)?;
        ts
    };

    Ok(quote_spanned! { name.span() =>
        #stru

        impl ::proxmox_schema::ApiType for #name {
            const API_SCHEMA: ::proxmox_schema::Schema = #schema;
        }
    })
}

fn handle_newtype_struct(attribs: JSONObject, stru: syn::ItemStruct) -> Result<TokenStream, Error> {
    // Ideally we could clone the contained item's schema, but this is "hard", so for now we assume
    // the contained type is a simple type.
    //
    // In order to support "specializing" an already existing type, we'd need to be able to
    // create "linked" schemas. We cannot do this purely via the macro.

    let mut schema: Schema = attribs.try_into()?;
    if let SchemaItem::Inferred(_span) = schema.item {
        // The schema has no `type` and we failed to guess it. Infer it from the contained field!

        let fields = match &stru.fields {
            syn::Fields::Unnamed(fields) => &fields.unnamed,

            // `handle_struct()` verified this!
            _ => panic!("handle_unit_struct on non-unit struct"),
        };
        // this is also part of `handle_struct()`'s verification!
        assert_eq!(
            fields.len(),
            1,
            "handle_unit_struct needs a struct with exactly 1 field"
        );

        // Now infer the type information:
        util::infer_type(&mut schema, &fields[0].ty)?;
    }

    get_struct_description(&mut schema, &stru)?;

    finish_schema(schema, &stru, &stru.ident)
}

fn handle_regular_struct(
    attribs: JSONObject,
    mut stru: syn::ItemStruct,
) -> Result<TokenStream, Error> {
    let mut schema: Schema = if attribs.is_empty() {
        Schema::empty_object(Span::call_site())
    } else {
        attribs.try_into()?
    };

    get_struct_description(&mut schema, &stru)?;

    // sanity check, first get us some quick by-name access to our fields:
    //
    // NOTE: We remove references we're "done with" and in the end fail with a list of extraneous
    // fields if there are any.
    let mut schema_fields: HashMap<String, &mut ObjectEntry> = HashMap::new();

    let mut additional_properties = None;

    // We also keep a reference to the SchemaObject around since we derive missing fields
    // automatically.
    if let SchemaItem::Object(obj) = &mut schema.item {
        additional_properties = obj
            .additional_properties
            .as_ref()
            .and_then(|a| a.to_option_string());
        for field in obj.properties_mut() {
            schema_fields.insert(field.name.as_str().to_string(), field);
        }
    } else {
        error!(schema.span, "structs need an object schema");
    }

    let mut new_fields: Vec<ObjectEntry> = Vec::new();

    let container_attrs = serde::ContainerAttrib::try_from(&stru.attrs[..])?;

    let mut all_of_schemas = TokenStream::new();
    let mut to_remove = Vec::new();

    if let syn::Fields::Named(ref fields) = &stru.fields {
        for field in &fields.named {
            let attrs = serde::FieldAttrib::try_from(&field.attrs[..])?;

            let (name, span) = {
                let ident: &Ident = field
                    .ident
                    .as_ref()
                    .ok_or_else(|| format_err!(field => "field without name?"))?;

                if let Some(renamed) = attrs.rename.clone() {
                    (renamed.value(), ident.span())
                } else if let Some(rename_all) = container_attrs.rename_all {
                    let name = rename_all.apply_to_field(&ident.to_string());
                    (name, ident.span())
                } else {
                    (ident.to_string(), ident.span())
                }
            };

            if additional_properties.as_deref() == Some(name.as_ref()) {
                // we just *skip* the additional properties field, it is supposed to be a flattened
                // `HashMap<String, Value>` collecting all the values that have no schema
                continue;
            }

            match schema_fields.remove(&name) {
                Some(field_def) => {
                    if attrs.flatten {
                        to_remove.push(name.clone());

                        if field_def.schema.description.is_explicit() {
                            error!(
                                field_def.name.span(),
                                "flattened field should not have a description, \
                                 it does not appear in serialized data as a field",
                            );
                        }

                        if field_def.optional.expect_bool() {
                            // openapi & json schema don't exactly have a proper way to represent
                            // this, so we simply refuse:
                            error!(
                                field_def.name.span(),
                                "optional flattened fields are not supported (by JSONSchema)"
                            );
                        }
                    }

                    handle_regular_field(field_def, field, false, &attrs)?;

                    if attrs.flatten {
                        all_of_schemas.extend(quote::quote! {&});
                        field_def.schema.to_schema(&mut all_of_schemas)?;
                        all_of_schemas.extend(quote::quote! {,});
                    }
                }
                None => {
                    let mut field_def = ObjectEntry::new(
                        FieldName::new(name.clone(), span),
                        false,
                        Schema::blank(span),
                    );
                    handle_regular_field(&mut field_def, field, true, &attrs)?;

                    if attrs.flatten {
                        all_of_schemas.extend(quote::quote! {&});
                        field_def.schema.to_schema(&mut all_of_schemas)?;
                        all_of_schemas.extend(quote::quote! {,});
                        to_remove.push(name.clone());
                    } else {
                        new_fields.push(field_def);
                    }
                }
            }
        }
    } else {
        panic!("handle_regular struct without named fields");
    };

    // now error out about all the fields not found in the struct:
    if !schema_fields.is_empty() {
        let bad_fields = util::join(", ", schema_fields.keys());
        error!(
            schema.span,
            "struct does not contain the following fields: {}", bad_fields
        );
    }

    {
        let obj = schema.item.check_object_mut()?;
        // remove flattened fields
        for field in to_remove {
            //if !obj.remove_property_by_ident(&field)
            if let Some(item) = obj.find_property_by_ident_mut(&field) {
                item.flatten_in_struct = true;
            } else {
                error!(
                    schema.span,
                    "internal error: failed to remove property {:?} from object schema", field,
                );
            }
        }

        // add derived fields
        obj.extend_properties(new_fields);
    }

    let updater = {
        let mut derive = false;
        util::retain_derived_items(&mut stru.attrs, |path| {
            if path.is_ident("Updater") {
                derive = true;
                true // FIXME: remove retain again?
            } else {
                true
            }
        });
        if derive {
            let updater =
                derive_updater(stru.clone(), schema.clone(), &mut stru, &container_attrs)?;

            // make sure we don't leave #[updater] attributes on the original struct:
            if let syn::Fields::Named(fields) = &mut stru.fields {
                for field in &mut fields.named {
                    let _ = UpdaterFieldAttributes::from_attributes(&mut field.attrs);
                }
            }

            updater
        } else {
            TokenStream::new()
        }
    };

    let mut output = if all_of_schemas.is_empty() {
        finish_schema(schema, &stru, &stru.ident)?
    } else {
        finish_all_of_struct(schema, &stru, all_of_schemas)?
    };

    output.extend(updater);

    Ok(output)
}

/// If we have flattened fields the struct schema is not the "final" schema, but part of an AllOf
/// schema containing it and all the flattened field schemas.
fn finish_all_of_struct(
    mut schema: Schema,
    stru: &syn::ItemStruct,
    all_of_schemas: TokenStream,
) -> Result<TokenStream, Error> {
    let name = &stru.ident;

    // take out the inner object schema's description
    let description = match schema.description.take().ok() {
        Some(description) => description,
        None => {
            error!(schema.span, "missing description on api type struct");
            syn::LitStr::new("<missing description>", schema.span)
        }
    };
    // and replace it with a "dummy"
    schema.description = Maybe::Derived(syn::LitStr::new(
        &format!("<INNER: {}>", description.value()),
        description.span(),
    ));

    // now check if it even has any fields
    let has_non_flattened_fields = match &schema.item {
        api::SchemaItem::Object(obj) => obj.has_non_flattened_fields(),
        _ => panic!("object schema is not an object schema?"),
    };

    let (inner_schema, inner_schema_ref) = if has_non_flattened_fields {
        // if it does, we need to create an "inner" schema to merge into the AllOf schema
        let obj_schema = {
            let mut ts = TokenStream::new();
            schema.to_schema(&mut ts)?;
            ts
        };

        (
            quote_spanned!(name.span() =>
                const INNER_API_SCHEMA: ::proxmox_schema::Schema = #obj_schema;
            ),
            quote_spanned!(name.span() => &Self::INNER_API_SCHEMA,),
        )
    } else {
        // otherwise it stays empty
        (TokenStream::new(), TokenStream::new())
    };

    Ok(quote_spanned!(name.span() =>
        #stru

        impl #name {
            #inner_schema
        }

        impl ::proxmox_schema::ApiType for #name {
            const API_SCHEMA: ::proxmox_schema::Schema =
                ::proxmox_schema::AllOfSchema::new(
                    #description,
                    &[
                        #inner_schema_ref
                        #all_of_schemas
                    ],
                )
                .schema();
        }
    ))
}

/// Field handling:
///
/// For each field we derive the description from doc-attributes if available.
fn handle_regular_field(
    field_def: &mut ObjectEntry,
    field: &syn::Field,
    derived: bool, // whether this field was missing in the schema
    attrs: &serde::FieldAttrib,
) -> Result<(), Error> {
    let schema: &mut Schema = &mut field_def.schema;

    if schema.description.is_none() {
        let (doc_comment, doc_span) = util::get_doc_comments(&field.attrs)?;
        util::derive_descriptions(schema, None, &doc_comment, doc_span)?;
    }

    util::infer_type(schema, &field.ty)?;

    if util::is_option_type(&field.ty).is_some() {
        if derived {
            field_def.optional = true.into();
        } else if !field_def.optional.expect_bool() {
            error!(&field.ty => "non-optional Option type?");
        }
    } else {
        attrs.check_non_option_type();
    }

    Ok(())
}

/// To derive an `Updater` we make all fields optional and use the `Updater` derive macro with
/// a `target` parameter.
fn derive_updater(
    mut stru: syn::ItemStruct,
    mut schema: Schema,
    original_struct: &mut syn::ItemStruct,
    container_attrs: &serde::ContainerAttrib,
) -> Result<TokenStream, Error> {
    let original_name = &original_struct.ident;
    stru.ident = Ident::new(&format!("{}Updater", stru.ident), stru.ident.span());

    if !util::derives_trait(&original_struct.attrs, "Default") {
        stru.attrs.push(util::make_derive_attribute(
            Span::call_site(),
            quote::quote! { Default },
        ));
    }

    let updater_name = &stru.ident;
    let mut all_of_schemas = TokenStream::new();
    let mut is_empty_impl = TokenStream::new();

    if let syn::Fields::Named(fields) = &mut stru.fields {
        for mut field in std::mem::take(&mut fields.named) {
            match handle_updater_field(
                &mut field,
                &mut schema,
                &mut all_of_schemas,
                &mut is_empty_impl,
                container_attrs,
            ) {
                Ok(FieldAction::Keep) => fields.named.push(field),
                Ok(FieldAction::Skip) => (),
                Err(err) => {
                    crate::add_error(err);
                    fields.named.push(field);
                }
            }
        }
    }

    let mut output = if all_of_schemas.is_empty() {
        finish_schema(schema, &stru, &stru.ident)?
    } else {
        finish_all_of_struct(schema, &stru, all_of_schemas)?
    };

    if !is_empty_impl.is_empty() {
        output.extend(quote::quote!(
            impl ::proxmox_schema::Updater for #updater_name {
                fn is_empty(&self) -> bool {
                    #is_empty_impl
                }
            }
        ));
    }

    output.extend(quote::quote!(
        impl ::proxmox_schema::UpdaterType for #original_name {
            type Updater = #updater_name;
        }
    ));

    Ok(output)
}

enum FieldAction {
    Keep,
    Skip,
}

fn handle_updater_field(
    field: &mut syn::Field,
    schema: &mut Schema,
    all_of_schemas: &mut TokenStream,
    is_empty_impl: &mut TokenStream,
    container_attrs: &serde::ContainerAttrib,
) -> Result<FieldAction, syn::Error> {
    let updater_attrs = UpdaterFieldAttributes::from_attributes(&mut field.attrs);
    let serde_attrs = serde::FieldAttrib::try_from(&field.attrs[..])?;

    let field_name = field.ident.as_ref().expect("unnamed field in FieldsNamed");
    let field_name_string = field_name.to_string();

    let (name, name_span) = {
        let ident: &Ident = field
            .ident
            .as_ref()
            .ok_or_else(|| format_err!(&field => "field without name?"))?;

        if let Some(renamed) = serde_attrs.rename.clone() {
            (renamed.value(), ident.span())
        } else if let Some(rename_all) = container_attrs.rename_all {
            let name = rename_all.apply_to_field(&ident.to_string());
            (name, ident.span())
        } else {
            (ident.to_string(), ident.span())
        }
    };

    if updater_attrs.skip() {
        if !schema.remove_obj_property_by_ident(&name)
            && !schema.remove_obj_property_by_ident(&field_name_string)
        {
            bail!(name_span, "failed to find schema entry for {:?}", name);
        }
        return Ok(FieldAction::Skip);
    }

    let field_schema = match schema.find_obj_property_by_ident_mut(&name) {
        Some(obj) => obj,
        None => match schema.find_obj_property_by_ident_mut(&field_name_string) {
            Some(obj) => obj,
            None => {
                bail!(
                    field_name.span(),
                    "failed to find schema entry for {:?}",
                    field_name_string,
                );
            }
        },
    };

    let span = Span::call_site();
    field_schema.optional = field.ty.clone().into();
    let updater = match updater_attrs.ty() {
        Some(ty) => ty.clone(),
        None => {
            syn::TypePath {
                qself: Some(syn::QSelf {
                    lt_token: syn::token::Lt { spans: [span] },
                    ty: Box::new(field.ty.clone()),
                    position: 2, // 'Updater' is item index 2 in the 'segments' below
                    as_token: Some(syn::token::As { span }),
                    gt_token: syn::token::Gt { spans: [span] },
                }),
                path: util::make_path(span, true, &["proxmox_schema", "UpdaterType", "Updater"]),
            }
        }
    };

    updater_attrs.replace_serde_attributes(&mut field.attrs);

    // we also need to update the schema to point to the updater's schema for `type: Foo` entries
    if let SchemaItem::ExternType(path) = &mut field_schema.schema.item {
        *path = syn::ExprPath {
            attrs: Vec::new(),
            qself: updater.qself.clone(),
            path: updater.path.clone(),
        };
    }

    field.ty = syn::Type::Path(updater);

    if field_schema.flatten_in_struct {
        let updater_ty = &field.ty;
        all_of_schemas
            .extend(quote::quote! {&<#updater_ty as ::proxmox_schema::ApiType>::API_SCHEMA,});
    }

    if !is_empty_impl.is_empty() {
        is_empty_impl.extend(quote::quote! { && });
    }
    is_empty_impl.extend(quote::quote! {
        self.#field_name.is_empty()
    });

    Ok(FieldAction::Keep)
}
