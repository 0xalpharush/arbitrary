extern crate proc_macro;

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{ext::IdentExt as _, *};

mod container_attributes;
mod field_attributes;
mod variant_attributes;

use container_attributes::ContainerAttributes;
use field_attributes::{determine_field_constructor, FieldConstructor};
use variant_attributes::not_skipped;

const ARBITRARY_ATTRIBUTE_NAME: &str = "arbitrary";
const ARBITRARY_LIFETIME_NAME: &str = "'arbitrary";

#[proc_macro_derive(Arbitrary, attributes(arbitrary))]
pub fn derive_arbitrary(tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(tokens as syn::DeriveInput);
    expand_derive_arbitrary(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_derive_arbitrary(input: syn::DeriveInput) -> Result<TokenStream> {
    let container_attrs = ContainerAttributes::from_derive_input(&input)?;

    let (lifetime_without_bounds, lifetime_with_bounds) =
        build_arbitrary_lifetime(input.generics.clone());

    // This won't be used if `needs_recursive_count` ends up false.
    let recursive_count = syn::Ident::new(
        &format!("RECURSIVE_COUNT_{}", input.ident.unraw()),
        Span::call_site(),
    );

    let (arbitrary_method, needs_recursive_count) =
        gen_arbitrary_method(&input, lifetime_without_bounds.clone(), &recursive_count)?;
    let size_hint_method = gen_size_hint_method(&input, needs_recursive_count)?;
    let name = input.ident;

    // Apply user-supplied bounds or automatic `T: ArbitraryBounds`.
    let generics = apply_trait_bounds(
        input.generics,
        lifetime_without_bounds.clone(),
        &container_attrs,
    )?;

    // Build ImplGeneric with a lifetime (https://github.com/dtolnay/syn/issues/90)
    let mut generics_with_lifetime = generics.clone();
    generics_with_lifetime
        .params
        .push(GenericParam::Lifetime(lifetime_with_bounds));
    let (impl_generics, _, _) = generics_with_lifetime.split_for_impl();

    // Build TypeGenerics and WhereClause without a lifetime
    let (_, ty_generics, where_clause) = generics.split_for_impl();

    let recursive_count = needs_recursive_count.then(|| {
        Some(quote! {
            ::std::thread_local! {
                #[allow(non_upper_case_globals)]
                static #recursive_count: ::core::cell::Cell<u32> = const {
                    ::core::cell::Cell::new(0)
                };
            }
        })
    });

    Ok(quote! {
        const _: () = {
            #recursive_count

            #[automatically_derived]
            impl #impl_generics arbitrary::Arbitrary<#lifetime_without_bounds>
                for #name #ty_generics #where_clause
            {
                #arbitrary_method
                #size_hint_method
            }
        };
    })
}

// Returns: (lifetime without bounds, lifetime with bounds)
// Example: ("'arbitrary", "'arbitrary: 'a + 'b")
fn build_arbitrary_lifetime(generics: Generics) -> (LifetimeParam, LifetimeParam) {
    let lifetime_without_bounds =
        LifetimeParam::new(Lifetime::new(ARBITRARY_LIFETIME_NAME, Span::call_site()));
    let mut lifetime_with_bounds = lifetime_without_bounds.clone();

    for param in generics.params.iter() {
        if let GenericParam::Lifetime(lifetime_def) = param {
            lifetime_with_bounds
                .bounds
                .push(lifetime_def.lifetime.clone());
        }
    }

    (lifetime_without_bounds, lifetime_with_bounds)
}

fn apply_trait_bounds(
    mut generics: Generics,
    lifetime: LifetimeParam,
    container_attrs: &ContainerAttributes,
) -> Result<Generics> {
    // If user-supplied bounds exist, apply them to their matching type parameters.
    if let Some(config_bounds) = &container_attrs.bounds {
        let mut config_bounds_applied = 0;
        for param in generics.params.iter_mut() {
            if let GenericParam::Type(type_param) = param {
                if let Some(replacement) = config_bounds
                    .iter()
                    .flatten()
                    .find(|p| p.ident == type_param.ident)
                {
                    *type_param = replacement.clone();
                    config_bounds_applied += 1;
                } else {
                    // If no user-supplied bounds exist for this type, delete the original bounds.
                    // This mimics serde.
                    type_param.bounds = Default::default();
                    type_param.default = None;
                }
            }
        }
        let config_bounds_supplied = config_bounds
            .iter()
            .map(|bounds| bounds.len())
            .sum::<usize>();
        if config_bounds_applied != config_bounds_supplied {
            return Err(Error::new(
                Span::call_site(),
                format!(
                    "invalid `{}` attribute. too many bounds, only {} out of {} are applicable",
                    ARBITRARY_ATTRIBUTE_NAME, config_bounds_applied, config_bounds_supplied,
                ),
            ));
        }
        Ok(generics)
    } else {
        // Otherwise, inject a `T: Arbitrary` bound for every parameter.
        Ok(add_trait_bounds(generics, lifetime))
    }
}

// Add a bound `T: Arbitrary` to every type parameter T.
fn add_trait_bounds(mut generics: Generics, lifetime: LifetimeParam) -> Generics {
    for param in generics.params.iter_mut() {
        if let GenericParam::Type(type_param) = param {
            type_param
                .bounds
                .push(parse_quote!(arbitrary::Arbitrary<#lifetime>));
        }
    }
    generics
}

fn gen_arbitrary_method(
    input: &DeriveInput,
    lifetime: LifetimeParam,
    recursive_count: &syn::Ident,
) -> Result<(TokenStream, bool)> {
    fn arbitrary_structlike(
        fields: &Fields,
        ident: &syn::Ident,
        lifetime: LifetimeParam,
        recursive_count: &syn::Ident,
    ) -> Result<TokenStream> {
        let arbitrary = construct(fields, |_idx, field| gen_constructor_for_field(field))?;
        let body = quote! {
            arbitrary::details::with_recursive_count(u, &#recursive_count, |mut u| {
                Ok(#ident #arbitrary)
            })
        };

        let arbitrary_take_rest = construct_take_rest(fields)?;
        let take_rest_body = quote! {
            arbitrary::details::with_recursive_count(u, &#recursive_count, |mut u| {
                Ok(#ident #arbitrary_take_rest)
            })
        };

        Ok(quote! {
            fn arbitrary(u: &mut arbitrary::Unstructured<#lifetime>) -> arbitrary::Result<Self> {
                #body
            }

            fn arbitrary_take_rest(mut u: arbitrary::Unstructured<#lifetime>) -> arbitrary::Result<Self> {
                #take_rest_body
            }
        })
    }

    fn arbitrary_variant(
        index: u64,
        enum_name: &Ident,
        variant_name: &Ident,
        ctor: TokenStream,
    ) -> TokenStream {
        quote! { #index => #enum_name::#variant_name #ctor }
    }

    fn arbitrary_enum_method(
        recursive_count: &syn::Ident,
        unstructured: TokenStream,
        variants: &[TokenStream],
        needs_recursive_count: bool,
    ) -> TokenStream {
        let count = variants.len() as u64;

        let do_variants = quote! {
            // Use a multiply + shift to generate a ranged random number
            // with slight bias. For details, see:
            // https://lemire.me/blog/2016/06/30/fast-random-shuffling
            Ok(match (
                u64::from(<u32 as arbitrary::Arbitrary>::arbitrary(#unstructured)?) * #count
            ) >> 32
            {
                #(#variants,)*
                _ => unreachable!()
            })
        };

        if needs_recursive_count {
            quote! {
                arbitrary::details::with_recursive_count(u, &#recursive_count, |mut u| {
                    #do_variants
                })
            }
        } else {
            do_variants
        }
    }

    fn arbitrary_enum(
        DataEnum { variants, .. }: &DataEnum,
        enum_name: &Ident,
        lifetime: LifetimeParam,
        recursive_count: &syn::Ident,
    ) -> Result<(TokenStream, bool)> {
        let filtered_variants = variants.iter().filter(not_skipped);

        // Check attributes of all variants:
        filtered_variants
            .clone()
            .try_for_each(check_variant_attrs)?;

        // From here on, we can assume that the attributes of all variants were checked.
        let enumerated_variants = filtered_variants
            .enumerate()
            .map(|(index, variant)| (index as u64, variant));

        // Construct `match`-arms for the `arbitrary` method.
        let mut needs_recursive_count = false;
        let variants = enumerated_variants
            .clone()
            .map(|(index, Variant { fields, ident, .. })| {
                construct(fields, |_, field| gen_constructor_for_field(field)).map(|ctor| {
                    if !ctor.is_empty() {
                        needs_recursive_count = true;
                    }
                    arbitrary_variant(index, enum_name, ident, ctor)
                })
            })
            .collect::<Result<Vec<TokenStream>>>()?;

        // Construct `match`-arms for the `arbitrary_take_rest` method.
        let variants_take_rest = enumerated_variants
            .map(|(index, Variant { fields, ident, .. })| {
                construct_take_rest(fields)
                    .map(|ctor| arbitrary_variant(index, enum_name, ident, ctor))
            })
            .collect::<Result<Vec<TokenStream>>>()?;

        // Most of the time, `variants` is not empty (the happy path),
        //   thus `variants_take_rest` will be used,
        //   so no need to move this check before constructing `variants_take_rest`.
        // If `variants` is empty, this will emit a compiler-error.
        (!variants.is_empty())
            .then(|| {
                // TODO: Improve dealing with `u` vs. `&mut u`.
                let arbitrary = arbitrary_enum_method(
                    recursive_count,
                    quote! { u },
                    &variants,
                    needs_recursive_count,
                );
                let arbitrary_take_rest = arbitrary_enum_method(
                    recursive_count,
                    quote! { &mut u },
                    &variants_take_rest,
                    needs_recursive_count,
                );

                (
                    quote! {
                        fn arbitrary(u: &mut arbitrary::Unstructured<#lifetime>)
                            -> arbitrary::Result<Self>
                        {
                            #arbitrary
                        }

                        fn arbitrary_take_rest(mut u: arbitrary::Unstructured<#lifetime>)
                            -> arbitrary::Result<Self>
                        {
                            #arbitrary_take_rest
                        }
                    },
                    needs_recursive_count,
                )
            })
            .ok_or_else(|| {
                Error::new_spanned(
                    enum_name,
                    "Enum must have at least one variant, that is not skipped",
                )
            })
    }

    let ident = &input.ident;
    let needs_recursive_count = true;
    match &input.data {
        Data::Struct(data) => arbitrary_structlike(&data.fields, ident, lifetime, recursive_count)
            .map(|ts| (ts, needs_recursive_count)),
        Data::Union(data) => arbitrary_structlike(
            &Fields::Named(data.fields.clone()),
            ident,
            lifetime,
            recursive_count,
        )
        .map(|ts| (ts, needs_recursive_count)),
        Data::Enum(data) => arbitrary_enum(data, ident, lifetime, recursive_count),
    }
}

fn construct(
    fields: &Fields,
    ctor: impl Fn(usize, &Field) -> Result<TokenStream>,
) -> Result<TokenStream> {
    let output = match fields {
        Fields::Named(names) => {
            let names: Vec<TokenStream> = names
                .named
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let name = f.ident.as_ref().unwrap();
                    ctor(i, f).map(|ctor| quote! { #name: #ctor })
                })
                .collect::<Result<_>>()?;
            quote! { { #(#names,)* } }
        }
        Fields::Unnamed(names) => {
            let names: Vec<TokenStream> = names
                .unnamed
                .iter()
                .enumerate()
                .map(|(i, f)| ctor(i, f).map(|ctor| quote! { #ctor }))
                .collect::<Result<_>>()?;
            quote! { ( #(#names),* ) }
        }
        Fields::Unit => quote!(),
    };
    Ok(output)
}

fn construct_take_rest(fields: &Fields) -> Result<TokenStream> {
    construct(fields, |idx, field| {
        determine_field_constructor(field).map(|field_constructor| match field_constructor {
            FieldConstructor::Default => quote!(::core::default::Default::default()),
            FieldConstructor::Arbitrary => {
                if idx + 1 == fields.len() {
                    quote! { arbitrary::Arbitrary::arbitrary_take_rest(u)? }
                } else {
                    quote! { arbitrary::Arbitrary::arbitrary(&mut u)? }
                }
            }
            FieldConstructor::With(function_or_closure) => quote!((#function_or_closure)(&mut u)?),
            FieldConstructor::Value(value) => quote!(#value),
        })
    })
}

fn gen_size_hint_method(input: &DeriveInput, needs_recursive_count: bool) -> Result<TokenStream> {
    let size_hint_fields = |fields: &Fields| {
        fields
            .iter()
            .map(|f| {
                let ty = &f.ty;
                determine_field_constructor(f).map(|field_constructor| {
                    match field_constructor {
                        FieldConstructor::Default | FieldConstructor::Value(_) => {
                            quote!(Ok((0, Some(0))))
                        }
                        FieldConstructor::Arbitrary => {
                            quote! { <#ty as arbitrary::Arbitrary>::try_size_hint(depth) }
                        }

                        // Note that in this case it's hard to determine what size_hint must be, so
                        // size_of::<T>() is just an educated guess, although it's gonna be
                        // inaccurate for dynamically allocated types (Vec, HashMap, etc.).
                        FieldConstructor::With(_) => {
                            quote! { Ok((::core::mem::size_of::<#ty>(), None)) }
                        }
                    }
                })
            })
            .collect::<Result<Vec<TokenStream>>>()
            .map(|hints| {
                quote! {
                    Ok(arbitrary::size_hint::and_all(&[
                        #( #hints? ),*
                    ]))
                }
            })
    };
    let size_hint_structlike = |fields: &Fields| {
        assert!(needs_recursive_count);
        size_hint_fields(fields).map(|hint| {
            quote! {
                #[inline]
                fn size_hint(depth: usize) -> (usize, ::core::option::Option<usize>) {
                    Self::try_size_hint(depth).unwrap_or_default()
                }

                #[inline]
                fn try_size_hint(depth: usize)
                    -> ::core::result::Result<
                        (usize, ::core::option::Option<usize>),
                        arbitrary::MaxRecursionReached,
                    >
                {
                    arbitrary::size_hint::try_recursion_guard(depth, |depth| #hint)
                }
            }
        })
    };
    match &input.data {
        Data::Struct(data) => size_hint_structlike(&data.fields),
        Data::Union(data) => size_hint_structlike(&Fields::Named(data.fields.clone())),
        Data::Enum(data) => data
            .variants
            .iter()
            .filter(not_skipped)
            .map(|Variant { fields, .. }| {
                if !needs_recursive_count {
                    assert!(fields.is_empty());
                }
                // The attributes of all variants are checked in `gen_arbitrary_method` above
                // and can therefore assume that they are valid.
                size_hint_fields(fields)
            })
            .collect::<Result<Vec<TokenStream>>>()
            .map(|variants| {
                if needs_recursive_count {
                    // The enum might be recursive: `try_size_hint` is the primary one, and
                    // `size_hint` is defined in terms of it.
                    quote! {
                        fn size_hint(depth: usize) -> (usize, ::core::option::Option<usize>) {
                            Self::try_size_hint(depth).unwrap_or_default()
                        }
                        #[inline]
                        fn try_size_hint(depth: usize)
                            -> ::core::result::Result<
                                (usize, ::core::option::Option<usize>),
                                arbitrary::MaxRecursionReached,
                            >
                        {
                            Ok(arbitrary::size_hint::and(
                                <u32 as arbitrary::Arbitrary>::size_hint(depth),
                                arbitrary::size_hint::try_recursion_guard(depth, |depth| {
                                    Ok(arbitrary::size_hint::or_all(&[ #( #variants? ),* ]))
                                })?,
                            ))
                        }
                    }
                } else {
                    // The enum is guaranteed non-recursive, i.e. fieldless: `size_hint` is the
                    // primary one, and the default `try_size_hint` is good enough.
                    quote! {
                        fn size_hint(depth: usize) -> (usize, ::core::option::Option<usize>) {
                            <u32 as arbitrary::Arbitrary>::size_hint(depth)
                        }
                    }
                }
            }),
    }
}

fn gen_constructor_for_field(field: &Field) -> Result<TokenStream> {
    let ctor = match determine_field_constructor(field)? {
        FieldConstructor::Default => quote!(::core::default::Default::default()),
        FieldConstructor::Arbitrary => quote!(arbitrary::Arbitrary::arbitrary(u)?),
        FieldConstructor::With(function_or_closure) => quote!((#function_or_closure)(u)?),
        FieldConstructor::Value(value) => quote!(#value),
    };
    Ok(ctor)
}

// =============================================================================
// #[derive(Dearbitrary)]
// =============================================================================

#[proc_macro_derive(Dearbitrary, attributes(arbitrary))]
pub fn derive_dearbitrary(tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(tokens as syn::DeriveInput);
    expand_derive_dearbitrary(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_derive_dearbitrary(input: syn::DeriveInput) -> Result<TokenStream> {
    let container_attrs = ContainerAttributes::from_derive_input(&input)?;

    let write_to_method = gen_dearbitrary_method(&input)?;
    let name = &input.ident;

    // Apply bounds: each type param needs `Dearbitrary` instead of `Arbitrary`.
    let generics = apply_dearbitrary_bounds(input.generics.clone(), &container_attrs)?;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics arbitrary::Dearbitrary for #name #ty_generics #where_clause {
            #write_to_method
        }
    })
}

fn apply_dearbitrary_bounds(
    mut generics: Generics,
    container_attrs: &ContainerAttributes,
) -> Result<Generics> {
    if container_attrs.bounds.is_some() {
        // User-supplied bounds: don't add automatic Dearbitrary bounds
        // (they should add them manually via the attribute)
        return Ok(generics);
    }
    for param in generics.params.iter_mut() {
        if let GenericParam::Type(type_param) = param {
            type_param.bounds.push(parse_quote!(arbitrary::Dearbitrary));
        }
    }
    Ok(generics)
}

fn gen_dearbitrary_method(input: &DeriveInput) -> Result<TokenStream> {
    match &input.data {
        Data::Struct(data) => gen_dearbitrary_struct(&data.fields),
        Data::Enum(data) => gen_dearbitrary_enum(data, &input.ident),
        Data::Union(data) => gen_dearbitrary_struct(&Fields::Named(data.fields.clone())),
    }
}

fn gen_dearbitrary_struct(fields: &Fields) -> Result<TokenStream> {
    let field_writes = gen_dearbitrary_field_writes(fields, &quote!(self))?;

    Ok(quote! {
        fn write_to(&self, s: &mut arbitrary::Structured) {
            #field_writes
        }
    })
}

fn gen_dearbitrary_field_writes(fields: &Fields, self_prefix: &TokenStream) -> Result<TokenStream> {
    let writes: Vec<TokenStream> = match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let name = f.ident.as_ref().unwrap();
                let ctor = determine_field_constructor(f)?;
                Ok(match ctor {
                    FieldConstructor::Default | FieldConstructor::Value(_) => {
                        // Default/fixed fields don't consume bytes during reading,
                        // so don't write any during dearbitrary.
                        quote!()
                    }
                    FieldConstructor::Arbitrary => {
                        quote! {
                            arbitrary::Dearbitrary::write_to(&#self_prefix.#name, s);
                        }
                    }
                    FieldConstructor::With(_) => {
                        // Custom constructors: best-effort, write as Dearbitrary
                        quote! {
                            arbitrary::Dearbitrary::write_to(&#self_prefix.#name, s);
                        }
                    }
                })
            })
            .collect::<Result<_>>()?,
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let idx = syn::Index::from(i);
                let ctor = determine_field_constructor(f)?;
                Ok(match ctor {
                    FieldConstructor::Default | FieldConstructor::Value(_) => quote!(),
                    FieldConstructor::Arbitrary | FieldConstructor::With(_) => {
                        quote! {
                            arbitrary::Dearbitrary::write_to(&#self_prefix.#idx, s);
                        }
                    }
                })
            })
            .collect::<Result<_>>()?,
        Fields::Unit => vec![],
    };

    Ok(quote! { #(#writes)* })
}

fn gen_dearbitrary_enum(data: &DataEnum, enum_name: &Ident) -> Result<TokenStream> {
    let variants: Vec<_> = data.variants.iter().filter(not_skipped).collect();
    let count = variants.len() as u64;

    if variants.is_empty() {
        return Err(Error::new_spanned(
            enum_name,
            "Enum must have at least one variant for Dearbitrary",
        ));
    }

    let arms: Vec<TokenStream> = variants
        .iter()
        .enumerate()
        .map(|(index, variant)| {
            let variant_name = &variant.ident;
            let index = index as u64;

            // Compute u32 value that reverses the Lemire multiply-shift:
            //   (u64::from(v) * count) >> 32 == index
            // v must be in [index * 2^32 / count, (index+1) * 2^32 / count).
            // Use the midpoint: v = (2*index + 1) * 2^32 / (2*count).
            let variant_u32 = if count == 1 {
                quote!(0u32) // any value works for single-variant enums
            } else {
                quote! {
                    (((2 * #index + 1) as u64 * (1u64 << 32)) / (2 * #count)) as u32
                }
            };

            match &variant.fields {
                Fields::Named(named) => {
                    let field_names: Vec<_> = named
                        .named
                        .iter()
                        .map(|f| f.ident.as_ref().unwrap())
                        .collect();
                    let field_writes: Vec<TokenStream> = named
                        .named
                        .iter()
                        .map(|f| {
                            let name = f.ident.as_ref().unwrap();
                            let ctor = determine_field_constructor(f).unwrap();
                            match ctor {
                                FieldConstructor::Default | FieldConstructor::Value(_) => quote!(),
                                FieldConstructor::Arbitrary | FieldConstructor::With(_) => {
                                    quote! {
                                        arbitrary::Dearbitrary::write_to(#name, s);
                                    }
                                }
                            }
                        })
                        .collect();

                    quote! {
                        #enum_name::#variant_name { #(ref #field_names),* } => {
                            let __variant_selector: u32 = #variant_u32;
                            arbitrary::Dearbitrary::write_to(&__variant_selector, s);
                            #(#field_writes)*
                        }
                    }
                }
                Fields::Unnamed(unnamed) => {
                    let field_names: Vec<Ident> = (0..unnamed.unnamed.len())
                        .map(|i| Ident::new(&format!("__field{}", i), Span::call_site()))
                        .collect();
                    let field_writes: Vec<TokenStream> = unnamed
                        .unnamed
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let name = &field_names[i];
                            let ctor = determine_field_constructor(f).unwrap();
                            match ctor {
                                FieldConstructor::Default | FieldConstructor::Value(_) => quote!(),
                                FieldConstructor::Arbitrary | FieldConstructor::With(_) => {
                                    quote! {
                                        arbitrary::Dearbitrary::write_to(#name, s);
                                    }
                                }
                            }
                        })
                        .collect();

                    quote! {
                        #enum_name::#variant_name(#(ref #field_names),*) => {
                            let __variant_selector: u32 = #variant_u32;
                            arbitrary::Dearbitrary::write_to(&__variant_selector, s);
                            #(#field_writes)*
                        }
                    }
                }
                Fields::Unit => {
                    quote! {
                        #enum_name::#variant_name => {
                            let __variant_selector: u32 = #variant_u32;
                            arbitrary::Dearbitrary::write_to(&__variant_selector, s);
                        }
                    }
                }
            }
        })
        .collect();

    Ok(quote! {
        fn write_to(&self, s: &mut arbitrary::Structured) {
            match self {
                #(#arms)*
            }
        }
    })
}

fn check_variant_attrs(variant: &Variant) -> Result<()> {
    for attr in &variant.attrs {
        if attr.path().is_ident(ARBITRARY_ATTRIBUTE_NAME) {
            return Err(Error::new_spanned(
                attr,
                format!(
                    "invalid `{}` attribute. it is unsupported on enum variants. try applying it to a field of the variant instead",
                    ARBITRARY_ATTRIBUTE_NAME
                ),
            ));
        }
    }
    Ok(())
}
