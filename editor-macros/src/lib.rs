use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};
use std::sync::Mutex;

static COMMANDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[proc_macro_attribute]
pub fn command(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let ts = item.clone();
    let func = parse_macro_input!(ts as ItemFn);
    let name = &func.sig.ident;

    if func.sig.ident == "custom_layer_init" {
        return syn::Error::new_spanned(
            &func.sig.ident,
            "custom_layer_init cannot be annotated with #[command], use #[export] directly"
        ).to_compile_error().into();
    }

    COMMANDS.lock().unwrap().push(name.to_string());

    export(_attr, item)
}

#[proc_macro]
pub fn collect_commands(_input: TokenStream) -> TokenStream {
    let commands = COMMANDS.lock().unwrap();
    let names: Vec<_> = commands.iter().map(|s| {
        syn::Ident::new(s, proc_macro2::Span::call_site())
    }).collect();
    let name_strs: Vec<_> = commands.iter().map(|s| s.as_str()).collect();

    quote! {
        &[
            #(CommandEntry {
                name: #name_strs,
                func: #names,
            }),*
        ]
    }.into()
}

#[proc_macro_attribute]
pub fn export(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(item as syn::Item);

    if let syn::Item::Fn(f) = &item {
        if let Some(err) = validate_custom_layer_init(f) {
            return err.to_compile_error().into();
        }
    }

    let attrs = match &mut item {
        syn::Item::Fn(f)     => &mut f.attrs,
        syn::Item::Const(c)  => &mut c.attrs,
        syn::Item::Static(s) => &mut s.attrs,
        _ => panic!("#[export] only supported on fn and const items"),
    };

    let has_no_mangle = attrs.iter().any(|a| a.path().is_ident("no_mangle"));
    if !has_no_mangle {
        attrs.push(syn::parse_quote!(#[unsafe(no_mangle)]));
    }

    match &mut item {
        syn::Item::Fn(f) => {
            if f.sig.abi.is_none() {
                f.sig.abi = Some(syn::parse_quote!(extern "C"));
            }
            if !matches!(f.vis, syn::Visibility::Public(_)) {
                f.vis = syn::parse_quote!(pub);
            }
        }

        syn::Item::Const(c) => {
            if !matches!(c.vis, syn::Visibility::Public(_)) {
                c.vis = syn::parse_quote!(pub);
            }
        }

        syn::Item::Static(s) => {
            if !matches!(s.vis, syn::Visibility::Public(_)) {
                s.vis = syn::parse_quote!(pub);
            }
        }

        _ => unreachable!()
    }

    quote! { #item }.into()
}

fn is_mut_ref_to(arg: &syn::FnArg, type_name: &str) -> bool {
    let syn::FnArg::Typed(pat_type) = arg else { return false };
    let syn::Type::Reference(r) = &*pat_type.ty else { return false };
    r.mutability.is_some() &&
    matches!(&*r.elem, syn::Type::Path(p) if p.path.is_ident(type_name))
}

fn is_ref_to(arg: &syn::FnArg, type_name: &str) -> bool {
    let syn::FnArg::Typed(pat_type) = arg else { return false };
    let syn::Type::Reference(r) = &*pat_type.ty else { return false };
    r.mutability.is_none() &&
    matches!(&*r.elem, syn::Type::Path(p) if p.path.is_ident(type_name))
}

fn validate_custom_layer_init(f: &syn::ItemFn) -> Option<syn::Error> {
    if f.sig.ident != "custom_layer_init" { return None; }
    let inputs: Vec<_> = f.sig.inputs.iter().collect();
    let valid = inputs.len() == 2
        && is_mut_ref_to(inputs[0], "CommandContext")
        && is_ref_to(inputs[1], "LoadedLib");
    if !valid {
        Some(syn::Error::new_spanned(
            &f.sig,
            "custom_layer_init must have signature: (cx: &mut CommandContext, loaded: &LoadedLib)"
        ))
    } else {
        None
    }
}
