// SPDX-FileCopyrightText: 2025-2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::similar_names)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_stream::try_stream;
use givc_common::query::Event;
use regex::Regex;
use tokio::sync::Mutex;
use tonic::{Code, Status};
use tracing::{debug, error, info, trace};

use givc_client::endpoint::{EndpointConfig, TlsConfig};
use givc_common::query::QueryResult;
use givc_macros::grpc_server;
use givc_policyadmin::policy::run_policy_admin;
use givc_policyadmin::policy_manager::{PolicyManager, Update, UpdateReceiver};

use crate::admin::entry::{Placement, RegistryEntry};
use crate::admin::policyclient::PolicyAdminClient;
use crate::admin::registry::Registry;
use crate::pb::{
    self, ApplicationRequest, ApplicationResponse, Empty, LocaleRequest, QueryListResponse,
    RegistryRequest, RegistryResponse, StartResponse, StartVmRequest, TimezoneRequest,
    UnitStatusRequest, WatchItem,
    ctap::CtapRequest,
    ctap::CtapResponse,
    update::{
        ImageInstallRequest, ListGenerationsResponse, RegistryChangelogRequest,
        RegistryChangelogResponse, RegistryDiscoverRequest, RegistryDiscoverResponse,
        RegistryPullRequest, SetGenerationRequest,
    },
};
use crate::systemd_api::client::SystemDClient;
use crate::types::{ServiceType, UnitType, VmType};
use crate::utils::naming::VmName;
use crate::utils::tonic::{Stream, invalid_request};

pub use pb::admin_service_server::AdminServiceServer;

const VM_STARTUP_TIME: Duration = Duration::new(10, 0);
const AGENT_REGISTRATION_TIMEOUT: Duration = Duration::new(60, 0);
const TIMEZONE_CONF: &str = "/etc/timezone.conf";
const LOCALE_CONF: &str = "/etc/locale-givc.conf";

// FIXME: this is almost copy of sysfsm::Event.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum State {
    Init,
    InitComplete,
    HostRegistered,
    VmsRegistered,
}

struct AdminServiceImpl {
    registry: Registry,
    state: State, // FIXME: use sysfsm statemachine
    tls_config: Option<TlsConfig>,
    locale_assigns: Mutex<Vec<pb::locale::LocaleAssignment>>,
    timezone: Mutex<String>,
    policy_manager: Option<Arc<PolicyManager>>,
}

#[derive(Clone)]
pub struct AdminService {
    inner: Arc<AdminServiceImpl>,
}

struct Validator();

impl Validator {
    pub fn validate_locale(locale: &str) -> bool {
        let validator = Regex::new(
            r"^(?:C|POSIX|[a-z]{2}(?:_[A-Z]{2})?(?:@[a-zA-Z0-9]+)?)(?:\.[-a-zA-Z0-9]+)?$",
        )
        .unwrap();
        validator.is_match(locale)
    }
    pub fn validate_timezone(timezone: &str) -> bool {
        let validator = Regex::new(r"^[A-Z][-+a-zA-Z0-9]*(?:/[A-Z][-+a-zA-Z0-9_]*)*$").unwrap();
        validator.is_match(timezone)
    }
}

impl AdminService {
    /**
     * Create new `AdminService` instance
     *
     * # Errors
     * Fails if policy setup fails.
     */
    pub fn new(
        use_tls: Option<TlsConfig>,
        monitoring: bool,
        enable_policy_admin: bool,
        policy_store: Option<PathBuf>,
        policy_config: Option<String>,
    ) -> anyhow::Result<Self> {
        let (manager, updates) = if enable_policy_admin {
            Some(AdminServiceImpl::setup_policy_admin(
                policy_store,
                policy_config,
            )?)
        } else {
            None
        }
        .unzip();
        let inner = Arc::new(AdminServiceImpl::new(use_tls, manager));
        if monitoring {
            let clone = inner.clone();
            tokio::task::spawn(async move {
                clone.monitor().await;
            });
        }

        if let Some(mut updates) = updates {
            let inner = inner.clone();
            tokio::spawn(async move {
                while let Some((
                    Update {
                        vm_name,
                        file,
                        policy,
                    },
                    tx,
                )) = updates.recv().await
                {
                    let _ = tx.send(inner.push_policy_update(&vm_name, &file, &policy).await);
                }
            });
        }

        Ok(Self { inner })
    }
}

