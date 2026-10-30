// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

//! Options accepted by the `#[grpc_server]` attribute macro.

use syn::parse::{Parse, ParseStream};
use syn::{Error, Ident, LitBool, Path, Token};

/// Options accepted by `#[grpc_server(...)]`.
///
/// The same options are understood on the impl block and on a single method;
/// the method level options win over the impl block ones.
#[derive(Clone, Default)]
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
        let mut args = Args::default();

        while !input.is_empty() {
            let name: Ident = input.parse().map_err(|_| {
                Error::new(
                    input.span(),
                    "expected an option name: `map_err`, `into`, `log` or `skip`",
                )
            })?;

            match name.to_string().as_str() {
                "map_err" => {
                    input.parse::<Token![=]>()?;
                    args.map_err = Some(input.parse()?);
                }
                "into" => {
                    input.parse::<Token![=]>()?;
                    args.into = Some(input.parse()?);
                }
                "log" => {
                    args.log = if input.peek(Token![=]) {
                        input.parse::<Token![=]>()?;
                        Some(parse_bool(input)?)
                    } else {
                        Some(true)
                    };
                }
                "skip" => {
                    args.skip = true;
                }
                other => {
                    return Err(Error::new(
                        name.span(),
                        format!(
                            "unknown option `{other}`; expected `map_err`, `into`, `log` or `skip`"
                        ),
                    ));
                }
            }

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(args)
    }
}

/// Parses a boolean literal.
fn parse_bool(input: ParseStream) -> syn::Result<bool> {
    let literal: LitBool = input.parse()?;
    Ok(literal.value)
}
