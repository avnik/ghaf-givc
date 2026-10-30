// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

//! Expansion of the `#[grpc_server]` attribute macro.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::visit_mut::VisitMut;
use syn::{
    Attribute, Error, Expr, ExprAsync, ExprClosure, ExprPath, GenericArgument, Ident, ImplItem,
    ItemImpl, Pat, PathArguments, Result, ReturnType, Token, Type,
};

use crate::args::Args;

/// Name of the error type generated for every handler.
const ERROR_TYPE: &str = "__GivcHandlerError";

/// Rewrites an impl block into a tonic service implementation.
///
/// Every method with a request parameter and a `Result<_, _>` return type
/// is rewritten so that the request is unwrapped and the response is built
/// automatically. The `#[tonic::async_trait]` attribute is added as well.
pub(crate) fn expand(args: &Args, mut item: ItemImpl) -> Result<TokenStream2> {
    // The macro adds `#[tonic::async_trait]` itself. Drop a user supplied one
    // so that the attribute is not applied twice.
    item.attrs.retain(|attr| !is_async_trait(attr));

    let mut rewritten = false;

    for impl_item in &mut item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };

        let (method_args, skip) = take_method_args(&mut method.attrs)?;
        if skip {
            continue;
        }
        let method_args = method_args.map_or(args.clone(), |method_args| args.merge(&method_args));

        if method.sig.asyncness.is_some() {
            rewritten |= rewrite_handler(&method_args, method)?;
        } else if looks_like_expanded_handler(&method.sig) {
            return Err(Error::new_spanned(
                &method.sig,
                "#[grpc_server] must not be applied after #[tonic::async_trait]: \
                 the macro adds #[tonic::async_trait] itself, so drop the attribute",
            ));
        }
    }

    if rewritten {
        item.attrs
            .insert(0, syn::parse_quote!(#[tonic::async_trait]));
    }

    Ok(quote!(#item))
}

/// Rewrites a single handler method.
///
/// Returns `true` if the method was a handler and got rewritten.
fn rewrite_handler(args: &Args, method: &mut syn::ImplItemFn) -> Result<bool> {
    let Some(request) = handler_param(&method.sig) else {
        return Ok(false);
    };

    let handler = method.sig.ident.to_string();
    let Some(types) = response_message_type(&method.sig.output) else {
        return Ok(false);
    };
    let HandlerTypes { message } = types;

    // Tonic requires Request<T> in the trait method signature. Let the user
    // write T and restore that wrapper in the generated impl signature.
    let request_type = method.sig.inputs.iter_mut().find_map(|arg| {
        let syn::FnArg::Typed(pat_type) = arg else {
            return None;
        };
        matches!(&*pat_type.pat, Pat::Ident(pat_ident) if pat_ident.ident == request)
            .then_some(&mut pat_type.ty)
    });
    let Some(request_type) = request_type else {
        return Err(Error::new_spanned(
            &method.sig,
            "could not find the request parameter",
        ));
    };
    if !is_type_named(request_type, "Request") {
        let message_type = (**request_type).clone();
        **request_type = syn::parse_quote!(::tonic::Request<#message_type>);
    }

    let binding = args.into.clone().unwrap_or_else(|| request.clone());
    let error_type = error_type();
    let result = format_ident!("__givc_result");

    let mut body = std::mem::replace(&mut method.block, syn::parse_quote!({}));
    BodyRewriter.visit_block_mut(&mut body);
    if let Some(syn::Stmt::Expr(expression, None)) = body.stmts.last_mut()
        && !matches!(expression, Expr::Return(_) | Expr::Loop(_))
        && !matches!(expression, Expr::Call(call) if is_named(&call.func, "Ok") || is_named(&call.func, "Err"))
    {
        let tail = std::mem::replace(expression, Expr::Verbatim(quote!()));
        *expression = syn::parse_quote!((#tail).map_err(#error_type::from_error));
    }
    let statements = &body.stmts;
    let definitions = error_definitions(args, &handler);

    // The handler returns the message, the trait method returns the response.
    method.sig.output = syn::parse_quote!(
        -> ::std::result::Result<::tonic::Response<#message>, ::tonic::Status>
    );

    method.block = syn::parse_quote!({
        #definitions

        #[allow(unused_mut, unused_variables)]
        let mut #binding = #request.into_inner();

        let #result: ::std::result::Result<#message, #error_type> =
            async move { #(#statements)* }.await;

        #result
            .map(::tonic::Response::new)
            .map_err(#error_type::into_status)
    });

    Ok(true)
}

/// Generates the error type used to convert handler failures into a status.
///
/// The type is generated inside the handler body so that it cannot clash with
/// anything in the surrounding module.
fn error_definitions(args: &Args, handler: &str) -> TokenStream2 {
    let error_type = error_type();

    let mapper = match &args.map_err {
        Some(path) => quote!(#path(error)),
        None => quote!(::tonic::Status::internal("internal server error")),
    };

    let log = if args.log.unwrap_or(true) {
        quote!(::tracing::error!(handler = #handler, error = ?error, "grpc handler failed");)
    } else {
        quote!()
    };

    quote! {
        #[allow(dead_code)]
        enum #error_type {
            Status(::tonic::Status),
            Anyhow(::anyhow::Error),
        }

        impl From<::tonic::Status> for #error_type {
            fn from(value: ::tonic::Status) -> Self {
                Self::Status(value)
            }
        }

        impl From<::anyhow::Error> for #error_type {
            fn from(value: ::anyhow::Error) -> Self {
                Self::Anyhow(value)
            }
        }

        impl From<std::io::Error> for #error_type {
            fn from(value: std::io::Error) -> Self {
                Self::Anyhow(::anyhow::Error::from(value))
            }
        }

        impl #error_type {
            fn from_error<E>(value: E) -> Self
            where
                E: ::std::convert::Into<::anyhow::Error>,
            {
                Self::Anyhow(value.into())
            }

            fn into_status(self) -> ::tonic::Status {
                match self {
                    Self::Status(status) => status,
                    Self::Anyhow(error) => {
                        match error.downcast::<::tonic::Status>() {
                            Ok(status) => status,
                            Err(error) => {
                                #log
                                #mapper
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Rewrites the handler body.
///
/// `Ok(Response::new(..))` becomes `Ok(..)` and `Err(..)` is converted into
/// the generated error type, so that `?` works with both `anyhow::Error` and
/// `tonic::Status`.
struct BodyRewriter;

impl VisitMut for BodyRewriter {
    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        let last = block.stmts.len().checked_sub(1);
        for (index, statement) in block.stmts.iter_mut().enumerate() {
            if Some(index) == last
                && let syn::Stmt::Expr(expression, None) = statement
            {
                self.visit_result_expr_mut(expression);
            } else {
                self.visit_stmt_mut(statement);
            }
        }
    }

    fn visit_stmt_mut(&mut self, statement: &mut syn::Stmt) {
        if let syn::Stmt::Macro(statement_macro) = statement {
            let Some(name) = statement_macro.mac.path.segments.last() else {
                return;
            };
            let error_type = error_type();
            if name.ident == "bail" {
                let arguments = statement_macro.mac.tokens.clone();
                *statement = syn::parse_quote! {
                    return Err(#error_type::from_error(::anyhow::anyhow!(#arguments)));
                };
                return;
            }
            if name.ident == "ensure"
                && let Ok(arguments) = syn::parse2::<EnsureArgs>(statement_macro.mac.tokens.clone())
            {
                let EnsureArgs { condition, message } = arguments;
                *statement = syn::parse_quote! {
                    if !(#condition) {
                        return Err(#error_type::from_error(::anyhow::anyhow!(#message)));
                    }
                };
                return;
            }
        }
        syn::visit_mut::visit_stmt_mut(self, statement);
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        if let Expr::Return(return_expr) = expr {
            if let Some(value) = &mut return_expr.expr {
                self.visit_result_expr_mut(value);
            }
            return;
        }

        if let Expr::Match(match_expr) = expr {
            self.visit_expr_mut(&mut match_expr.expr);
            for arm in &mut match_expr.arms {
                if let Some((_, guard)) = &mut arm.guard {
                    self.visit_expr_mut(guard);
                }
                self.visit_result_expr_mut(&mut arm.body);
            }
            return;
        }

        if let Expr::Try(expression_try) = expr {
            let mut value = *expression_try.expr.clone();
            self.visit_expr_mut(&mut value);
            let error_type = error_type();
            *expression_try.expr = syn::parse_quote!((#value).map_err(#error_type::from_error));
            return;
        }

        if let Expr::Macro(expression_macro) = expr {
            let Some(name) = expression_macro.mac.path.segments.last() else {
                return;
            };
            let error_type = error_type();
            if name.ident == "bail" {
                let arguments = expression_macro.mac.tokens.clone();
                *expr = syn::parse_quote!(
                    return Err(#error_type::from_error(::anyhow::anyhow!(#arguments)))
                );
                return;
            }
            if name.ident == "ensure"
                && let Ok(arguments) =
                    syn::parse2::<EnsureArgs>(expression_macro.mac.tokens.clone())
            {
                let EnsureArgs { condition, message } = arguments;
                *expr = syn::parse_quote!({
                    if !(#condition) {
                        return Err(#error_type::from_error(::anyhow::anyhow!(#message)));
                    }
                });
                return;
            }
        }

        if let Expr::Call(call) = expr
            && call.args.len() == 1
        {
            if is_named(&call.func, "Ok") {
                if let Some(message) = response_new_arg(&mut call.args[0]) {
                    call.args[0] = message;
                }
            }
        }

        syn::visit_mut::visit_expr_mut(self, expr);
    }

    // Nested scopes keep their own error type.
    fn visit_expr_closure_mut(&mut self, _: &mut ExprClosure) {}
    fn visit_expr_async_mut(&mut self, _: &mut ExprAsync) {}
}

impl BodyRewriter {
    fn visit_result_expr_mut(&mut self, expression: &mut Expr) {
        if let Expr::Call(call) = expression
            && is_named(&call.func, "Err")
            && call.args.len() == 1
        {
            let error_type = error_type();
            let value = std::mem::replace(&mut call.args[0], Expr::Verbatim(quote!()));
            call.args[0] = syn::parse_quote!(#error_type::from_error(#value));
            return;
        }
        self.visit_expr_mut(expression);
    }
}

/// Arguments accepted by `bail!`/`ensure!`: a condition followed by an
/// arbitrary anyhow-compatible error expression or formatting arguments.
struct EnsureArgs {
    condition: Expr,
    message: TokenStream2,
}

impl Parse for EnsureArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let condition = input.parse()?;
        input.parse::<Token![,]>()?;
        let message = input.parse()?;
        Ok(Self { condition, message })
    }
}

/// Returns the name of the typed request parameter written by the user.
fn handler_param(signature: &syn::Signature) -> Option<Ident> {
    let mut typed = signature.inputs.iter().filter_map(|arg| {
        let syn::FnArg::Typed(pat_type) = arg else {
            return None;
        };
        Some(pat_type)
    });
    let pat_type = typed.next()?;
    if typed.next().is_some() {
        return None;
    }
    match &*pat_type.pat {
        Pat::Ident(pat_ident) => Some(pat_ident.ident.clone()),
        _ => None,
    }
}

/// Returns the name of a request parameter already using tonic's `Request<T>`.
fn request_param(signature: &syn::Signature) -> Option<Ident> {
    signature.inputs.iter().find_map(|arg| {
        let syn::FnArg::Typed(pat_type) = arg else {
            return None;
        };
        if !is_type_named(&pat_type.ty, "Request") {
            return None;
        }
        match &*pat_type.pat {
            Pat::Ident(pat_ident) => Some(pat_ident.ident.clone()),
            _ => None,
        }
    })
}

/// The types of a handler signature.
struct HandlerTypes {
    /// The message the handler returns.
    message: Type,
}

/// Returns the message type of a handler returning `Result<Message, _>`.
///
/// A `Response` in the return type is unwrapped, so that handlers written in
/// the tonic style keep working. The source-facing error type may be omitted
/// with `anyhow::Result<Message>` or may be any type with the two-argument
/// `Result<Message, Error>` form. The generated trait implementation always
/// returns `Result<Response<Message>, tonic::Status>`.
fn response_message_type(output: &ReturnType) -> Option<HandlerTypes> {
    let ReturnType::Type(_, ty) = output else {
        return None;
    };
    let result = type_arguments(ty, "Result")?;
    let ok = match result.as_slice() {
        [ok, _err] => ok,
        [ok] if is_anyhow_result(ty) => ok,
        _ => return None,
    };
    let message = match type_arguments(ok, "Response") {
        Some(arguments) => {
            let [message] = arguments.as_slice() else {
                return None;
            };
            message.clone()
        }
        None => ok.clone(),
    };

    Some(HandlerTypes { message })
}

/// Checks whether a one-argument `Result<T>` is `anyhow::Result<T>`.
fn is_anyhow_result(ty: &Type) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };
    type_path.path.segments.len() == 2
        && type_path.path.segments[0].ident == "anyhow"
        && type_path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Result")
}
/// Returns the type arguments of `ty`, if `ty` is a path ending in `name`.
fn type_arguments(ty: &Type, name: &str) -> Option<Vec<Type>> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != name {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    Some(
        arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                GenericArgument::Type(ty) => Some(ty.clone()),
                _ => None,
            })
            .collect(),
    )
}

/// Checks whether `ty` is a path ending in `name`.
fn is_type_named(ty: &Type, name: &str) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };
    type_path
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == name)
}

/// Checks whether `expr` is a call of a function named `name`.
fn is_named(expr: &Expr, name: &str) -> bool {
    let Expr::Path(ExprPath { path, .. }) = expr else {
        return false;
    };
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == name)
}

/// Checks whether `expr` is a call of `Response::new`.
fn is_response_new(expr: &Expr) -> bool {
    let Expr::Path(ExprPath { path, .. }) = expr else {
        return false;
    };
    let segments: Vec<_> = path.segments.iter().collect();
    segments.len() >= 2
        && segments[segments.len() - 2].ident == "Response"
        && segments[segments.len() - 1].ident == "new"
}

/// Unwraps `Response::new(message)` into `message`.
fn response_new_arg(argument: &mut Expr) -> Option<Expr> {
    let Expr::Call(call) = argument else {
        return None;
    };
    if !is_response_new(&call.func) || call.args.len() != 1 {
        return None;
    }
    Some(std::mem::replace(
        &mut call.args[0],
        Expr::Verbatim(quote!()),
    ))
}

/// Checks whether the attribute is `#[tonic::async_trait]`.
fn is_async_trait(attr: &Attribute) -> bool {
    let segments: Vec<String> = attr
        .path()
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    matches!(segments.as_slice(), [single] if single == "async_trait")
        || matches!(segments.as_slice(), [tonic, single] if tonic == "tonic" && single == "async_trait")
}

/// Collects the options of a single method and removes the attribute.
fn take_method_args(attrs: &mut Vec<Attribute>) -> Result<(Option<Args>, bool)> {
    let mut kept = Vec::new();
    let mut options = None;
    let mut skip = false;

    for attr in std::mem::take(attrs) {
        if !attr.path().is_ident("grpc_server") {
            kept.push(attr);
            continue;
        }
        let args = attr.parse_args::<Args>()?;
        if args.skip {
            skip = true;
        } else {
            options = Some(args);
        }
    }

    *attrs = kept;
    Ok((options, skip))
}

/// Checks whether the signature looks like a handler expanded by async-trait.
fn looks_like_expanded_handler(signature: &syn::Signature) -> bool {
    let ReturnType::Type(_, ty) = &signature.output else {
        return false;
    };
    let Type::Path(type_path) = ty.as_ref() else {
        return false;
    };
    let Some(segment) = type_path.path.segments.last() else {
        return false;
    };
    if segment.ident != "Pin" {
        return false;
    }
    request_param(signature).is_some()
}

/// Name of the generated error type.
fn error_type() -> Ident {
    format_ident!("{ERROR_TYPE}")
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    fn expand_impl(args: Args, item: proc_macro2::TokenStream) -> String {
        let item: ItemImpl = syn::parse2(item).expect("valid impl block");
        expand(&args, item).expect("expansion succeeds").to_string()
    }

    fn handler() -> proc_macro2::TokenStream {
        quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Message,
                ) -> Result<Reply, Status> {
                    let message = request.into_inner();
                    Ok(Response::new(Reply { value: message.value }))
                }
            }
        }
    }

    #[test]
    fn adds_async_trait() {
        let expanded = expand_impl(Args::default(), handler());
        assert!(expanded.contains("# [tonic :: async_trait]"), "{expanded}");
    }

    #[test]
    fn rewrites_return_type() {
        let expanded = expand_impl(Args::default(), handler());
        assert!(
            expanded.contains(
                "-> :: std :: result :: Result < :: tonic :: Response < Reply > , :: tonic :: Status >",
            ),
            "{expanded}"
        );
    }

    #[test]
    fn wraps_message_parameter_in_tonic_request() {
        let expanded = expand_impl(Args::default(), handler());
        assert!(
            expanded.contains("request : :: tonic :: Request < Message >"),
            "{expanded}"
        );
    }

    #[test]
    fn converts_errors_from_a_returned_result() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Message,
                ) -> Result<Reply, Status> {
                    make_reply(request).await
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(
            expanded.contains(
                "make_reply (request) . await) . map_err (__GivcHandlerError :: from_error)",
            ),
            "{expanded}"
        );
    }

    #[test]
    fn accepts_anyhow_result_signature() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Message,
                ) -> anyhow::Result<Reply> {
                    make_reply(request).await
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(
            expanded.contains(
                "-> :: std :: result :: Result < :: tonic :: Response < Reply > , :: tonic :: Status >",
            ),
            "{expanded}"
        );
    }

    #[test]
    fn converts_anyhow_bail_macro_errors() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Message,
                ) -> Result<Reply, Status> {
                    anyhow::bail!("failed: {}", request.value);
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(
            expanded
                .contains("return Err (__GivcHandlerError :: from_error (:: anyhow :: anyhow !"),
            "{expanded}"
        );
    }

    #[test]
    fn accepts_tonic_style_return_type() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Request<Message>,
                ) -> Result<Response<Reply>, Status> {
                    Ok(Response::new(Reply { value: 0 }))
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(expanded.contains("Response < Reply >"), "{expanded}");
        assert!(!expanded.contains("Response :: new (Reply"), "{expanded}");
    }

    #[test]
    fn leaves_non_handler_methods_untouched() {
        let item = quote! {
            impl Service for Server {
                async fn handle(&self, request: Request<Message>) -> Reply {
                    Reply { value: 0 }
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(expanded.contains("Request < Message >"), "{expanded}");
        assert!(!expanded.contains("tonic :: async_trait"), "{expanded}");
    }

    #[test]
    fn unwraps_request_and_builds_response() {
        let expanded = expand_impl(Args::default(), handler());
        assert!(
            expanded.contains("let mut request = request . into_inner ()"),
            "{expanded}"
        );
        assert!(
            expanded.contains(". map (:: tonic :: Response :: new)"),
            "{expanded}"
        );
    }

    #[test]
    fn drops_response_new() {
        let expanded = expand_impl(Args::default(), handler());
        assert!(!expanded.contains("Response :: new (Reply"), "{expanded}");
        assert!(expanded.contains("Ok (Reply"), "{expanded}");
    }

    #[test]
    fn converts_errors() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Request<Message>,
                ) -> Result<Response<Reply>, Status> {
                    Err(Status::internal("boom"))
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(
            expanded.contains("__GivcHandlerError :: from_error (Status :: internal (\"boom\"))"),
            "{expanded}"
        );
    }

    #[test]
    fn converts_anyhow_ensure_macro_errors() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Message,
                ) -> anyhow::Result<Reply> {
                    anyhow::ensure!(request.value > 0, "value must be positive");
                    Ok(Reply { value: request.value })
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(
            expanded.contains("if ! (request . value > 0)")
                && expanded.contains("from_error (:: anyhow :: anyhow !"),
            "{expanded}"
        );
    }

    #[test]
    fn uses_custom_mapper() {
        let item = quote! {
            impl Service for Server {
                async fn handle(
                    &self,
                    request: Request<Message>,
                ) -> Result<Response<Reply>, Status> {
                    Ok(Reply { value: 0 })
                }
            }
        };
        let args: Args = syn::parse2(quote!(map_err = my_mapper)).expect("valid options");
        let expanded = expand_impl(args, item);
        assert!(expanded.contains("my_mapper (error)"), "{expanded}");
    }

    #[test]
    fn merges_method_options() {
        let args: Args =
            syn::parse2(quote!(map_err = impl_mapper, log = false)).expect("valid options");
        let item = quote! {
            impl Service for Server {
                #[grpc_server(map_err = method_mapper)]
                async fn handle(
                    &self,
                    request: Request<Message>,
                ) -> Result<Response<Reply>, Status> {
                    Ok(Reply { value: 0 })
                }
            }
        };
        let expanded = expand_impl(args, item);
        assert!(expanded.contains("method_mapper (error)"), "{expanded}");
        assert!(!expanded.contains("impl_mapper"), "{expanded}");
        assert!(!expanded.contains("tracing"), "{expanded}");
    }

    #[test]
    fn skips_methods() {
        let item = quote! {
            impl Service for Server {
                #[grpc_server(skip)]
                async fn handle(
                    &self,
                    request: Request<Message>,
                ) -> Result<Response<Reply>, Status> {
                    Ok(Response::new(Reply { value: 0 }))
                }
            }
        };
        let expanded = expand_impl(Args::default(), item);
        assert!(!expanded.contains("into_inner"), "{expanded}");
        assert!(!expanded.contains("async_trait"), "{expanded}");
    }

    #[test]
    fn rejects_async_trait_expanded_handlers() {
        let item = quote! {
            impl Service for Server {
                fn handle(
                    &self,
                    request: Request<Message>,
                ) -> Pin<Box<dyn Future<Output = Result<Response<Reply>, Status>> + Send>> {
                    Box::pin(async move { Ok(Response::new(Reply { value: 0 })) })
                }
            }
        };
        let args = Args::default();
        let error = expand(&args, syn::parse2(item).expect("valid impl block"))
            .expect_err("expansion fails");
        assert!(error.to_string().contains("async_trait"), "{error}");
    }
}
