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
