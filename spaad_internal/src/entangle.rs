use crate::entangle::transform::transform_method;
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use std::sync::atomic::{AtomicUsize, Ordering};
use syn::parse::{Parse, ParseStream};
use syn::parse_macro_input;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::*;

mod transform;

static IMPL_BLOCK_NUM: AtomicUsize = AtomicUsize::new(0);

enum EntangledItem {
    Struct(ItemStruct),
    Impl(ItemImpl),
}

impl Parse for EntangledItem {
    fn parse(input: ParseStream) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let lookahead = input.lookahead1();
        let expanded = if lookahead.peek(Token![impl]) {
            let mut item: ItemImpl = input.parse()?;
            item.attrs = attrs;
            EntangledItem::Impl(item)
        } else {
            let mut item: ItemStruct = input.parse()?;
            item.attrs = attrs;
            EntangledItem::Struct(item)
        };
        Ok(expanded)
    }
}

pub fn entangle(input: TokenStream) -> proc_macro::TokenStream {
    let item = parse_macro_input!(input as EntangledItem);
    let expanded = match item {
        EntangledItem::Struct(s) => entangle_struct(s),
        EntangledItem::Impl(i) => entangle_impl(i),
    };

    TokenStream::from(expanded)
}

fn set_visibility_min_pub_super(vis: &mut Visibility) {
    let mut segments = Punctuated::new();
    segments.push(PathSegment::from(format_ident!("super")));

    if matches!(
        &vis,
        Visibility::Restricted(res) if res.path.segments.first().unwrap().ident != "self"
    ) {
        vis.span()
            .unwrap()
            .warning(
                "This visibility is not supported due to macro expansion and will be converted \
                to `pub(super)`"
            )
            .emit();
    }

    if matches!(vis,  Visibility::Inherited | Visibility::Restricted(_)) {
        *vis = Visibility::Restricted(VisRestricted {
            pub_token: syn::token::Pub {
                span: vis.span(),
            },
            paren_token: syn::token::Paren {
                span: vis.span(),
            },
            in_token: None,
            path: Box::new(Path {
                leading_colon: None,
                segments,
            }),
        })
    }
}

fn entangle_struct(struct_def: ItemStruct) -> proc_macro2::TokenStream {
    let ItemStruct {
        attrs,
        vis,
        ident,
        generics,
        mut fields,
        semi_token,
        ..
    } = struct_def;
    let actor_mod = format_ident!("__{}Actor", ident);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    for field in fields.iter_mut() {
        set_visibility_min_pub_super(&mut field.vis);
    }

    quote! {
        #[derive(Clone)]
        #vis struct #ident#impl_generics #where_clause {
            addr: ::spaad::export::xtra::Address<#actor_mod::#ident#ty_generics>,
        }

        impl#impl_generics #ident#ty_generics #where_clause {
            #vis fn into_address(
                self
            ) -> ::spaad::export::xtra::Address<#actor_mod::#ident#ty_generics> {
                self.addr
            }

            #vis fn address(
                &self
            ) -> &::spaad::export::xtra::Address<#actor_mod::#ident#ty_generics> {
                &self.addr
            }
        }

        #[doc(hidden)]
        #[allow(non_snake_case)]
        #vis mod #actor_mod {
            #(#attrs)*
            pub struct #ident#impl_generics #where_clause #fields #semi_token
        }
    }
}

fn entangle_impl(impl_block: ItemImpl) -> proc_macro2::TokenStream {
    if !matches!(*impl_block.self_ty, Type::Path(_)) {
        impl_block
            .span()
            .unwrap()
            .error("`spaad::entangle` can only be called on impls of an actor struct")
            .emit()
    }

    match &impl_block.trait_ {
        Some(_) => entangle_trait_impl(impl_block),
        None => entangle_handlers_impl(impl_block),
    }
}

fn get_name_from_path(p: &Path) -> &proc_macro2::Ident {
    &p.segments.last().unwrap().ident
}

