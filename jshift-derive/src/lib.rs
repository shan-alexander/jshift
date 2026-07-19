use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Ident, Type};

#[proc_macro_derive(JsonMutatorSchema, attributes(json))]
pub fn derive_json_mutator_schema(input: TokenStream) -> TokenStream {
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
                "JsonMutatorSchema can only be derived on structs",
            ));
        }
    };

    let fields = match &data.fields {
        Fields::Named(f) => &f.named,
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "JsonMutatorSchema can only be derived on structs with named fields",
            ));
        }
    };

    let mut field_reads = Vec::new();
    let mut mutator_setters = Vec::new();
    let mut path_statics = Vec::new();

    for field in fields {
        let field_name = field.ident.as_ref().ok_or_else(|| {
            syn::Error::new_spanned(field, "JsonMutatorSchema requires named fields")
        })?;
        let field_type = &field.ty;
        let raw_path = get_json_path(field)?;

        let path_segments = parse_path_segments(&raw_path).map_err(|msg| {
            syn::Error::new_spanned(
                field,
                format!("invalid #[json(path = ...)] value `{raw_path}`: {msg}"),
            )
        })?;

        let path_const_name = Ident::new(
            &format!("__JSHIFT_PATH_{}", field_name.to_string().to_uppercase()),
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
        });

        let is_option = is_option_type(field_type);
        let setter_name = Ident::new(&format!("set_{}", field_name), field_name.span());

        if is_option {
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
        }

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

    Ok(quote! {
        impl #struct_name {
            #(#path_statics)*

            pub fn read_from_json(json: &[u8]) -> Result<Self, jshift::Error> {
                Ok(Self {
                    #(#field_reads),*
                })
            }

            pub fn mutator(json: &mut Vec<u8>) -> #mutator_name {
                #mutator_name { json }
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

fn get_json_path(field: &syn::Field) -> Result<String, syn::Error> {
    let mut path_str = None;
    for attr in &field.attrs {
        if attr.path().is_ident("json") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("path") {
                    let value = meta.value()?;
                    let lit: syn::LitStr = value.parse()?;
                    path_str = Some(lit.value());
                    Ok(())
                } else {
                    Err(meta.error("unsupported json attribute; expected `path = \"...\"`"))
                }
            })?;
        }
    }

    Ok(path_str.unwrap_or_else(|| {
        field
            .ident
            .as_ref()
            .expect("named field")
            .to_string()
    }))
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

/// Strict path parse for compile-time constants (mirrors `try_parse_path`).
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
