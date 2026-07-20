use proc_macro::TokenStream;
use quote::quote;
use std::collections::BTreeSet;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Ident, Type};

/// Derive typed JSON path readers/mutators and implement [`jshift::JsonView`].
///
/// Same as [`JsonView`] — kept for backward compatibility.
#[proc_macro_derive(JsonMutatorSchema, attributes(json))]
pub fn derive_json_mutator_schema(input: TokenStream) -> TokenStream {
    derive_inner(input)
}

/// Alias for [`JsonMutatorSchema`]: a Rust type is a projection of JSON bytes.
///
/// Prefer this name when thinking in views / partial records (prost-like messages).
#[proc_macro_derive(JsonView, attributes(json))]
pub fn derive_json_view(input: TokenStream) -> TokenStream {
    derive_inner(input)
}

fn derive_inner(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_derive(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_derive(input: &DeriveInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let struct_name = &input.ident;
    let mutator_name = Ident::new(&format!("{}Mutator", struct_name), struct_name.span());

    let data = match &input.data {
        Data::Struct(s) => s,
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "JsonMutatorSchema / JsonView can only be derived on structs",
            ));
        }
    };

    let fields = match &data.fields {
        Fields::Named(f) => &f.named,
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "JsonMutatorSchema / JsonView can only be derived on structs with named fields",
            ));
        }
    };

    let mut field_reads = Vec::new();
    let mut field_reads_indexed = Vec::new();
    let mut field_reads_doc = Vec::new();
    let mut mutator_setters = Vec::new();
    let mut path_statics = Vec::new();
    let mut write_fields = Vec::new();
    let mut field_path_lits = Vec::new();
    let mut field_jmes_lits = Vec::new();
    let mut has_any_jmes = false;
    let mut array_prefixes: BTreeSet<String> = BTreeSet::new();

    for field in fields {
        let field_name = field.ident.as_ref().ok_or_else(|| {
            syn::Error::new_spanned(field, "JsonMutatorSchema requires named fields")
        })?;
        let field_type = &field.ty;
        let (raw_path, jmes_opt) = get_json_attrs(field)?;
        field_path_lits.push(raw_path.clone());
        let jmes_expr = jmes_opt.clone().unwrap_or_else(|| raw_path.clone());
        field_jmes_lits.push(jmes_expr.clone());
        if jmes_opt.is_some() {
            has_any_jmes = true;
        }

        let path_segments = parse_path_segments(&raw_path).map_err(|msg| {
            syn::Error::new_spanned(
                field,
                format!("invalid #[json(path = ...)] value `{raw_path}`: {msg}"),
            )
        })?;

        for pref in static_array_prefixes_from_segments(&path_segments) {
            array_prefixes.insert(pref);
        }

        let path_const_name = Ident::new(
            &format!("__JSHIFT_PATH_{}", field_name.to_string().to_uppercase()),
            field_name.span(),
        );
        let jmes_const_name = Ident::new(
            &format!("__JSHIFT_JMES_{}", field_name.to_string().to_uppercase()),
            field_name.span(),
        );

        let seg_tokens: Vec<_> = path_segments
            .iter()
            .map(|s| match s {
                DerSegment::Key(k) => quote! { jshift::PathSegment::Key(#k) },
                DerSegment::Index(i) => quote! { jshift::PathSegment::Index(#i) },
            })
            .collect();

        path_statics.push(quote! {
            const #path_const_name: &'static [jshift::PathSegment<'static>] = &[
                #(#seg_tokens),*
            ];
            const #jmes_const_name: &'static str = #jmes_expr;
        });

        let is_option = is_option_type(field_type);
        let setter_name = Ident::new(&format!("set_{}", field_name), field_name.span());
        let uses_jmes_read = jmes_opt.is_some();

        if is_option {
            if uses_jmes_read {
                field_reads.push(quote! {
                    #field_name: {
                        match jshift::project_jmespath(json, Self::#jmes_const_name) {
                            Ok(bytes) => {
                                if bytes == b"null" {
                                    None
                                } else {
                                    jshift::FromJsonSlice::from_json_slice(&bytes)
                                        .ok_or(jshift::Error::TypeMismatch {
                                            expected: stringify!(#field_type),
                                            found: "invalid format",
                                        })?
                                }
                            }
                            Err(jshift::Error::PathNotFound) => None,
                            Err(e) => return Err(e),
                        }
                    }
                });
            } else {
                field_reads.push(quote! {
                    #field_name: {
                        match jshift::find_value(json, Self::#path_const_name) {
                            Ok(slice) => {
                                jshift::FromJsonSlice::from_json_slice(slice)
                                    .ok_or(jshift::Error::TypeMismatch {
                                        expected: stringify!(#field_type),
                                        found: "invalid format",
                                    })?
                            }
                            Err(jshift::Error::PathNotFound) => None,
                            Err(e) => return Err(e),
                        }
                    }
                });
            }
            field_reads_indexed.push(quote! {
                #field_name: {
                    match doc.find(Self::#path_const_name) {
                        Ok(slice) => {
                            jshift::FromJsonSlice::from_json_slice(slice)
                                .ok_or(jshift::Error::TypeMismatch {
                                    expected: stringify!(#field_type),
                                    found: "invalid format",
                                })?
                        }
                        Err(jshift::Error::PathNotFound) => None,
                        Err(e) => return Err(e),
                    }
                }
            });
            field_reads_doc.push(quote! {
                #field_name: {
                    match doc.find(Self::#path_const_name) {
                        Ok(slice) => {
                            jshift::FromJsonSlice::from_json_slice(slice)
                                .ok_or(jshift::Error::TypeMismatch {
                                    expected: stringify!(#field_type),
                                    found: "invalid format",
                                })?
                        }
                        Err(jshift::Error::PathNotFound) => None,
                        Err(e) => return Err(e),
                    }
                }
            });
        } else if uses_jmes_read {
            field_reads.push(quote! {
                #field_name: {
                    let bytes = jshift::project_jmespath(json, Self::#jmes_const_name)?;
                    jshift::FromJsonSlice::from_json_slice(&bytes)
                        .ok_or(jshift::Error::TypeMismatch {
                            expected: stringify!(#field_type),
                            found: "invalid format",
                        })?
                }
            });
            // Indexed find still uses path segments when available.
            field_reads_indexed.push(quote! {
                #field_name: {
                    let bytes = jshift::project_jmespath(doc.as_bytes(), Self::#jmes_const_name)?;
                    jshift::FromJsonSlice::from_json_slice(&bytes)
                        .ok_or(jshift::Error::TypeMismatch {
                            expected: stringify!(#field_type),
                            found: "invalid format",
                        })?
                }
            });
            field_reads_doc.push(quote! {
                #field_name: {
                    let bytes = jshift::project_jmespath(doc.as_bytes(), Self::#jmes_const_name)?;
                    jshift::FromJsonSlice::from_json_slice(&bytes)
                        .ok_or(jshift::Error::TypeMismatch {
                            expected: stringify!(#field_type),
                            found: "invalid format",
                        })?
                }
            });
        } else {
            field_reads.push(quote! {
                #field_name: {
                    let slice = jshift::find_value(json, Self::#path_const_name)?;
                    jshift::FromJsonSlice::from_json_slice(slice)
                        .ok_or(jshift::Error::TypeMismatch {
                            expected: stringify!(#field_type),
                            found: "invalid format",
                        })?
                }
            });
            field_reads_indexed.push(quote! {
                #field_name: {
                    let slice = doc.find(Self::#path_const_name)?;
                    jshift::FromJsonSlice::from_json_slice(slice)
                        .ok_or(jshift::Error::TypeMismatch {
                            expected: stringify!(#field_type),
                            found: "invalid format",
                        })?
                }
            });
            field_reads_doc.push(quote! {
                #field_name: {
                    let slice = doc.find(Self::#path_const_name)?;
                    jshift::FromJsonSlice::from_json_slice(slice)
                        .ok_or(jshift::Error::TypeMismatch {
                            expected: stringify!(#field_type),
                            found: "invalid format",
                        })?
                }
            });
        }

        write_fields.push(quote! {
            {
                let bytes = jshift::ToJsonBytes::to_json_bytes(&self.#field_name);
                jshift::upsert_at_path(json, Self::#path_const_name, &bytes)?;
            }
        });

        mutator_setters.push(quote! {
            pub fn #setter_name(&mut self, val: &(impl jshift::ToJsonBytes + ?Sized)) -> Result<(), jshift::Error> {
                let bytes = jshift::ToJsonBytes::to_json_bytes(val);
                jshift::mutate_value(self.json, #struct_name::#path_const_name, &bytes)
            }
        });

        if is_vec_type(field_type) {
            let append_name = Ident::new(&format!("append_{}", field_name), field_name.span());
            mutator_setters.push(quote! {
                pub fn #append_name(&mut self, val: &(impl jshift::ToJsonBytes + ?Sized)) -> Result<(), jshift::Error> {
                    let bytes = jshift::ToJsonBytes::to_json_bytes(val);
                    jshift::append_to_array(self.json, #struct_name::#path_const_name, &bytes)
                }
            });
        }
    }

    let array_path_lits: Vec<_> = array_prefixes.iter().map(|s| quote! { #s }).collect();
    let field_path_tokens: Vec<_> = field_path_lits.iter().map(|s| quote! { #s }).collect();
    let field_jmes_tokens: Vec<_> = field_jmes_lits.iter().map(|s| quote! { #s }).collect();
    let field_names: Vec<_> = fields
        .iter()
        .filter_map(|f| f.ident.as_ref())
        .map(|id| id.to_string())
        .collect();
    let field_name_tokens: Vec<_> = field_names.iter().map(|s| quote! { #s }).collect();

    let project_plan_body = if has_any_jmes {
        quote! {
            {
                let mut fields = Vec::new();
                let names: &[&str] = &[#(#field_name_tokens),*];
                let exprs: &[&str] = Self::FIELD_JMES;
                for (name, expr) in names.iter().zip(exprs.iter()) {
                    let sel = jshift::parse_jmespath_expr(expr)
                        .expect("FIELD_JMES entries must be valid JMESPath");
                    fields.push(jshift::HashField::new((*name).to_string(), sel));
                }
                jshift::ProjectPlan::from_select(jshift::SelectExpr::MultiSelectHash(fields))
            }
        }
    } else {
        quote! {
            jshift::ProjectPlan::from_paths(Self::FIELD_PATHS)
                .expect("FIELD_PATHS must be valid project paths")
        }
    };

    Ok(quote! {
        impl #struct_name {
            #(#path_statics)*

            /// All `#[json(path = ...)]` paths for this view (schema surface).
            pub const FIELD_PATHS: &'static [&'static str] = &[
                #(#field_path_tokens),*
            ];

            /// Per-field JMESPath (or path fallback) used for schema projection.
            ///
            /// Set with `#[json(jmes = "...")]`. When unset, equals the path string.
            pub const FIELD_JMES: &'static [&'static str] = &[
                #(#field_jmes_tokens),*
            ];

            /// Static array path prefixes inferred from field paths (for auto-index).
            ///
            /// e.g. `products[0].title` contributes `"products"`.
            /// Schema-complete index plan: runtime only builds what paths need.
            pub const INDEXED_ARRAY_PATHS: &'static [&'static str] = &[
                #(#array_path_lits),*
            ];

            pub fn read_from_json(json: &[u8]) -> Result<Self, jshift::Error> {
                Ok(Self {
                    #(#field_reads),*
                })
            }

            /// Build an [`jshift::IndexedDocument`] for this schema's array paths
            /// (Stage-1 structural + array side-tables).
            pub fn indexed_document(json: &[u8]) -> Result<jshift::IndexedDocument<'_>, jshift::Error> {
                let mut doc = jshift::IndexedDocument::empty(json);
                doc.index_structural()?;
                for p in Self::INDEXED_ARRAY_PATHS {
                    doc.index_array_str(p)?;
                }
                Ok(doc)
            }

            /// Schema-guided prepare: same as [`Self::indexed_document`].
            #[inline]
            pub fn prepare(json: &[u8]) -> Result<jshift::IndexedDocument<'_>, jshift::Error> {
                Self::indexed_document(json)
            }

            /// Like [`Self::read_from_json`] but uses [`Self::indexed_document`] so
            /// paths through large arrays jump via side-tables.
            pub fn read_from_json_indexed(json: &[u8]) -> Result<Self, jshift::Error> {
                let doc = Self::indexed_document(json)?;
                Ok(Self {
                    #(#field_reads_indexed),*
                })
            }

            /// Read using a pre-built index (reuses side-tables across views).
            ///
            /// Prefer [`jshift::JsonView::read_from_doc`] in generic code.
            pub fn from_indexed_document(
                doc: &jshift::IndexedDocument<'_>,
            ) -> Result<Self, jshift::Error> {
                Ok(Self {
                    #(#field_reads_doc),*
                })
            }

            /// Upsert all schema fields into `json` (unmentioned paths preserved).
            pub fn write_into_json(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
                #(#write_fields)*
                Ok(())
            }

            /// Rough projected size if only this schema's fields were kept.
            pub fn estimate_projected_len(json: &[u8]) -> Result<usize, jshift::Error> {
                jshift::estimate_projected_len(json, Self::FIELD_PATHS)
            }

            /// Schema project plan: keep-list paths, or multi-select hash when any
            /// field uses `#[json(jmes = "...")]`.
            pub fn schema_project_plan() -> jshift::ProjectPlan {
                #project_plan_body
            }

            /// Project a document down to this schema (new buffer).
            pub fn project_json(json: &[u8]) -> Result<Vec<u8>, jshift::Error> {
                jshift::project(json, &Self::schema_project_plan())
            }

            /// Project using a shared [`jshift::IndexedDocument`] snapshot.
            pub fn project_indexed(doc: &jshift::IndexedDocument<'_>) -> Result<Vec<u8>, jshift::Error> {
                jshift::project_indexed(doc, &Self::schema_project_plan())
            }

            /// Index arrays from the schema plan (if missing), then project.
            pub fn project_indexed_prepare(
                doc: &mut jshift::IndexedDocument<'_>,
            ) -> Result<Vec<u8>, jshift::Error> {
                jshift::project_indexed_prepare(doc, &Self::schema_project_plan())
            }

            /// One-shot: build plan indexes + project (see [`jshift::project_auto_indexed`]).
            pub fn project_auto_indexed(json: &[u8]) -> Result<Vec<u8>, jshift::Error> {
                jshift::project_auto_indexed(json, &Self::schema_project_plan())
            }

            pub fn mutator(json: &mut Vec<u8>) -> #mutator_name {
                #mutator_name { json }
            }
        }

        impl jshift::JsonView for #struct_name {
            #[inline]
            fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
                Self::read_from_json(json)
            }

            #[inline]
            fn read_from_indexed(json: &[u8]) -> Result<Self, jshift::Error> {
                Self::read_from_json_indexed(json)
            }

            #[inline]
            fn read_from_doc(doc: &jshift::IndexedDocument<'_>) -> Result<Self, jshift::Error> {
                Self::from_indexed_document(doc)
            }

            #[inline]
            fn write_into(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
                self.write_into_json(json)
            }

            #[inline]
            fn project_plan() -> jshift::ProjectPlan {
                Self::schema_project_plan()
            }

            #[inline]
            fn project_bytes(json: &[u8]) -> Result<Vec<u8>, jshift::Error> {
                Self::project_json(json)
            }
        }

        pub struct #mutator_name<'a> {
            json: &'a mut Vec<u8>,
        }

        impl<'a> #mutator_name<'a> {
            #(#mutator_setters)*
        }
    })
}

/// Returns `(path, optional jmes)`. Path defaults to the field name.
fn get_json_attrs(field: &syn::Field) -> Result<(String, Option<String>), syn::Error> {
    let mut path_str = None;
    let mut jmes_str = None;
    for attr in &field.attrs {
        if attr.path().is_ident("json") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("path") {
                    let value = meta.value()?;
                    let lit: syn::LitStr = value.parse()?;
                    path_str = Some(lit.value());
                    Ok(())
                } else if meta.path.is_ident("jmes") {
                    let value = meta.value()?;
                    let lit: syn::LitStr = value.parse()?;
                    jmes_str = Some(lit.value());
                    Ok(())
                } else {
                    Err(meta.error(
                        "unsupported json attribute; expected `path = \"...\"` or `jmes = \"...\"`",
                    ))
                }
            })?;
        }
    }

    let path = path_str.unwrap_or_else(|| {
        field
            .ident
            .as_ref()
            .expect("named field")
            .to_string()
    });
    Ok((path, jmes_str))
}

fn is_vec_type(ty: &Type) -> bool {
    type_last_ident(ty).is_some_and(|id| id == "Vec")
}

fn is_option_type(ty: &Type) -> bool {
    type_last_ident(ty).is_some_and(|id| id == "Option")
}

fn type_last_ident(ty: &Type) -> Option<&syn::Ident> {
    if let Type::Path(type_path) = ty {
        type_path.path.segments.last().map(|s| &s.ident)
    } else {
        None
    }
}

enum DerSegment {
    Key(String),
    Index(usize),
}

fn parse_path_segments(s: &str) -> Result<Vec<DerSegment>, &'static str> {
    let mut rest = s;
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if rest.starts_with('.') {
            rest = &rest[1..];
            continue;
        }
        if rest.starts_with('[') {
            match rest.find(']') {
                Some(end_idx) => {
                    let idx_str = &rest[1..end_idx];
                    if idx_str.is_empty() {
                        return Err("empty array index brackets");
                    }
                    if !idx_str.bytes().all(|b| b.is_ascii_digit()) {
                        return Err("non-numeric array index");
                    }
                    let idx = idx_str
                        .parse::<usize>()
                        .map_err(|_| "array index out of range for usize")?;
                    segments.push(DerSegment::Index(idx));
                    rest = &rest[end_idx + 1..];
                }
                None => return Err("unclosed array index bracket '['"),
            }
        } else {
            let end_key = rest.find(['.', '[']).unwrap_or(rest.len());
            let key = &rest[..end_key];
            if !key.is_empty() {
                segments.push(DerSegment::Key(key.to_string()));
            }
            rest = &rest[end_key..];
        }
    }
    Ok(segments)
}

fn static_array_prefixes_from_segments(segs: &[DerSegment]) -> Vec<String> {
    let mut out = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for s in segs {
        match s {
            DerSegment::Key(k) => keys.push(k.as_str()),
            DerSegment::Index(_) => {
                if !keys.is_empty() {
                    out.push(keys.join("."));
                }
                break;
            }
        }
    }
    out
}