fn get_name_from_ty(ty: &syn::Type) -> &proc_macro2::Ident {
    let ty_path = match &*ty {
        Type::Path(path) => &path.path,
        _ => {
            ty.span()
                .unwrap()
                .error(
                    "the return type of a `spaad::entangled` handler must be\
                    `Result<T, xtra::Disconnected>`",
                )
                .emit();
            unreachable!()
        }
    };
    get_name_from_path(ty_path)
}

fn get_name(block: &ItemImpl) -> &proc_macro2::Ident {
    let self_ty_path = match &*block.self_ty {
        Type::Path(path) => &path.path,
        _ => {
            block
                .self_ty
                .span()
                .unwrap()
                .error("the self type of a `spaad::entangled` impl must be a struct")
                .emit();
            unreachable!()
        }
    };
    get_name_from_path(self_ty_path)
}

fn get_actor_name(block: &ItemImpl) -> proc_macro2::TokenStream {
    let name = get_name(block);
    let mod_name = format_ident!("__{}Actor", name);
    quote!(#mod_name::#name)
}

fn entangle_handlers_impl(mut handlers_impl: ItemImpl) -> proc_macro2::TokenStream {
    let old_impl = handlers_impl.clone();
    let name = get_name(&handlers_impl).clone();
    let wrapper = match &*handlers_impl.self_ty {
        Type::Path(ref path) => {
            let mut path = path.path.clone();
            path.segments.last_mut().unwrap().ident = name.clone();
            path
        }
        _ => unreachable!(),
    };
    let actor_path = match &mut *handlers_impl.self_ty {
        Type::Path(ref mut path) => {
            transform_actor_path(&name, &mut path.path);

            let mut actor_path = path.path.clone();
            actor_path.segments.last_mut().unwrap().arguments = PathArguments::None;
            actor_path
        }
        _ => unreachable!(),
    };
    let actor = handlers_impl.self_ty.clone();

    let (impl_generics, _, where_clause) = handlers_impl.generics.split_for_impl();
    let mut actor_items = handlers_impl.items.clone();
    let transformed_items = transform_items(&old_impl, handlers_impl.items.iter());

    let impl_num = format_ident!("__impl{}", IMPL_BLOCK_NUM.fetch_add(1, Ordering::SeqCst));

    for item in actor_items.iter_mut() {
        let vis = match item {
            ImplItem::Const(const_) => &mut const_.vis,
            ImplItem::Method(meth) => &mut meth.vis,
            ImplItem::Type(typ) =>  &mut typ.vis,
            _ => continue,
        };
        set_visibility_min_pub_super(vis);
    }

    quote! {
        mod #impl_num {
            use super::*;
            use #actor_path;

            impl#impl_generics #actor #where_clause {
                #(#actor_items)*
            }

            impl#impl_generics super::#wrapper #where_clause {
                #(#transformed_items)*
            }
        }
    }
}

fn transform_items<'a, I: Iterator<Item = &'a ImplItem> + 'a>(
    impl_block: &'a ItemImpl,
    iter: I,
) -> impl Iterator<Item = proc_macro2::TokenStream> + 'a {
    iter.map(move |method| match method {
        ImplItem::Const(c) => quote!(#c),
        ImplItem::Type(t) => quote!(#t),
        ImplItem::Macro(m) => quote!(#m),
        ImplItem::Verbatim(v) => quote!(#v),
        ImplItem::Method(m) => transform_method(impl_block, m.clone()),
        _ => unimplemented!("Unknown impl item"),
    })
}

fn transform_actor_path(name: &Ident, path: &mut Path) {
    let mod_name = format_ident!("__{}Actor", name);
    let last = path.segments.pop().unwrap().into_tuple().0;
    path.segments.push(PathSegment::from(mod_name));
    path.segments.push(last)
}

fn entangle_trait_impl(mut trait_impl: ItemImpl) -> proc_macro2::TokenStream {
    let name = get_name(&trait_impl).clone();
    match &mut *trait_impl.self_ty {
        Type::Path(ref mut path) => transform_actor_path(&name, &mut path.path),
        _ => unreachable!(),
    }

    quote!(#trait_impl)
}