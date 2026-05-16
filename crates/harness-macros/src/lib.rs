//! Procedural macros for the harness framework.
//!
//! - `#[skill]` — function-shaped skill (agentskills.io-compliant)
//! - `#[tool]`  — function-shaped tool, name + risk + schema
//! - `#[guide]` — function-shaped guide, scope-based feedforward
//! - `#[sensor]`— function-shaped sensor, stage-based feedback
//! - `#[hook]`  — synchronous hook on a lifecycle event

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Expr, ExprLit, ItemFn, Lit, Meta, Token, parse_macro_input, punctuated::Punctuated};

// ============================================================
// #[skill]
// ============================================================

#[proc_macro_attribute]
pub fn skill(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated<Meta, Token![,]>::parse_terminated);
    match expand_skill(args, item_fn) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_skill(args: Punctuated<Meta, Token![,]>, item_fn: ItemFn) -> syn::Result<TokenStream2> {
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
                    "name" => name = Some(value),
                    "description" => description = Some(value),
                    "license" => license = Some(value),
                    "compatibility" => compatibility = Some(value),
                    "allowed_tools" => allowed_tools = Some(value),
                    other => return err(nv, format!("unknown attribute `{other}`")),
                }
            }
            Meta::List(ml) if ml.path.is_ident("harness") => {
                let nested: Punctuated<Meta, Token![,]> =
                    ml.parse_args_with(Punctuated::parse_terminated)?;
                for m in &nested {
                    if let Meta::NameValue(nv) = m {
                        let k = nv
                            .path
                            .get_ident()
                            .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected ident"))?
                            .to_string();
                        let v = lit_str(&nv.value)?;
                        match k.as_str() {
                            "kind" => harness_kind = Some(v),
                            "risk" => harness_risk = Some(v),
                            other => return err(nv, format!("unknown harness(...) key `{other}`")),
                        }
                    } else {
                        return err(m, "expected key = \"value\"");
                    }
                }
            }
            other => return err(other, "expected `key = \"value\"` or `harness(...)`"),
        }
    }

    let name = name.ok_or_else(|| syn::Error::new_spanned(&fn_ident, "missing required `name`"))?;
    validate_skill_name(&name).map_err(|r| syn::Error::new_spanned(&fn_ident, r))?;
    let description = description
        .or_else(|| extract_doc_comments(&item_fn.attrs))
        .ok_or_else(|| {
            syn::Error::new_spanned(&fn_ident, "missing `description` (or `///` doc-comment)")
        })?;
    if description.is_empty() {
        return err(&fn_ident, "description must not be empty");
    }
    if description.len() > 1024 {
        return err(
            &fn_ident,
            format!("description exceeds 1024 chars (got {})", description.len()),
        );
    }

    let marker = format_ident!("__Harness_Skill_{}", to_pascal_case(&name));
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
                ::harness_core::__export::serde_json::from_str(#json).unwrap();
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
        pub struct #marker;

        impl ::harness_core::Skill for #marker {
            fn manifest(&self) -> &::harness_core::SkillManifest {
                static M: ::std::sync::OnceLock<::harness_core::SkillManifest> = ::std::sync::OnceLock::new();
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
                factory: || ::std::sync::Arc::new(#marker)
                    as ::std::sync::Arc<dyn ::harness_core::Skill>,
            }
        }
    })
}

// ============================================================
// #[tool]
// ============================================================

