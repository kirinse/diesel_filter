use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::{quote, ToTokens};
use std::default::Default;
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, Data, DeriveInput, Fields, Meta, NestedMeta, Path, Token, Type, TypePath,
};

struct Filter {
    pub name: Ident,
    pub ty: FilterableType,
    pub opts: FilterOpts,
}

enum FilterableType {
    String,
    Uuid,
    Foreign(String),
}

enum FilterKind {
    Basic,
    Substr,
    Insensitive,
    SubstrInsensitive,
}

struct FilterOpts {
    multiple: bool,
    kind: FilterKind,
}

impl Default for FilterOpts {
    fn default() -> Self {
        Self {
            multiple: false,
            kind: FilterKind::Basic,
        }
    }
}

impl From<Vec<NestedMeta>> for FilterOpts {
    fn from(m: Vec<NestedMeta>) -> Self {
        let meta = m
            .into_iter()
            .filter_map(|m| match m {
                NestedMeta::Meta(m) => Some(m.path().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();

        let matches =
            |m: &Vec<Path>, tested: &[&str]| tested.iter().all(|t| m.iter().any(|m| m.is_ident(t)));

        let kind = if matches(&meta, &["substring", "insensitive"]) {
            FilterKind::SubstrInsensitive
        } else if matches(&meta, &["substring"]) {
            FilterKind::Substr
        } else if matches(&meta, &["insensitive"]) {
            FilterKind::Insensitive
        } else {
            FilterKind::Basic
        };

        Self {
            multiple: matches(&meta, &["multiple"]),
            kind,
        }
    }
}

impl From<&TypePath> for FilterableType {
    fn from(ty: &TypePath) -> Self {
        match ty.to_token_stream().to_string().replace(' ', "").as_str() {
            "String" => Self::String,
            "Uuid" => Self::Uuid,
            "uuid::Uuid" => Self::Uuid,
            "Option<String>" => Self::String,
            "Option<Uuid>" => Self::Uuid,
            "Option<uuid::Uuid>" => Self::Uuid,
            other => Self::Foreign(other.to_string()),
        }
    }
}

impl From<FilterableType> for Ident {
    fn from(val: FilterableType) -> Self {
        match val {
            FilterableType::String => Ident::new("String", Span::call_site()),
            FilterableType::Uuid => Ident::new("Uuid", Span::call_site()),
            FilterableType::Foreign(ty) => Ident::new(&ty, Span::call_site()),
        }
    }
}

struct TableName {
    name: Ident,
}

impl Parse for TableName {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attr_name: Ident = input.parse()?;
        if attr_name != "table_name" {
            return Err(syn::Error::new(attr_name.span(), "Wrong attribute name"));
        }
        input.parse::<Token![=]>()?;
        let name: Ident = input.parse()?;
        Ok(TableName { name })
    }
}

#[proc_macro_derive(DieselFilter, attributes(filter, table_name, pagination))]
pub fn filter(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let table_name = match input
        .attrs
        .iter()
        .filter(|attr| attr.path.is_ident("diesel"))
        .filter_map(|a| a.parse_args::<TableName>().ok())
        .next()
    {
        Some(tn) => tn.name,
        None => panic!("please provide #[diesel(table_name = ...)] attribute"),
    };

    let pagination = input
        .attrs
        .iter()
        .filter(|m| m.path.is_ident("pagination"))
        .last()
        .is_some();

    let struct_name = input.ident;
    let mut filters = vec![];

    if let Data::Struct(data) = input.data {
        if let Fields::Named(fields) = data.fields {
            for field in fields.named {
                match field.ident {
                    Some(name) => {
                        let field_type = field.ty;
                        for attr in field.attrs.into_iter() {
                            if !attr.path.is_ident("filter") {
                                continue;
                            }
                            let opts = match attr.parse_meta().unwrap() {
                                Meta::List(te) => {
                                    FilterOpts::from(te.nested.into_iter().collect::<Vec<_>>())
                                }
                                Meta::Path(_) => FilterOpts::default(),
                                _ => continue,
                            };

                            if let Type::Path(ty) = &field_type {
                                let ty = FilterableType::from(ty);
                                let name = name.clone();

                                filters.push(Filter { name, ty, opts });
                                continue;
                            }
                            panic!("this type is not supported");
                        }
                    }
                    None => continue,
                }
            }
        }
    }

    let filter_struct_ident = Ident::new(&format!("{}Filters", struct_name), struct_name.span());

    if filters.is_empty() {
        panic!("please annotate at least one field to filter with #[filter] on your struct");
    }

    let mut fields = vec![];
    let mut queries = vec![];
    let mut uses = vec![];
    let mut has_multiple = false;
    for filter in filters {
        let field = filter.name;
        let ty: Ident = filter.ty.into();
        let opts = filter.opts;

        let q = if opts.multiple {
            has_multiple = true;
            #[cfg(feature = "rocket")]
            fields.push(quote! {
                #[field(default = Option::None)]
                pub #field: Option<Vec<#ty>>,
            });
            #[cfg(not(feature = "rocket"))]
            fields.push(quote! {
                pub #field: Option<Vec<#ty>>,
            });
            match opts.kind {
                FilterKind::Basic => {
                    quote! { #table_name::#field.eq(any(filter)) }
                }
                FilterKind::Substr => {
                    quote! {
                        #table_name::#field.like(any(
                            filter.iter().map(|f| format!("%{}%", f)).collect::<Vec<_>>()
                        ))
                    }
                }
                FilterKind::Insensitive => {
                    quote! { #table_name::#field.ilike(any(filter)) }
                }
                FilterKind::SubstrInsensitive => {
                    quote! {
                        #table_name::#field.ilike(any(
                            filter.iter().map(|f| format!("%{}%", f)).collect::<Vec<_>>()
                        ))
                    }
                }
            }
        } else {
            fields.push(quote! {
                pub #field: Option<#ty>,
            });
            match opts.kind {
                FilterKind::Basic => {
                    quote! { #table_name::#field.eq(filter) }
                }
                FilterKind::Substr => {
                    quote! { #table_name::#field.like(format!("%{}%", filter)) }
                }
                FilterKind::Insensitive => {
                    quote! { #table_name::#field.ilike(filter) }
                }
                FilterKind::SubstrInsensitive => {
                    quote! { #table_name::#field.ilike(format!("%{}%", filter)) }
                }
            }
        };

        queries.push(quote! {
            if let Some(ref filter) = filters.#field {
                query = query.filter(#q);
            }
        });
    }

    if has_multiple {
        uses.push(quote! { use diesel::dsl::any; })
    }
    if pagination {
        fields.push(quote! {
            pub page: Option<i64>,
            pub per_page: Option<i64>,
        });
    }

    #[cfg(feature = "rocket")]
    let filters_struct = quote! {
        #[derive(FromForm, Debug)]
        pub struct #filter_struct_ident {
            #( #fields )*
        }
    };

    #[cfg(any(feature = "actix", feature = "axum"))]
    let filters_struct = quote! {
        #[derive(serde::Deserialize, Debug)]
        pub struct #filter_struct_ident {
            #( #fields )*
        }
    };

    #[cfg(not(any(feature = "rocket", feature = "actix", feature = "axum")))]
    let filters_struct = quote! {
        #[derive(Debug)]
        pub struct #filter_struct_ident {
            #( #fields )*
        }
    };

    let expanded = match pagination {
        true => {
            quote! {
                #filters_struct

                impl #struct_name {
                    pub fn filtered(filters: &#filter_struct_ident, conn: &mut PgConnection) -> Result<(Vec<#struct_name>, i64), diesel::result::Error> {
                        Self::filter(filters)
                          .paginate(filters.page)
                          .per_page(filters.per_page)
                          .load_and_count::<#struct_name>(conn)
                    }

                    pub fn filter<'a>(filters: &'a #filter_struct_ident) -> crate::schema::#table_name::BoxedQuery<'a, diesel::pg::Pg> {
                        #( #uses )*
                        let mut query = crate::schema::#table_name::table.into_boxed();

                        #( #queries )*

                        query
                    }
                }
            }
        }
        false => {
            quote! {
                #filters_struct

                impl #struct_name {
                    pub fn filtered(filters: &#filter_struct_ident, conn: &mut PgConnection) -> Result<Vec<#struct_name>, diesel::result::Error> {
                        Self::filter(filters).load::<#struct_name>(conn)
                    }

                    pub fn filter<'a>(filters: &'a #filter_struct_ident) -> crate::schema::#table_name::BoxedQuery<'a, diesel::pg::Pg> {
                        #( #uses )*
                        let mut query = crate::schema::#table_name::table.into_boxed();

                        #( #queries )*

                        query
                    }
                }
            }
        }
    };
    TokenStream::from(expanded)
}
