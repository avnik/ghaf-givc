// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

//! Attribute macros that take the boilerplate out of tonic services.
//!
//! [`macro@grpc_server`] rewrites a service implementation so that every
//! handler unwraps its request and builds its response automatically:
//!
//! ```ignore
//! #[grpc_server]
//! impl pb::locale::locale_client_server::LocaleClient for LocaleServer {
//!     async fn locale_set(
//!         &self,
//!         request: pb::locale::LocaleMessage,
//!     ) -> anyhow::Result<pb::locale::Empty> {
//!         self.set_locale(&request.assignments)?;
//!         Ok(pb::locale::Empty {})
//!     }
//! }
//! ```
//!
//! The macro
//!
//! * adds `#[tonic::async_trait]`, so the attribute does not have to be
//!   repeated on every service,
//! * unwraps the `Request` into the request parameter, so the handler works
//!   on the message directly,
//!   and generates the `Request<Message>` parameter required by the tonic
//!   trait,
//! * builds the `Response`, so the handler only returns the message. The
//!   return type of the handler is rewritten to the one the trait expects,
//!   so the source signature can use `anyhow::Result<Message>` while the
//!   generated trait implementation uses `Result<Response<Message>, Status>`,
//! * converts errors into a status, so `?` works with `anyhow::Error` and
//!   `tonic::Status` alike. Errors keep their status code and the full error
//!   chain ends up in the status message.
//!
//! A handler can return a message as `Ok(message)`, or return a `Result`
//! directly. The source-facing error type may be `anyhow::Error` (including
//! the `anyhow::Result<T>` alias), while handlers that fail with a status keep
//! their status code, handlers
//! that fail with an `anyhow::Error` are reported as `Status::internal` with
//! the error chain as the message. `?` works with `tonic::Status`,
//! `anyhow::Error` and `std::io::Error`.
//!
//! Handlers written in the tonic style, returning `Result<Response<Message>,
//! Status>` and taking `Request<Message>`, keep working as well.
//!
//! # Options
//!
//! * `map_err = path`: `fn(anyhow::Error) -> tonic::Status` used to map
//!   errors that are not a status already.
//! * `into = name`: name of the binding that holds the unwrapped request.
//!   Defaults to the name of the request parameter.
//! * `log = false`: do not log handler failures. Failures are logged by
//!   default.
//! * `skip`: leave the method untouched.
//!
//! Options given on a method override the ones given on the impl block.
//!
//! # Requirements
//!
//! The crate using the macro needs a dependency on `tonic` and on `anyhow`.
//! Failures are logged with `tracing`, unless `log = false` is given.

use proc_macro::TokenStream;
use syn::{ItemImpl, parse_macro_input};

mod args;
mod expand;

use args::Args;

/// Implements a tonic service without the per handler boilerplate.
///
/// See the [crate documentation](crate) for the details.
#[proc_macro_attribute]
pub fn grpc_server(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as Args);
    let item = parse_macro_input!(input as ItemImpl);

    match expand::expand(&args, item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}
