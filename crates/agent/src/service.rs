// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use std::pin::Pin;

use givc_macros::grpc_server;
use tonic::Status;

pub use givc_common::pb::systemd::unit_control_service_server::UnitControlServiceServer;

type Stream<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Debug, Clone, Default)]
pub struct UnitControlService;

impl UnitControlService {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[grpc_server]
impl givc_common::pb::systemd::unit_control_service_server::UnitControlService
    for UnitControlService
{
    async fn get_unit_status(
        &self,
        _request: givc_common::pb::systemd::UnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    async fn start_unit(
        &self,
        _request: givc_common::pb::systemd::UnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    async fn stop_unit(
        &self,
        _request: givc_common::pb::systemd::UnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    async fn kill_unit(
        &self,
        _request: givc_common::pb::systemd::UnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    async fn freeze_unit(
        &self,
        _request: givc_common::pb::systemd::UnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    async fn unfreeze_unit(
        &self,
        _request: givc_common::pb::systemd::UnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    type MonitorUnitStream = Stream<givc_common::pb::systemd::UnitResourceResponse>;

    async fn monitor_unit(
        &self,
        _request: givc_common::pb::systemd::UnitResourceRequest,
    ) -> Result<Self::MonitorUnitStream, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }

    async fn start_application(
        &self,
        _request: givc_common::pb::systemd::AppUnitRequest,
    ) -> Result<givc_common::pb::systemd::UnitResponse, Status> {
        Err(Status::unimplemented("agent boilerplate"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use givc_common::pb::systemd::unit_control_service_server::UnitControlService as UnitControl;
    use tonic::Request;

    #[tokio::test]
    async fn handler_keeps_status_code() {
        let server = UnitControlService::new();
        let err = server
            .get_unit_status(Request::new(givc_common::pb::systemd::UnitRequest {
                unit_name: "agent.service".to_owned(),
            }))
            .await
            .expect_err("boilerplate service is not implemented");

        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert!(err.message().contains("agent boilerplate"), "{err}");
    }
}
