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

    for field in fields {
        let field_name = field.ident.as_ref().ok_or_else(|| {
            syn::Error::new_spanned(field, "JsonMutatorSchema requires named fields")
        })?;
        let field_type = &field.ty;
        let raw_path = get_json_path(field)?;

        if let Err(msg) = validate_path_str(&raw_path) {
            return Err(syn::Error::new_spanned(
                field,
                format!("invalid #[json(path = ...)] value `{raw_path}`: {msg}"),
            ));
        }

        let setter_name = Ident::new(&format!("set_{}", field_name), field_name.span());

        field_reads.push(quote! {
            #field_name: {
                let path = jshift::parse_path(#raw_path);
                let slice = jshift::find_value(json, &path)?;
                jshift::FromJsonSlice::from_json_slice(slice)
                    .ok_or(jshift::Error::TypeMismatch {
                        expected: stringify!(#field_type),
                        found: "invalid format",
                    })?
            }
        });

        mutator_setters.push(quote! {
            pub fn #setter_name(&mut self, val: &(impl jshift::ToJsonBytes + ?Sized)) -> Result<(), jshift::Error> {
                let bytes = jshift::ToJsonBytes::to_json_bytes(val);
                let path = jshift::parse_path(#raw_path);
                jshift::mutate_value(self.json, &path, &bytes)
            }
        });

        if is_vec_type(field_type) {
            let append_name = Ident::new(&format!("append_{}", field_name), field_name.span());
            mutator_setters.push(quote! {
                pub fn #append_name(&mut self, val: &(impl jshift::ToJsonBytes + ?Sized)) -> Result<(), jshift::Error> {
                    let bytes = jshift::ToJsonBytes::to_json_bytes(val);
                    let path = jshift::parse_path(#raw_path);
                    jshift::append_to_array(self.json, &path, &bytes)
                }
            });
        }
    }

    Ok(quote! {
        impl #struct_name {
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
    if let Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            return segment.ident == "Vec";
        }
    }
    false
}

/// Mirror of jshift::try_parse_path strict rules (derive cannot depend on jshift).
fn validate_path_str(s: &str) -> Result<(), &'static str> {
    let mut rest = s;
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
                    if idx_str.parse::<usize>().is_err() {
                        return Err("array index out of range for usize");
                    }
                    rest = &rest[end_idx + 1..];
                }
                None => return Err("unclosed array index bracket '['"),
            }
        } else {
            let end_key = rest.find(['.', '[']).unwrap_or(rest.len());
            rest = &rest[end_key..];
        }
    }
    Ok(())
}
