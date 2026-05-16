//! Procedural macros for the harness framework.
//!
//! Currently implements `#[skill]` for function-shaped skills (DESIGN.md §6.4).
//! Future additions: `#[skill]` for structs, `#[tool]`, `#[guide]`, `#[sensor]`,
//! `#[hook]`, plus the declarative `skills_dir!` macro.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Expr, ExprLit, ItemFn, Lit, Meta, Token, parse_macro_input, punctuated::Punctuated};

/// `#[skill]` — declare a function-shaped skill that is automatically registered
/// at process start via the `inventory` crate.
///
/// ## Example
///
/// ```ignore
/// use harness::prelude::*;
///
/// /// Run `cargo fmt` across the workspace. Use after Rust edits.
/// #[skill(
///     name = "format-rust",
///     license = "Apache-2.0",
///     allowed_tools = "Bash(cargo:fmt)",
///     harness(kind = "computational", risk = "read-only"),
/// )]
/// async fn format_rust(ctx: &mut Context, w: &mut World) -> Result<(), harness::SkillError> {
///     // ...
///     Ok(())
/// }
/// ```
///
/// `description` is optional: when omitted, the macro uses the function's
/// doc-comment (each `///` line, joined with spaces). This matches the
/// agentskills.io spec's "description (1–1024 chars, required)" rule.
#[proc_macro_attribute]
pub fn skill(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated<Meta, Token![,]>::parse_terminated);

    match expand_skill(args, item_fn) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_skill(
    args: Punctuated<Meta, Token![,]>,
    item_fn: ItemFn,
) -> syn::Result<TokenStream2> {
    let fn_ident = item_fn.sig.ident.clone();

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut license: Option<String> = None;
    let mut compatibility: Option<String> = None;
    let mut allowed_tools: Option<String> = None;
    let mut harness_kind: Option<String> = None;
    let mut harness_risk: Option<String> = None;

    for meta in &args {
        match meta {
            Meta::NameValue(nv) => {
                let key = nv
                    .path
                    .get_ident()
                    .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected ident"))?
                    .to_string();
                let value = lit_str(&nv.value)?;
                match key.as_str() {
                    "name"          => name = Some(value),
                    "description"   => description = Some(value),
                    "license"       => license = Some(value),
                    "compatibility" => compatibility = Some(value),
                    "allowed_tools" => allowed_tools = Some(value),
                    other => {
                        return Err(syn::Error::new_spanned(
                            nv,
                            format!("unknown attribute `{other}`"),
                        ));
                    }
                }
            }
            Meta::List(ml) if ml.path.is_ident("harness") => {
                let nested: Punctuated<Meta, Token![,]> =
                    ml.parse_args_with(Punctuated::parse_terminated)?;
                for m in &nested {
                    if let Meta::NameValue(nv) = m {
                        let key = nv
                            .path
                            .get_ident()
                            .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected ident"))?
                            .to_string();
                        let value = lit_str(&nv.value)?;
                        match key.as_str() {
                            "kind" => harness_kind = Some(value),
                            "risk" => harness_risk = Some(value),
                            other => {
                                return Err(syn::Error::new_spanned(
                                    nv,
                                    format!("unknown harness(...) key `{other}`"),
                                ));
                            }
                        }
                    } else {
                        return Err(syn::Error::new_spanned(m, "expected key = \"value\""));
                    }
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "expected `key = \"value\"` or `harness(...)`",
                ));
            }
        }
    }

    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(&fn_ident, "missing required `name = \"...\"` attribute")
    })?;
    validate_name(&name).map_err(|reason| syn::Error::new_spanned(&fn_ident, reason))?;

    let description = description.or_else(|| extract_doc_comments(&item_fn.attrs));
    let description = description.ok_or_else(|| {
        syn::Error::new_spanned(
            &fn_ident,
            "missing description: either set `description = \"...\"` or use a `///` doc-comment",
        )
    })?;
    if description.is_empty() {
        return Err(syn::Error::new_spanned(
            &fn_ident,
            "description must not be empty",
        ));
    }
    if description.len() > 1024 {
        return Err(syn::Error::new_spanned(
            &fn_ident,
            format!(
                "description exceeds 1024 chars (got {}); spec limit is 1024",
                description.len()
            ),
        ));
    }

    let marker_ident = format_ident!("__Harness_Skill_{}", to_pascal_case(&name));

    // Build metadata.harness JSON literal (or empty BTreeMap).
    let has_harness_meta = harness_kind.is_some() || harness_risk.is_some();
    let metadata_tok = if has_harness_meta {
        let mut json = String::from("{");
        let mut comma = false;
        if let Some(k) = &harness_kind {
            json.push_str(&format!("\"kind\":\"{k}\""));
            comma = true;
        }
        if let Some(r) = &harness_risk {
            if comma {
                json.push(',');
            }
            json.push_str(&format!("\"risk\":\"{r}\""));
        }
        json.push('}');
        quote! {{
            let mut m = ::std::collections::BTreeMap::new();
            let v: ::harness_core::__export::serde_json::Value =
                ::harness_core::__export::serde_json::from_str(#json)
                    .expect("statically-built JSON literal is well-formed");
            m.insert("harness".to_string(), v);
            m
        }}
    } else {
        quote! { ::std::collections::BTreeMap::new() }
    };

    let lic_tok = opt_string(license.as_deref());
    let compat_tok = opt_string(compatibility.as_deref());
    let allowed_tok = opt_string(allowed_tools.as_deref());

    Ok(quote! {
        #item_fn

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #marker_ident;

        impl ::harness_core::Skill for #marker_ident {
            fn manifest(&self) -> &::harness_core::SkillManifest {
                static M: ::std::sync::OnceLock<::harness_core::SkillManifest> =
                    ::std::sync::OnceLock::new();
                M.get_or_init(|| ::harness_core::SkillManifest {
                    name:          #name.to_string(),
                    description:   #description.to_string(),
                    license:       #lic_tok,
                    compatibility: #compat_tok,
                    metadata:      #metadata_tok,
                    allowed_tools: #allowed_tok,
                })
            }

            fn body(&self) -> ::std::borrow::Cow<'_, str> {
                ::std::borrow::Cow::Borrowed(#description)
            }

            fn handler(&self) -> ::std::option::Option<::harness_core::SkillHandler> {
                ::std::option::Option::Some(::std::sync::Arc::new(|ctx, world| {
                    ::std::boxed::Box::pin(#fn_ident(ctx, world))
                }))
            }
        }

        ::harness_core::__export::inventory::submit! {
            ::harness_core::SkillEntry {
                factory: || ::std::sync::Arc::new(#marker_ident)
                    as ::std::sync::Arc<dyn ::harness_core::Skill>,
            }
        }
    })
}

// ---------- helpers ----------

fn lit_str(expr: &Expr) -> syn::Result<String> {
    if let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = expr {
        Ok(s.value())
    } else {
        Err(syn::Error::new_spanned(expr, "expected string literal"))
    }
}

fn opt_string(v: Option<&str>) -> TokenStream2 {
    match v {
        Some(s) => quote! { ::std::option::Option::Some(#s.to_string()) },
        None => quote! { ::std::option::Option::None },
    }
}

/// Validate name against the agentskills.io spec rules.
/// Duplicated minimally here (a small regex) so the macro stays an isolated
/// proc-macro crate without depending on harness-core for runtime use.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 64 {
        return Err(format!(
            "name length {} exceeds spec limit of 64",
            name.len()
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with `-`".into());
    }
    if name.contains("--") {
        return Err("name must not contain consecutive `--`".into());
    }
    for (i, c) in name.char_indices() {
        let ok = c.is_ascii_digit() || ('a'..='z').contains(&c) || c == '-';
        if !ok {
            return Err(format!(
                "name contains invalid char `{c}` at byte {i}; spec allows [a-z0-9-] only"
            ));
        }
    }
    Ok(())
}

fn extract_doc_comments(attrs: &[syn::Attribute]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta
            && let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
        {
            lines.push(s.value().trim().to_string());
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" ").trim().to_string())
    }
}

fn to_pascal_case(s: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in s.chars() {
        if c == '-' || c == '_' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}
