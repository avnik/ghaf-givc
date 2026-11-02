// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use std::pin::Pin;
use thiserror::Error;
use tonic::{Code, Status};
use tracing::{debug, error};

use crate::admin::registry::NotRegistered;

pub(crate) type Stream<T> =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Debug, Error)]
pub(crate) enum AdminError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl AdminError {
    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }
}

pub(crate) fn invalid_request(message: impl Into<String>) -> anyhow::Error {
    anyhow!(AdminError::invalid_request(message))
}

/// Is this error just "the caller asked about something not registered yet"?
///
/// Must go through `anyhow::Error::downcast_ref`, which knows how to look
/// inside the layers `.context()` adds. Walking `chain()` and testing each
/// `&dyn Error` with `is::<NotRegistered>()` does NOT work: the chain yields
/// anyhow's own wrapper around the context value, so the test is always false
/// and the classification silently does nothing.
fn is_expected_miss(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotRegistered>().is_some()
}

// Kludge: wrap_error have .into() semantic, so should be destructive
// Clippy hint here use &anyhow::Error, but implementing it trigger another clippy warning,
// suggests to pass by-value here.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn wrap_error(any_err: anyhow::Error) -> tonic::Status {
    // Preserve a status that was wrapped in anyhow without cloning it. This
    // matters for helpers returning anyhow::Result that use `?` on a Status.
    let any_err = match any_err.downcast::<tonic::Status>() {
        Ok(status) => return status,
        Err(any_err) => any_err,
    };

    // Convert root cause and stack to strings
    let stack: Vec<_> = any_err.chain().skip(1).map(ToString::to_string).collect();
    let cause = any_err.root_cause().to_string();

    // ...then dump them, at a level that matches how alarming they are.
    if is_expected_miss(&any_err) {
        debug!("Local error cause is {cause}");
        for each in &stack {
            debug!("Local reasons is {each}");
        }
    } else {
        error!("Local error cause is {cause}");
        for each in &stack {
            error!("Local reasons is {each}");
        }
    }

    // Generate a stable public status. Detailed causes are logged server-side
    // and are not exposed through the gRPC response.
    let code = if is_expected_miss(&any_err) {
        Code::NotFound
    } else if any_err.downcast_ref::<AdminError>().is_some() {
        Code::InvalidArgument
    } else {
        Code::Internal
    };
    let message = match code {
        Code::NotFound => "service is not registered",
        Code::InvalidArgument => "request contains invalid arguments",
        _ => "internal server error",
    };
    Status::new(code, message)
}

pub(crate) trait WrapError<T> {
    #[allow(clippy::result_large_err)]
    fn wrap_error(self) -> Result<T, tonic::Status>;
}

impl<T> WrapError<T> for Result<T, anyhow::Error> {
    fn wrap_error(self) -> Result<T, tonic::Status> {
        self.map_err(wrap_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::registry::Registry;

    /// The error a client actually provokes by asking about a VM that has not
    /// registered yet. Built through `Registry`, not by hand, so the test fails
    /// if `by_name` ever stops attaching the marker.
    #[test]
    fn miss_from_registry_is_expected() {
        let err = Registry::new()
            .by_name("givc-gui-vm.service")
            .expect_err("empty registry must not resolve a name");
        assert!(
            is_expected_miss(&err),
            "a registry miss must be classified as expected, got: {err:?}"
        );
    }

    #[test]
    fn unrelated_error_is_not_expected() {
        let err = anyhow::anyhow!("disk on fire").context("while doing something important");
        assert!(
            !is_expected_miss(&err),
            "an unrelated error must keep error-level logging"
        );
    }

    #[test]
    fn wrapped_status_is_preserved() {
        let expected =
            Status::with_details(Code::PermissionDenied, "access denied", "details".into());
        let actual = wrap_error(anyhow::Error::new(expected));

        assert_eq!(actual.code(), Code::PermissionDenied);
        assert_eq!(actual.message(), "access denied");
        assert!(actual.details().is_empty());
    }

    #[test]
    fn registry_miss_maps_to_not_found() {
        let error = Registry::new()
            .by_name("missing.service")
            .expect_err("empty registry must not resolve a name");
        let status = wrap_error(error);

        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "service is not registered");
    }
}