#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated<Meta, Token![,]>::parse_terminated);
    match expand_tool(args, item_fn) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_tool(args: Punctuated<Meta, Token![,]>, item_fn: ItemFn) -> syn::Result<TokenStream2> {
    let fn_ident = item_fn.sig.ident.clone();
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut risk: String = "read-only".into();
    let mut schema: Option<String> = None;

    for meta in &args {
        if let Meta::NameValue(nv) = meta {
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            let value = lit_str(&nv.value)?;
            match key.as_str() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                "risk" => risk = value,
                "schema" => schema = Some(value),
                other => return err(nv, format!("unknown attribute `{other}`")),
            }
        } else {
            return err(meta, "expected `key = \"value\"`");
        }
    }

    let name = name.ok_or_else(|| syn::Error::new_spanned(&fn_ident, "missing required `name`"))?;
    let description = description
        .or_else(|| extract_doc_comments(&item_fn.attrs))
        .ok_or_else(|| {
            syn::Error::new_spanned(&fn_ident, "missing `description` (or `///` doc-comment)")
        })?;
    let schema = schema.unwrap_or_else(|| r#"{"type":"object"}"#.to_string());
    // Validate schema parses.
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&schema) {
        return err(&fn_ident, format!("schema is not valid JSON: {e}"));
    }
    let risk_variant = match risk.as_str() {
        "read-only" => quote!(::harness_core::ToolRisk::ReadOnly),
        "idempotent" => quote!(::harness_core::ToolRisk::Idempotent),
        "destructive" => quote!(::harness_core::ToolRisk::Destructive),
        "network" => quote!(::harness_core::ToolRisk::Network),
        other => {
            return err(
                &fn_ident,
                format!(
                    "risk must be one of read-only|idempotent|destructive|network, got `{other}`"
                ),
            );
        }
    };
    let marker = format_ident!("__Harness_Tool_{}", to_pascal_case(&name));

    Ok(quote! {
        #item_fn

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #marker;

        #[::harness_core::__export::async_trait]
        impl ::harness_core::Tool for #marker {
            fn name(&self) -> &str { #name }
            fn schema(&self) -> &::harness_core::ToolSchema {
                static S: ::std::sync::OnceLock<::harness_core::ToolSchema> = ::std::sync::OnceLock::new();
                S.get_or_init(|| ::harness_core::ToolSchema {
                    name:        #name.to_string(),
                    description: #description.to_string(),
                    input:       ::harness_core::__export::serde_json::from_str(#schema).unwrap(),
                })
            }
            fn risk(&self) -> ::harness_core::ToolRisk { #risk_variant }
            async fn invoke(
                &self,
                args: ::harness_core::__export::serde_json::Value,
                world: &mut ::harness_core::World,
            ) -> ::std::result::Result<::harness_core::ToolResult, ::harness_core::ToolError> {
                #fn_ident(args, world).await
            }
        }

        ::harness_core::__export::inventory::submit! {
            ::harness_core::ToolEntry {
                factory: || ::std::sync::Arc::new(#marker)
                    as ::std::sync::Arc<dyn ::harness_core::Tool>,
            }
        }
    })
}

// ============================================================
// #[guide]
// ============================================================

#[proc_macro_attribute]
pub fn guide(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated<Meta, Token![,]>::parse_terminated);
    match expand_guide(args, item_fn) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_guide(args: Punctuated<Meta, Token![,]>, item_fn: ItemFn) -> syn::Result<TokenStream2> {
    let fn_ident = item_fn.sig.ident.clone();
    let mut id: Option<String> = None;
    let mut scope: String = "always".into();
    let mut kind: String = "inferential".into();
    let mut task_matches: Vec<String> = Vec::new();

    for meta in &args {
        if let Meta::NameValue(nv) = meta {
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            let value = lit_str(&nv.value)?;
            match key.as_str() {
                "id" => id = Some(value),
                "scope" => scope = value,
                "kind" => kind = value,
                "task_matches" => {
                    task_matches = value.split(',').map(|s| s.trim().to_string()).collect()
                }
                other => return err(nv, format!("unknown attribute `{other}`")),
            }
        } else {
            return err(meta, "expected `key = \"value\"`");
        }
    }
    let id = id.unwrap_or_else(|| fn_ident.to_string());
    let kind_variant = match kind.as_str() {
        "computational" => quote!(::harness_core::Execution::Computational),
        "inferential" => quote!(::harness_core::Execution::Inferential),
        other => {
            return err(
                &fn_ident,
                format!("kind must be computational|inferential, got `{other}`"),
            );
        }
    };
    let scope_expr = match scope.as_str() {
        "always" => quote!(::harness_core::GuideScope::Always),
        "task-matches" if !task_matches.is_empty() => {
            let items = task_matches.iter().map(|s| quote!(#s.to_string()));
            quote!(::harness_core::GuideScope::TaskMatches(vec![#(#items),*]))
        }
        other => {
            return err(
                &fn_ident,
                format!(
                    "unsupported scope `{other}`; use \"always\" or \"task-matches\" + task_matches=..."
                ),
            );
        }
    };
    let marker = format_ident!("__Harness_Guide_{}", to_pascal_case(&id));

    Ok(quote! {
        #item_fn

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #marker;

        #[::harness_core::__export::async_trait]
        impl ::harness_core::Guide for #marker {
            fn id(&self) -> &::harness_core::GuideId {
                static I: ::std::sync::OnceLock<::harness_core::GuideId> = ::std::sync::OnceLock::new();
                I.get_or_init(|| #id.to_string())
            }
            fn kind(&self) -> ::harness_core::Execution { #kind_variant }
            fn scope(&self) -> &::harness_core::GuideScope {
                static S: ::std::sync::OnceLock<::harness_core::GuideScope> = ::std::sync::OnceLock::new();
                S.get_or_init(|| #scope_expr)
            }
            async fn apply(
                &self,
                ctx: &mut ::harness_core::Context,
                world: &::harness_core::World,
            ) -> ::std::result::Result<(), ::harness_core::GuideError> {
                #fn_ident(ctx, world).await
            }
        }

        ::harness_core::__export::inventory::submit! {
            ::harness_core::GuideEntry {
                factory: || ::std::sync::Arc::new(#marker)
                    as ::std::sync::Arc<dyn ::harness_core::Guide>,
            }
        }
    })
}

// ============================================================
// #[sensor]
// ============================================================

#[proc_macro_attribute]
pub fn sensor(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated<Meta, Token![,]>::parse_terminated);
    match expand_sensor(args, item_fn) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_sensor(args: Punctuated<Meta, Token![,]>, item_fn: ItemFn) -> syn::Result<TokenStream2> {
    let fn_ident = item_fn.sig.ident.clone();
    let mut id: Option<String> = None;
    let mut stage: String = "self-correct".into();
    let mut kind: String = "computational".into();

    for meta in &args {
        if let Meta::NameValue(nv) = meta {
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            let value = lit_str(&nv.value)?;
            match key.as_str() {
                "id" => id = Some(value),
                "stage" => stage = value,
                "kind" => kind = value,
                other => return err(nv, format!("unknown attribute `{other}`")),
            }
        } else {
            return err(meta, "expected `key = \"value\"`");
        }
    }
    let id = id.unwrap_or_else(|| fn_ident.to_string());
    let kind_variant = match kind.as_str() {
        "computational" => quote!(::harness_core::Execution::Computational),
        "inferential" => quote!(::harness_core::Execution::Inferential),
        other => {
            return err(
                &fn_ident,
                format!("kind must be computational|inferential, got `{other}`"),
            );
        }
    };
    let stage_variant = match stage.as_str() {
        "pre-action" => quote!(::harness_core::Stage::PreAction),
        "self-correct" => quote!(::harness_core::Stage::SelfCorrect),
        "pre-commit" => quote!(::harness_core::Stage::PreCommit),
        "post-integrate" => quote!(::harness_core::Stage::PostIntegrate),
        "continuous" => quote!(::harness_core::Stage::Continuous),
        other => return err(&fn_ident, format!("unknown stage `{other}`")),
    };
    let marker = format_ident!("__Harness_Sensor_{}", to_pascal_case(&id));

    Ok(quote! {
        #item_fn

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #marker;

        #[::harness_core::__export::async_trait]
        impl ::harness_core::Sensor for #marker {
            fn id(&self) -> &::harness_core::SensorId {
                static I: ::std::sync::OnceLock<::harness_core::SensorId> = ::std::sync::OnceLock::new();
                I.get_or_init(|| #id.to_string())
            }
            fn kind(&self) -> ::harness_core::Execution { #kind_variant }
            fn stage(&self) -> ::harness_core::Stage { #stage_variant }
            async fn observe(
                &self,
                action: &::harness_core::Action,
                world: &::harness_core::World,
            ) -> ::std::result::Result<::std::vec::Vec<::harness_core::Signal>, ::harness_core::SensorError> {
                #fn_ident(action, world).await
            }
        }

        ::harness_core::__export::inventory::submit! {
            ::harness_core::SensorEntry {
                factory: || ::std::sync::Arc::new(#marker)
                    as ::std::sync::Arc<dyn ::harness_core::Sensor>,
            }
        }
    })
}

// ============================================================
// #[hook]
// ============================================================

#[proc_macro_attribute]
pub fn hook(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr with Punctuated<Meta, Token![,]>::parse_terminated);
    match expand_hook(args, item_fn) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_hook(args: Punctuated<Meta, Token![,]>, item_fn: ItemFn) -> syn::Result<TokenStream2> {
    let fn_ident = item_fn.sig.ident.clone();
    let mut name: Option<String> = None;
    let mut event: Option<String> = None;

    for meta in &args {
        if let Meta::NameValue(nv) = meta {
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            let value = lit_str(&nv.value)?;
            match key.as_str() {
                "name" => name = Some(value),
                "event" => event = Some(value),
                other => return err(nv, format!("unknown attribute `{other}`")),
            }
        } else {
            return err(meta, "expected `key = \"value\"`");
        }
    }
    let event =
        event.ok_or_else(|| syn::Error::new_spanned(&fn_ident, "missing required `event`"))?;
    let name = name.unwrap_or_else(|| fn_ident.to_string());
    let marker = format_ident!("__Harness_Hook_{}", to_pascal_case(&name));

    Ok(quote! {
        #item_fn

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #marker;

        impl ::harness_core::Hook for #marker {
            fn name(&self) -> &str { #name }
            fn matches(&self, ev: &::harness_core::Event<'_>) -> bool {
                ev.name() == #event
            }
            fn fire(
                &self,
                ev: &::harness_core::Event<'_>,
                world: &mut ::harness_core::World,
            ) -> ::harness_core::HookOutcome {
                #fn_ident(ev, world)
            }
        }

        ::harness_core::__export::inventory::submit! {
            ::harness_core::HookEntry {
                factory: || ::std::sync::Arc::new(#marker)
                    as ::std::sync::Arc<dyn ::harness_core::Hook>,
            }
        }
    })
}

// ============================================================
// Shared helpers
// ============================================================

fn lit_str(expr: &Expr) -> syn::Result<String> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(s), ..
    }) = expr
    {
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

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 64 {
        return Err(format!("name length {} > 64", name.len()));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with `-`".into());
    }
    if name.contains("--") {
        return Err("name must not contain `--`".into());
    }
    for (i, c) in name.char_indices() {
        if !(c.is_ascii_digit() || c.is_ascii_lowercase() || c == '-') {
            return Err(format!("name contains invalid char `{c}` at byte {i}"));
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

fn err<T: quote::ToTokens, R>(tokens: T, msg: impl Into<String>) -> syn::Result<R> {
    Err(syn::Error::new_spanned(tokens, msg.into()))
}
