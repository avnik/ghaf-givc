// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::process::Stdio;

use anyhow::{Result, bail};
use givc_common::{pb, tonic::RewrapErr};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tonic::{Request, Response, Status};

pub use pb::ctap::ctap_server::CtapServer as CtapServiceServer;

#[derive(Debug, Default, Clone)]
pub struct CtapService;

impl CtapService {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn resolve_program(req: &str) -> Result<&'static str> {
        match req {
            "ctap.ClientPin" => Ok("qctap-client-pin"),
            "ctap.GetInfo" => Ok("qctap-get-info"),
            "u2f.Authenticate" => Ok("qctap-get-assertion"),
            "u2f.Register" => Ok("qctap-make-credential"),
            _ => bail!("Invalid request"),
        }
    }
}

#[grpc_server]
impl pb::ctap::ctap_server::Ctap for CtapService {
    async fn ctap(&self, request: pb::ctap::CtapRequest) -> Result<pb::ctap::CtapResponse, Status> {
        let prog = Self::resolve_program(&request.req).rewrap_err()?;

        let mut cmd = Command::new(prog);
        cmd.args(request.args);
        cmd.stdin(Stdio::piped());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&request.payload).await;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|err| Status::internal(err.to_string()))?;
        Ok(pb::ctap::CtapResponse {
            output: output.stdout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_requests() {
        assert_eq!(
            CtapService::resolve_program("ctap.ClientPin").unwrap(),
            "qctap-client-pin"
        );
        assert_eq!(
            CtapService::resolve_program("u2f.Register").unwrap(),
            "qctap-make-credential"
        );
    }

    #[test]
    fn rejects_unknown_requests() {
        assert!(CtapService::resolve_program("nope").is_err());
    }
}
