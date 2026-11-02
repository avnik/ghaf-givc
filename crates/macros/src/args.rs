// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

//! Options accepted by the `#[grpc_server]` attribute macro.

use darling::FromMeta;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Path};

/// Options accepted by `#[grpc_server(...)]`.
///
/// The same options are understood on the impl block and on a single method;
/// the method level options win over the impl block ones.
#[derive(Clone, Default, FromMeta)]
pub(crate) struct Args {
    /// `fn(anyhow::Error) -> tonic::Status` used to map handler failures.
    ///
    /// Defaults to `tonic::Status::internal` with the full error chain.
    pub(crate) map_err: Option<Path>,
    /// Name of the binding that holds the unwrapped request.
    ///
    /// Defaults to the name of the request parameter.
    pub(crate) into: Option<Ident>,
    /// Whether handler failures are logged. Defaults to `true`.
    pub(crate) log: Option<bool>,
    /// Leave the method untouched.
    #[darling(default)]
    pub(crate) skip: bool,
}

impl Args {
    /// Combines the options of an impl block with the ones of a method.
    ///
    /// Options given on the method win over the ones given on the impl block.
    #[must_use]
    pub(crate) fn merge(&self, method: &Args) -> Args {
        Args {
            map_err: method.map_err.clone().or_else(|| self.map_err.clone()),
            into: method.into.clone().or_else(|| self.into.clone()),
            log: method.log.or(self.log),
            skip: self.skip || method.skip,
        }
    }
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let tokens = input.parse()?;
        let metas = darling::ast::NestedMeta::parse_meta_list(tokens)
            .map_err(|error| syn::Error::new(input.span(), error.to_string()))?;
        Self::from_list(&metas).map_err(|error| syn::Error::new(input.span(), error.to_string()))
    }
}