impl AdminServiceImpl {
    #[must_use]
    fn new(use_tls: Option<TlsConfig>, policy_manager: Option<Arc<PolicyManager>>) -> Self {
        let timezone = std::fs::read_to_string(TIMEZONE_CONF)
            .ok()
            .and_then(|l| l.lines().next().map(ToOwned::to_owned))
            .unwrap_or_default();
        let locale_assigns = std::fs::read_to_string(LOCALE_CONF)
            .map(|content| {
                content
                    .lines()
                    .filter_map(|line| {
                        let (key, value) = line.split_once('=')?;
                        let key_enum = pb::locale::LocaleMacroKey::from_str_name(key)?;
                        Some(pb::locale::LocaleAssignment {
                            key: key_enum as i32,
                            value: value.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self {
            registry: Registry::new(),
            state: State::Init,
            tls_config: use_tls,
            timezone: Mutex::new(timezone),
            locale_assigns: Mutex::new(locale_assigns),
            policy_manager,
        }
    }

    fn setup_policy_admin(
        policy_store: Option<PathBuf>,
        policy_config: Option<String>,
    ) -> anyhow::Result<(Arc<PolicyManager>, UpdateReceiver)> {
        run_policy_admin(policy_store, policy_config)
    }

    fn host_endpoint(&self) -> anyhow::Result<EndpointConfig> {
        let host_mgr = self.registry.by_type(UnitType {
            vm: VmType::Host,
            service: ServiceType::Mgr,
        })?;
        self.endpoint(&host_mgr).context("Resolving host agent")
    }

    fn net_vm_endpoint(&self) -> anyhow::Result<EndpointConfig> {
        let net_vm = VmName::Vm("net-vm").agent_service();
        self.agent_endpoint(&net_vm)
            .with_context(|| "Resolving net-vm agent")
    }

    fn endpoint(&self, entry: &RegistryEntry) -> anyhow::Result<EndpointConfig> {
        let transport = match &entry.placement {
            Placement::Managed { by: parent, .. } => {
                let parent = self.registry.by_name(parent)?;
                parent
                    .agent()
                    .with_context(|| "When get_remote_status()")?
                    .to_owned() // Fail, if parent also `Managed`
            }
            Placement::Endpoint { endpoint, .. } => endpoint.clone(), // FIXME: avoid clone!
            Placement::Host => bail!("endpoint() called for Host"), // impossible, FIXME: should never happens atm
        };
        let tls_name = transport.tls_name.clone();
        Ok(EndpointConfig {
            transport,
            tls: self.tls_config.clone().map(|mut tls| {
                tls.tls_name = Some(tls_name);
                tls
            }),
        })
    }

    fn agent_endpoint(&self, name: &str) -> anyhow::Result<EndpointConfig> {
        let reentry = self.registry.by_name(name)?;
        self.endpoint(&reentry)
    }

    fn app_entries(&self, name: &str) -> anyhow::Result<Vec<String>> {
        if name.contains('@') {
            let list = self.registry.find_names(name)?;
            Ok(list)
        } else {
            Ok(vec![name.to_owned()])
        }
    }

    async fn get_remote_status(
        &self,
        entry: &RegistryEntry,
    ) -> anyhow::Result<crate::types::UnitStatus> {
        let endpoint = self.endpoint(entry)?;
        let client = SystemDClient::new(endpoint);
        client.get_remote_status(entry.name.clone()).await
    }

    async fn send_system_command(&self, name: String) -> anyhow::Result<()> {
        let endpoint = self.host_endpoint()?;
        let client = SystemDClient::new(endpoint);
        client.start_remote(name).await?;
        Ok(())
    }

    async fn get_sysinfo(&self) -> anyhow::Result<pb::stats::SysinfoResponse> {
        let endpoint = self.host_endpoint()?;
        let conn = endpoint.connect().await?;
        let mut client = pb::stats::stats_service_client::StatsServiceClient::new(conn);
        let response = client.get_sysinfo(pb::stats::StatsRequest {}).await?;
        Ok(response.into_inner())
    }

    async fn push_policy_update(
        &self,
        vm_name: &str,
        policy_file_path: &std::path::Path,
        policy_name: &str,
    ) -> anyhow::Result<()> {
        let agent_service_name = VmName::Vm(vm_name).agent_service();
        info!(
            "policy-admin: pushing policy update to vm '{}' via agent '{}'",
            vm_name, agent_service_name
        );

        let endpoint = self.agent_endpoint(&agent_service_name).with_context(|| {
            format!("policy-admin: failed to get endpoint for agent {agent_service_name}")
        })?;

        let client = PolicyAdminClient::new(endpoint);

        /* For safe streaming  */
        let temp_path = policy_file_path.with_extension("tmp");
        tokio::fs::copy(policy_file_path, &temp_path).await?;
        let result = client
            .upload_policy(policy_name.to_string(), temp_path.clone())
            .await;
        let _ = tokio::fs::remove_file(&temp_path).await;
        result
    }

    async fn start_unit_on_vm(&self, unit: &str, vmname: &str) -> anyhow::Result<String> {
        let vmservice = VmName::Vm(vmname).agent_service();

        /* Return error if the vm is not registered */
        let endpoint = self
            .agent_endpoint(&vmservice)
            .with_context(|| format!("{vmservice} not registered"))?;
        let client = SystemDClient::new(endpoint);

        /* Check status of the unit */
        match client.get_remote_status(unit.into()).await {
            Ok(status) if status.load_state == "loaded" => {
                /* No action, if the unit is loaded and already running. */
                if status.active_state == "active" && status.sub_state == "running" {
                    info!("Service {unit} is already in running state!");
                } else {
                    /* Start the unit if it is loaded and not running. */
                    client.start_remote(unit.into()).await?;
                }
                Ok(vmservice)
            }
            Ok(_) => {
                /* Error, if the unit is not loaded. */
                Err(anyhow!("Service {unit} is not loaded!"))
            }
            Err(e) => {
                error!("Error retrieving unit status: {e}");
                Err(e)
            }
        }
    }

    async fn stop_unit_on_vm(&self, unit: &str, vmname: &str) -> anyhow::Result<()> {
        let vmservice = VmName::Vm(vmname).agent_service();

        /* Return error if the vm is not registered */
        let endpoint = self
            .agent_endpoint(&vmservice)
            .with_context(|| format!("{vmservice} not registered"))?;
        let client = SystemDClient::new(endpoint);

        /* Stop the unit on the remote VM */
        let status = client.stop_remote(unit.into()).await?;
        ensure!(status.is_exitted(), "Failed to stop {unit} on {vmname}");
        info!("Service {unit} stopped on {vmname}");
        Ok(())
    }

    async fn start_vm(&self, name: &str) -> anyhow::Result<()> {
        let endpoint = self.host_endpoint()?;
        let client = SystemDClient::new(endpoint);

        let status = client
            .get_remote_status(name.to_string())
            .await
            .with_context(|| format!("cannot retrieve vm status for {name}, host agent failed"))?;

        if status.load_state != "loaded" {
            bail!("vm {name} not loaded");
        }

        if status.active_state != "active" {
            client
                .start_remote(name.to_string())
                .await
                .with_context(|| format!("spawn remote VM service {name}"))?;

            tokio::time::sleep(VM_STARTUP_TIME).await;

            let new_status = client
                .get_remote_status(name.to_string())
                .await
                .with_context(|| format!("cannot retrieve vm status for {name}"))?;

            if new_status.active_state != "active" {
                bail!("Unable to launch VM {name}")
            }
        }
        Ok(())
    }

    async fn get_unit_status(
        &self,
        vm_service: String,
        unit_name: String,
    ) -> anyhow::Result<pb::systemd::UnitStatus> {
        let endpoint = self
            .agent_endpoint(&vm_service)
            .with_context(|| format!("{vm_service} not registered"))?;
        let client = SystemDClient::new(endpoint);

        /* Check status of the unit */
        match client.get_remote_status(unit_name).await {
            Err(e) => {
                error!("Error retrieving unit status: {e}");
                Err(e)
            }
            Ok(status) => Ok(status.into()),
        }
    }

    async fn handle_error(&self, entry: RegistryEntry) -> anyhow::Result<()> {
        info!(
            "Handling error for {} vm type {} service type {}",
            entry.name, entry.r#type.vm, entry.r#type.service
        );
        match (entry.r#type.vm, entry.r#type.service) {
            (VmType::AppVM, ServiceType::App) => {
                if entry.status.is_exitted() {
                    debug!("Deregister exitted {}", entry.name);
                    self.registry.deregister(&entry.name)?;
                }
                Ok(())
            }
            (VmType::AppVM | VmType::SysVM, ServiceType::Mgr) => {
                if let Placement::Managed { vm: vm_name, .. } = entry.placement {
                    self.start_vm(&vm_name)
                        .await
                        .with_context(|| format!("handing error, by restart VM {}", entry.name))?;
                }
                Ok(()) // FIXME: should use `?` from line above, why it didn't work?
            }
            (x, y) => {
                error!("Don't known how to handle_error for VM type: {x:?}:{y:?}");
                Ok(())
            }
        }
    }

    async fn monitor_routine(&self, entry: RegistryEntry) -> anyhow::Result<()> {
        match self.get_remote_status(&entry).await {
            Err(err) => {
                error!("could not get status of unit {}: {}", entry.name, err);
                self.handle_error(entry)
                    .await
                    .context("during handle error")?;
            }
            Ok(status) => {
                let invalid = !status.is_valid();
                if invalid {
                    error!("Status of {} is invalid: {status:?}", entry.name);
                }
                let inactive = status.active_state != "active";
                // Difference from "go" algorithm -- save new status before recovering attempt
                if inactive {
                    error!(
                        "Status of {} is {}, instead of active. Recovering.",
                        entry.name, status.active_state
                    );
                }

                trace!("Status of {} is {:?} (updated)", entry.name, status);
                // We have immutable copy of entry here, but need update _in registry_ copy
                self.registry.update_state(&entry.name, status)?;

                if invalid || inactive {
                    self.handle_error(entry)
                        .await
                        .context("during handle error")?;
                }
            }
        }
        Ok(())
    }

    async fn monitor(&self) {
        use tokio::time::{MissedTickBehavior, interval};
        let mut watch = interval(Duration::from_secs(5));
        watch.set_missed_tick_behavior(MissedTickBehavior::Delay);
        watch.tick().await; // First tick fires instantly
        loop {
            watch.tick().await;
            let watch_list = self.registry.watch_list();
            for entry in watch_list {
                trace!("Monitoring {}", entry.name);
                let name = entry.name.clone();
                if let Err(err) = self.monitor_routine(entry).await {
                    error!("Error during watch {}: {err}", name);
                }
            }
        }
    }

    // Refactoring kludge
    pub fn register(&self, entry: RegistryEntry) {
        self.registry.register(entry);
    }

    async fn start_app(&self, req: ApplicationRequest) -> anyhow::Result<String> {
        if self.state != State::VmsRegistered {
            info!("not all required system-vms are registered");
        }
        let name = req.app_name;
        let vm = req
            .vm_name
            .as_deref()
            .map_or(VmName::App(&name), VmName::Vm);
        let vm_name = vm.vm_service();
        let systemd_agent_name = vm.agent_service();

        info!("Starting app {name} on {vm_name} via {systemd_agent_name}");

        // Entry unused in "go" code
        if self.registry.by_name(&systemd_agent_name).is_err() {
            info!("Starting up VM {vm_name}");
            self.start_vm(&vm_name)
                .await
                .with_context(|| format!("Starting vm for {name}"))?;
            info!("Waiting for {systemd_agent_name} to register");
            self.registry
                .wait_for(&systemd_agent_name, AGENT_REGISTRATION_TIMEOUT)
                .await
                .context("after starting VM")?;
        }
        let endpoint = self
            .agent_endpoint(&systemd_agent_name)
            .with_context(|| format!("while lookung up {systemd_agent_name} for {vm_name}"))?;
        let client = SystemDClient::new(endpoint);
        let app_name = self.registry.create_unique_entry_name(&name);
        let status = client.start_application(app_name.clone(), req.args).await?;
        let remote_name = status.clone().name;
        if status.active_state == "active" {
            let app_entry = RegistryEntry {
                name: remote_name.clone(),
                status: status.clone(),
                watch: true,
                r#type: UnitType {
                    vm: VmType::AppVM,
                    service: ServiceType::App,
                },
                placement: Placement::Managed {
                    by: systemd_agent_name,
                    vm: vm_name,
                },
            };
            self.registry.register(app_entry);
        }
        Ok(remote_name)
    }

    async fn notify_user(
        &self,
        vm_name: String,
        notification: pb::notify::UserNotification,
    ) -> anyhow::Result<pb::notify::Status> {
        let endpoint = self.agent_endpoint(&vm_name)?;
        let conn = endpoint.connect().await?;
        let mut client =
            pb::notify::user_notification_service_client::UserNotificationServiceClient::new(conn);
        let response = client.notify_user(notification).await?;
        Ok(response.into_inner())
    }
}

#[allow(clippy::unnecessary_wraps)]
fn app_success() -> anyhow::Result<ApplicationResponse> {
    // FIXME: what should be response
    let res = ApplicationResponse {
        cmd_status: String::from("Command successful."),
        app_status: String::from("Command successful."),
    };
    Ok(res)
}

#[grpc_server(map_err = crate::utils::tonic::wrap_error, log = false)]
impl pb::admin_service_server::AdminService for AdminService {
    async fn register_service(
        &self,
        req: RegistryRequest,
    ) -> std::result::Result<pb::RegistryResponse, tonic::Status> {
        info!("Registering service {:?}", req);
        let entry = RegistryEntry::try_from(req)
            .map_err(|e| Status::new(Code::InvalidArgument, format!("{e}")))?;
        let notify = matches!(
            entry.r#type,
            UnitType {
                service: ServiceType::Mgr,
                ..
            }
        )
        .then(|| entry.name.clone());

        let mut need_update = None;
        if !entry.status.is_valid() {
            need_update = Some(entry.clone());
        }

        self.inner.register(entry);

        if let Some(entry) = need_update {
            let inner = self.inner.clone(); // is Arc<>
            tokio::spawn(async move { inner.monitor_routine(entry).await });
        }

        let res = RegistryResponse { error: None };

        if let Some(name) = notify
            && let Ok(endpoint) = self.inner.agent_endpoint(&name)
        {
            let locale_assigns = self.inner.locale_assigns.lock().await.clone();
            let timezone = self.inner.timezone.lock().await.clone();
            let vm_name = endpoint.transport.tls_name.clone();
            let policy_manager = self.inner.policy_manager.clone();

            tokio::spawn(async move {
                if let Ok(conn) = endpoint.connect().await {
                    if let Some(policy_manager) = policy_manager {
                        debug!("policy-admin: sending policy updates to vm '{}'", vm_name);
                        let _ = policy_manager.send_all_policies(&vm_name);
                    } else {
                        debug!("policy-admin: disabled");
                    }
                    let mut client =
                        pb::locale::locale_client_client::LocaleClientClient::new(conn);

                    // Only propagate state we actually have. Both fields default to
                    // empty when /etc/locale-givc.conf and /etc/timezone.conf are
                    // empty or absent, which is the normal state of a freshly
                    // installed system.
                    if locale_assigns.is_empty() {
                        debug!("locale: nothing set yet, not propagating to '{vm_name}'");
                    } else {
                        let localemsg = pb::locale::LocaleMessage {
                            assignments: locale_assigns,
                        };
                        let _ = client.locale_set(localemsg).await;
                    }

                    if timezone.is_empty() {
                        debug!("timezone: nothing set yet, not propagating to '{vm_name}'");
                    } else {
                        let timezonemsg = pb::locale::TimezoneMessage { timezone };
                        let _ = client.timezone_set(timezonemsg).await;
                    }
                }
            });
        }
        debug!("Responding with {res:?}");
        Ok(res)
    }

    async fn start_application(
        &self,
        request: ApplicationRequest,
    ) -> std::result::Result<StartResponse, tonic::Status> {
        let app_name = self.inner.start_app(request).await?;
        Ok(StartResponse {
            registry_id: app_name,
        })
    }

    async fn start_vm(
        &self,
        request: StartVmRequest,
    ) -> std::result::Result<StartResponse, tonic::Status> {
        let vm_name = VmName::Vm(&request.vm_name);
        self.inner.start_vm(&vm_name.vm_service()).await?;
        Ok(StartResponse {
            registry_id: vm_name.agent_service(),
        })
    }

    async fn start_service(
        &self,
        request: givc_common::pb::ServiceRequest,
    ) -> std::result::Result<StartResponse, tonic::Status> {
        let registry_id = self
            .inner
            .start_unit_on_vm(&request.service_name, &request.vm_name)
            .await?;
        Ok(StartResponse { registry_id })
    }

    async fn stop_service(
        &self,
        request: givc_common::pb::ServiceRequest,
    ) -> std::result::Result<ApplicationResponse, tonic::Status> {
        self.inner
            .stop_unit_on_vm(&request.service_name, &request.vm_name)
            .await?;
        app_success()
    }

    async fn pause_application(
        &self,
        request: ApplicationRequest,
    ) -> std::result::Result<ApplicationResponse, tonic::Status> {
        let agent = self.inner.agent_endpoint(&request.app_name)?;
        let client = SystemDClient::new(agent);
        for each in self.inner.app_entries(&request.app_name)? {
            let name = each.clone();
            let status = client.pause_remote(each).await?;
            if !status.is_paused() {
                bail!("Failed to pause {name}");
            }
        }
        app_success()
    }

    async fn resume_application(
        &self,
        request: ApplicationRequest,
    ) -> std::result::Result<ApplicationResponse, tonic::Status> {
        let agent = self.inner.agent_endpoint(&request.app_name)?;
        let client = SystemDClient::new(agent);
        for each in self.inner.app_entries(&request.app_name)? {
            let name = each.clone();
            let status = client.resume_remote(each).await?;
            if !status.is_running() {
                bail!("Failed to resume {name}");
            }
        }
        app_success()
    }

    async fn stop_application(
        &self,
        request: ApplicationRequest,
    ) -> std::result::Result<ApplicationResponse, tonic::Status> {
        let agent = self.inner.agent_endpoint(&request.app_name)?;
        let client = SystemDClient::new(agent);
        for each in self.inner.app_entries(&request.app_name)? {
            let name = each.clone();
            let status = client.stop_remote(each).await?;
            if !status.is_exitted() {
                bail!("Failed to stop {name}");
            }
        }
        app_success()
    }

    async fn poweroff(&self, _request: Empty) -> std::result::Result<Empty, tonic::Status> {
        self.inner
            .send_system_command(String::from("poweroff.target"))
            .await?;
        Ok(Empty {})
    }

    async fn reboot(&self, _request: Empty) -> std::result::Result<Empty, tonic::Status> {
        self.inner
            .send_system_command(String::from("reboot.target"))
            .await?;
        Ok(Empty {})
    }

    async fn suspend(&self, _request: Empty) -> std::result::Result<Empty, tonic::Status> {
        self.inner
            .send_system_command(String::from("suspend.target"))
            .await?;
        Ok(Empty {})
    }

    async fn wakeup(&self, _request: Empty) -> std::result::Result<Empty, tonic::Status> {
        println!("Not supported");
        Err(Status::unimplemented("Not supported"))
    }

    async fn sysinfo(
        &self,
        _request: Empty,
    ) -> std::result::Result<pb::stats::SysinfoResponse, tonic::Status> {
        Ok(self.inner.get_sysinfo().await?)
    }

    async fn query_list(&self, _request: Empty) -> Result<QueryListResponse, tonic::Status> {
        let list = self
            .inner
            .registry
            .contents()
            .into_iter()
            .map(QueryResult::from)
            .map(From::from)
            .collect();
        Ok(QueryListResponse { list })
    }

    async fn get_unit_status(
        &self,
        request: UnitStatusRequest,
    ) -> Result<pb::systemd::UnitStatus, tonic::Status> {
        let UnitStatusRequest { unit_name, vm_name } = request;
        let vm_name = VmName::Vm(&vm_name).agent_service();
        self.inner.get_unit_status(vm_name, unit_name).await
    }

    async fn set_locale(
        &self,
        request: LocaleRequest,
    ) -> std::result::Result<Empty, tonic::Status> {
        if request.assignments.is_empty() {
            return Err(invalid_request("No locale assignments provided"));
        }
        for pb::locale::LocaleAssignment { value, key } in &request.assignments {
            if value.is_empty() {
                return Err(invalid_request(format!("Empty locale value for {key}")));
            }
            if !Validator::validate_locale(value) {
                return Err(invalid_request(format!("Invalid locale value {value}")));
            }
        }

        let content = request
            .assignments
            .iter()
            .map(|a| {
                pb::locale::LocaleMacroKey::try_from(a.key)
                    .map_err(anyhow::Error::from)
                    .map(|key| format!("{}={}", key.as_str_name(), a.value))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join("\n");
        let _ = tokio::fs::write(LOCALE_CONF, content).await;

        let managers = self.inner.registry.filter_map(|re| {
            (re.r#type.service == ServiceType::Mgr)
                .then_some(())
                .and_then(|()| self.inner.endpoint(re).ok())
        });
        let assignments = request.assignments.clone();
        tokio::spawn(async move {
            let localemsg = pb::locale::LocaleMessage { assignments };
            for ec in managers {
                if let Ok(conn) = ec.connect().await {
                    let mut client =
                        pb::locale::locale_client_client::LocaleClientClient::new(conn);
                    let _ = client.locale_set(localemsg.clone()).await;
                }
            }
        });
        *self.inner.locale_assigns.lock().await = request.assignments;

        Ok(Empty {})
    }

    async fn set_timezone(
        &self,
        request: TimezoneRequest,
    ) -> std::result::Result<Empty, tonic::Status> {
        if !Validator::validate_timezone(&request.timezone) {
            bail!("Invalid timezone");
        }
        let _ = tokio::fs::write(TIMEZONE_CONF, &request.timezone).await;
        let managers = self.inner.registry.filter_map(|re| {
            (re.r#type.service == ServiceType::Mgr)
                .then_some(())
                .and_then(|()| self.inner.endpoint(re).ok())
        });
        let timezone = request.timezone.clone();
        tokio::spawn(async move {
            for ec in managers {
                if let Ok(conn) = ec.connect().await {
                    let mut client =
                        pb::locale::locale_client_client::LocaleClientClient::new(conn);
                    let tzmsg = pb::locale::TimezoneMessage {
                        timezone: timezone.clone(),
                    };
                    let _ = client.timezone_set(tzmsg).await;
                }
            }
        });
        *self.inner.timezone.lock().await = request.timezone;
        Ok(Empty {})
    }

    async fn get_stats(
        &self,
        request: pb::StatsRequest,
    ) -> std::result::Result<pb::stats::StatsResponse, tonic::Status> {
        let vm_name = VmName::Vm(&request.vm_name).agent_service();
        let vm = self
            .inner
            .registry
            .find_map(|re| {
                (re.r#type.service == ServiceType::Mgr && re.name == vm_name)
                    .then(|| self.inner.endpoint(re))
            })
            .with_context(|| format!("VM {vm_name} not found"))??;
        Ok(vm
            .connect()
            .await
            .map(pb::stats::stats_service_client::StatsServiceClient::new)?
            .get_stats(pb::stats::StatsRequest {})
            .await?
            .into_inner())
    }

    type WatchStream = Stream<WatchItem>;
    async fn watch(&self, _request: Empty) -> Result<Self::WatchStream, tonic::Status> {
        let (initial_list, mut chan) = self.inner.registry.subscribe();

        let stream = try_stream! {
            yield Event::into_initial(initial_list);

            loop {
                match chan.recv().await {
                    Ok(event) => {
                        yield event.into()
                    },
                    Err(e) => {
                        error!("Failed to receive subscription item from registry: {e}");
                        break;
                    },
                 }
             }
        };
        Ok(Box::pin(stream) as Self::WatchStream)
    }

    // User notification
    async fn notify_user(
        &self,
        request: pb::admin::UserNotificationRequest,
    ) -> Result<pb::notify::Status, tonic::Status> {
        let vm_name = VmName::Vm(&request.vm_name).agent_service();
        self.inner
            .notify_user(vm_name, request.notification.unwrap_or_default())
            .await
    }

    // OTA
    async fn list_generations(
        &self,
        _request: crate::pb::update::Empty,
    ) -> Result<ListGenerationsResponse, tonic::Status> {
        let endpoint = self.inner.host_endpoint()?;
        let ota = super::OTA::OTA::new(endpoint);
        let list = ota.list().await?;
        Ok(ListGenerationsResponse { list })
    }

    async fn discover(
        &self,
        request: RegistryDiscoverRequest,
    ) -> Result<RegistryDiscoverResponse, tonic::Status> {
        let endpoint = self.inner.net_vm_endpoint()?;
        let ota = super::OTA::OTA::new(endpoint);
        Ok(RegistryDiscoverResponse {
            list: ota.discover(request).await?,
        })
    }

    async fn changelog(
        &self,
        request: RegistryChangelogRequest,
    ) -> Result<RegistryChangelogResponse, tonic::Status> {
        let endpoint = self.inner.net_vm_endpoint()?;
        let ota = super::OTA::OTA::new(endpoint);
        Ok(RegistryChangelogResponse {
            changelog: ota.changelog(request).await?,
        })
    }

    type PullStream = super::OTA::PullStream;
    async fn pull(&self, request: RegistryPullRequest) -> Result<Self::PullStream, tonic::Status> {
        let endpoint = self.inner.net_vm_endpoint()?;
        let ota = super::OTA::OTA::new(endpoint);
        ota.pull(request).await
    }

    type ImageInstallStream = super::OTA::ImageInstallStream;
    async fn image_install(
        &self,
        request: ImageInstallRequest,
    ) -> Result<Self::ImageInstallStream, tonic::Status> {
        let endpoint = self.inner.host_endpoint()?;
        let ota = super::OTA::OTA::new(endpoint);
        ota.image_install(request).await
    }

    type SetGenerationStream = super::OTA::SetGenerationStream;
    async fn set_generation(
        &self,
        request: SetGenerationRequest,
    ) -> Result<Self::SetGenerationStream, tonic::Status> {
        let endpoint = self.inner.host_endpoint()?;
        let ota = super::OTA::OTA::new(endpoint);
        match request.update {
            Some(crate::pb::update::set_generation_request::Update::Cachix(cachix_request)) => {
                let stream = ota.install_via_cachix(cachix_request).await?;
                Ok::<_, anyhow::Error>(Box::pin(stream) as Self::SetGenerationStream)
            }
            _ => anyhow::bail!("unimplemented update method"),
        }
    }

    async fn ctap(&self, request: CtapRequest) -> Result<CtapResponse, tonic::Status> {
        let gui_vm_mgr = self
            .inner
            .agent_endpoint("givc-gui-vm.service")
            .context("VM not found")?;
        let mut client = pb::ctap::ctap_client::CtapClient::new(gui_vm_mgr.connect().await?);

        Ok(client.ctap(request).await?.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::admin_service_server::AdminService as AdminServiceTrait;

    #[test]
    fn test_locale_validator() -> anyhow::Result<()> {
        if ![
            "en_US.UTF-8",
            "C",
            "POSIX",
            "C.UTF-8",
            "ar_AE.UTF-8",
            "fi_FI@euro.UTF-8",
            "fi_FI@euro",
        ]
        .into_iter()
        .all(Validator::validate_locale)
        {
            bail!("Valid locale rejected");
        }
        if ["`rm -Rf --no-preserve-root /`", "; whoami", "iwaenfli"]
            .into_iter()
            .any(Validator::validate_locale)
        {
            bail!("Invalid locale accepted");
        }
        Ok(())
    }

    #[test]
    fn test_timezone_validator() -> anyhow::Result<()> {
        if ![
            "UTC",
            "Europe/Helsinki",
            "Asia/Abu_Dhabi",
            "Etc/GMT+8",
            "GMT-0",
            "America/Argentina/Rio_Gallegos",
        ]
        .into_iter()
        .all(Validator::validate_timezone)
        {
            bail!("Valid timezone rejected");
        }
        if ["/foobar", "`whoami`", "Almost//Valid"]
            .into_iter()
            .any(Validator::validate_timezone)
        {
            bail!("Invalid timezone accepted");
        }
        Ok(())
    }

    #[tokio::test]
    async fn grpc_macro_returns_safe_admin_error() {
        let service = AdminService::new(None, false, false, None, None)
            .expect("admin service initializes without optional policy admin");
        let error = service
            .set_locale(tonic::Request::new(LocaleRequest {
                assignments: Vec::new(),
            }))
            .await
            .expect_err("empty locale assignments are rejected");

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert_eq!(error.message(), "request contains invalid arguments");
        assert!(error.details().is_empty());
    }
}
