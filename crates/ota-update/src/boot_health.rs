// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

use crate::bootctl::BootctlItem;
use crate::image::uki::{BootEntry, UkiEntry};
use crate::lock::UpdateLock;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LOCK_PATH: &str = "/run/ota-update.lock";
const BLESS_BOOT_PATH: &str = "/run/current-system/sw/lib/systemd/systemd-bless-boot";

#[derive(Clone, Debug)]
pub struct BootHealthConfig {
    pub luks_mapper: String,
    pub verity_mapper: String,
    pub mountpoints: Vec<PathBuf>,
    pub services: Vec<String>,
    pub accepted_generation_file: PathBuf,
}

#[async_trait]
trait CommandRunner: Send + Sync {
    async fn output(&self, program: &str, args: &[OsString]) -> Result<Vec<u8>>;

    async fn run(&self, program: &str, args: &[OsString]) -> Result<()>;
}

struct ProcessRunner;

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn output(&self, program: &str, args: &[OsString]) -> Result<Vec<u8>> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .with_context(|| format!("executing {program}"))?;
        ensure!(
            output.status.success(),
            "{program} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }

    async fn run(&self, program: &str, args: &[OsString]) -> Result<()> {
        self.output(program, args).await?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectedBoot {
    pub(crate) id: String,
    pub(crate) uki: UkiEntry,
}

impl SelectedBoot {
    fn from_boot_entry(entry: &BootEntry) -> Option<Self> {
        Some(Self {
            id: entry.uki()?.boot_id(),
            uki: entry.uki()?.clone(),
        })
    }

    pub(crate) fn trial_remaining(&self) -> Option<u32> {
        self.uki
            .boot_counter
            .as_ref()
            .map(|counter| counter.remaining)
    }
}

pub(crate) fn select_current_boot(items: Vec<BootctlItem>) -> Result<SelectedBoot> {
    let selected: Vec<_> = BootEntry::from_bootctl(items)
        .filter(|entry| entry.is_managed() && entry.is_selected)
        .collect();

    ensure!(
        selected.len() == 1,
        "expected exactly one selected managed Ghaf UKI, found {}",
        selected.len()
    );

    SelectedBoot::from_boot_entry(&selected[0]).context("selected entry is not a managed UKI")
}

pub(crate) fn validate_generation(value: &str) -> Result<u64> {
    let value = value.trim();
    ensure!(!value.is_empty(), "generation is empty");
    ensure!(
        value.chars().all(|character| character.is_ascii_digit()),
        "generation is not a positive decimal integer"
    );
    let generation = value.parse::<u64>().context("parsing generation")?;
    ensure!(generation > 0, "generation must be positive");
    Ok(generation)
}

pub(crate) fn read_generation(path: &Path) -> Result<u64> {
    let value = fs::read_to_string(path)
        .with_context(|| format!("reading accepted generation from {}", path.display()))?;
    validate_generation(&value)
        .with_context(|| format!("invalid accepted generation in {}", path.display()))
}

pub(crate) fn write_generation_atomically(path: &Path, generation: u64) -> Result<()> {
    ensure!(generation > 0, "generation must be positive");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("accepted generation path has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "creating accepted generation directory {}",
            parent.display()
        )
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting permissions on {}", parent.display()))?;

    let name = path
        .file_name()
        .context("accepted generation path has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.tmp"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("opening {}", temporary.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", temporary.display()))?;
    writeln!(file, "{generation}").with_context(|| format!("writing {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, path).with_context(|| {
        format!(
            "renaming accepted generation {} to {}",
            temporary.display(),
            path.display()
        )
    })?;
    let directory = OpenOptions::new().read(true).open(parent)?;
    directory
        .sync_all()
        .with_context(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

async fn evaluate_health_with(runner: &dyn CommandRunner, config: &BootHealthConfig) -> Result<()> {
    runner
        .run(
            "cryptsetup",
            &[
                OsString::from("status"),
                OsString::from(&config.luks_mapper),
            ],
        )
        .await
        .context("LUKS health check")?;
    runner
        .run(
            "veritysetup",
            &[
                OsString::from("status"),
                OsString::from(&config.verity_mapper),
            ],
        )
        .await
        .context("verity health check")?;
    for mountpoint in &config.mountpoints {
        runner
            .run(
                "findmnt",
                &[
                    OsString::from("--mountpoint"),
                    mountpoint.as_os_str().to_os_string(),
                ],
            )
            .await
            .with_context(|| format!("mount health check for {}", mountpoint.display()))?;
    }
    for service in &config.services {
        runner
            .run(
                "systemctl",
                &[
                    OsString::from("is-active"),
                    OsString::from("--quiet"),
                    OsString::from(service),
                ],
            )
            .await
            .with_context(|| format!("service health check for {service}"))?;
    }
    Ok(())
}

async fn run_with(
    runner: &dyn CommandRunner,
    config: &BootHealthConfig,
    cmdline: &str,
    bootctl_json: &[u8],
    dry_run: bool,
) -> Result<()> {
    let generation = parse_running_generation(cmdline)?;
    let bootctl: Vec<BootctlItem> =
        serde_json::from_slice(bootctl_json).context("parsing bootctl JSON")?;
    let current = select_current_boot(bootctl)?;
    let accepted = read_generation(&config.accepted_generation_file)?;

    let health_error = evaluate_health_with(runner, config).await.err();
    if let Some(error) = health_error {
        if dry_run {
            return Err(error.context("boot health failed during dry-run"));
        }
        return retry_or_fail(runner, &current, error).await;
    }

    if generation <= accepted {
        return Ok(());
    }

    if dry_run {
        println!(
            "DRY-RUN: bless {} and set it as default, then accept generation {generation}",
            current.id
        );
        return Ok(());
    }

    if current.trial_remaining().is_some() {
        runner
            .run(BLESS_BOOT_PATH, &[OsString::from("good")])
            .await
            .context("blessing healthy trial boot")?;
    }
    runner
        .run(
            "bootctl",
            &[OsString::from("set-default"), OsString::from(&current.id)],
        )
        .await
        .context("promoting healthy boot entry")?;
    write_generation_atomically(&config.accepted_generation_file, generation)?;
    Ok(())
}

async fn retry_or_fail(
    runner: &dyn CommandRunner,
    current: &SelectedBoot,
    error: anyhow::Error,
) -> Result<()> {
    let Some(remaining) = current.trial_remaining() else {
        return Err(error);
    };
    if remaining > 0 {
        runner
            .run(
                "bootctl",
                &[OsString::from("set-oneshot"), OsString::from(&current.id)],
            )
            .await
            .context("re-arming failed trial boot")?;
        runner
            .run(
                "systemctl",
                &[
                    OsString::from("reboot"),
                    OsString::from("--message='Ghaf A/B trial health check failed'"),
                ],
            )
            .await
            .context("rebooting after failed trial boot")?;
    }
    Err(error.context("boot health failed"))
}

fn parse_running_generation(cmdline: &str) -> Result<u64> {
    let value = cmdline
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("ghaf.generation="))
        .context("missing ghaf.generation in kernel command line")?;
    validate_generation(value).context("invalid ghaf.generation in kernel command line")
}

pub async fn run(config: BootHealthConfig, dry_run: bool) -> Result<()> {
    let _lock = UpdateLock::acquire(LOCK_PATH, "boot-health")?;
    let cmdline = tokio::fs::read_to_string("/proc/cmdline")
        .await
        .context("reading /proc/cmdline")?;
    let bootctl_json = ProcessRunner
        .output(
            "bootctl",
            &[OsString::from("list"), OsString::from("--json=short")],
        )
        .await
        .context("reading bootctl state")?;
    run_with(&ProcessRunner, &config, &cmdline, &bootctl_json, dry_run).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootctl::parse_bootctl;
    use anyhow::anyhow;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    type CommandCall = (String, Vec<OsString>);

    #[derive(Default)]
    struct MockRunner {
        calls: Arc<Mutex<Vec<CommandCall>>>,
        failure: Option<String>,
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn output(&self, program: &str, args: &[OsString]) -> Result<Vec<u8>> {
            self.run(program, args).await?;
            Ok(Vec::new())
        }

        async fn run(&self, program: &str, args: &[OsString]) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            if self.failure.as_deref() == Some(program) {
                return Err(anyhow!("mock failure for {program}"));
            }
            Ok(())
        }
    }

    fn item(id: &str, path: &str, selected: bool) -> BootctlItem {
        BootctlItem {
            r#type: "type2".into(),
            source: "esp".into(),
            id: id.into(),
            path: PathBuf::from(path),
            root: PathBuf::from("/boot"),
            title: "Ghaf".into(),
            show_title: "Ghaf".into(),
            sort_key: "ghaf".into(),
            version: "test".into(),
            machine_id: None,
            options: String::new(),
            linux: None,
            efi: None,
            initrd: None,
            is_reported: true,
            is_default: false,
            is_selected: selected,
            addons: None,
            cmdline: String::new(),
        }
    }

    #[test]
    fn selects_one_managed_uki() {
        let boot = select_current_boot(vec![item(
            "ghaf-1.2.3-deadbeef.efi",
            "/boot/EFI/Linux/ghaf-1.2.3-deadbeef+3-1.efi",
            true,
        )])
        .unwrap();

        assert_eq!(boot.id, "ghaf-1.2.3-deadbeef.efi");
        assert_eq!(boot.trial_remaining(), Some(3));
    }

    #[test]
    fn selected_exhausted_trial_is_preserved() {
        let boot = select_current_boot(vec![item(
            "ghaf-1.2.3-deadbeef.efi",
            "/boot/EFI/Linux/ghaf-1.2.3-deadbeef+0-3.efi",
            true,
        )])
        .unwrap();

        assert_eq!(boot.trial_remaining(), Some(0));
    }

    #[test]
    fn rejects_missing_selected_entry() {
        let error = select_current_boot(vec![item(
            "ghaf-1.2.3-deadbeef.efi",
            "/boot/EFI/Linux/ghaf-1.2.3-deadbeef.efi",
            false,
        )])
        .unwrap_err();

        assert!(error.to_string().contains("exactly one selected"));
    }

    #[test]
    fn rejects_ambiguous_selected_entries() {
        let error = select_current_boot(vec![
            item(
                "ghaf-1.2.3-deadbeef.efi",
                "/boot/EFI/Linux/ghaf-1.2.3-deadbeef.efi",
                true,
            ),
            item(
                "ghaf-2.0.0-cafebabe.efi",
                "/boot/EFI/Linux/ghaf-2.0.0-cafebabe.efi",
                true,
            ),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("exactly one selected"));
    }

    #[test]
    fn malformed_counter_fails_closed() {
        let malformed = r#"
        [{
          "type":"type2", "source":"esp", "id":"ghaf-1.2.3-deadbeef.efi",
          "path":"/boot/EFI/Linux/ghaf-1.2.3-deadbeef+bad.efi", "root":"/boot",
          "title":"Ghaf", "showTitle":"Ghaf", "sortKey":"ghaf", "version":"test",
          "options":"", "isSelected":true, "cmdline":""
        }]
        "#;
        let items = parse_bootctl(malformed).unwrap();
        let error = select_current_boot(items).unwrap_err();

        assert!(error.to_string().contains("exactly one selected"));
    }

    #[test]
    fn validates_positive_generation() {
        assert_eq!(validate_generation("7\n").unwrap(), 7);
        assert!(validate_generation("0").is_err());
        assert!(validate_generation("not-a-number").is_err());
        assert!(validate_generation("1 2").is_err());
    }

    #[test]
    fn reads_and_writes_generation_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/accepted-generation");

        write_generation_atomically(&path, 7).unwrap();

        assert_eq!(read_generation(&path).unwrap(), 7);
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn missing_and_invalid_generation_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("accepted-generation");

        assert!(read_generation(&path).is_err());
        fs::write(&path, "not-a-number\n").unwrap();
        assert!(read_generation(&path).is_err());
        fs::write(&path, "0\n").unwrap();
        assert!(read_generation(&path).is_err());
    }

    #[tokio::test]
    async fn health_checks_use_fixed_commands() {
        let runner = MockRunner::default();
        let config = BootHealthConfig {
            luks_mapper: "cryptroot".into(),
            verity_mapper: "nix-store".into(),
            mountpoints: vec!["/nix/store".into(), "/persist".into()],
            services: vec!["microvm@admin.service".into()],
            accepted_generation_file: "/persist/common/ota/accepted-generation".into(),
        };

        evaluate_health_with(&runner, &config).await.unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0].0, "cryptsetup");
        assert_eq!(calls[1].0, "veritysetup");
        assert_eq!(calls[2].0, "findmnt");
        assert_eq!(calls[4].0, "systemctl");
        assert_eq!(calls[4].1[1], OsString::from("--quiet"));
    }

    #[tokio::test]
    async fn failed_health_check_is_returned() {
        let runner = MockRunner {
            failure: Some("veritysetup".into()),
            ..MockRunner::default()
        };
        let config = BootHealthConfig {
            luks_mapper: "cryptroot".into(),
            verity_mapper: "nix-store".into(),
            mountpoints: vec![],
            services: vec![],
            accepted_generation_file: "/tmp/accepted-generation".into(),
        };

        assert!(evaluate_health_with(&runner, &config).await.is_err());
    }

    fn boot_json(filename: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([{
            "type": "type2",
            "source": "esp",
            "id": "ghaf-2.0.0-deadbeef.efi",
            "path": format!("/boot/EFI/Linux/{filename}"),
            "root": "/boot",
            "title": "Ghaf",
            "showTitle": "Ghaf",
            "sortKey": "ghaf",
            "version": "2.0.0",
            "options": "",
            "isSelected": true,
            "isDefault": false,
            "cmdline": ""
        }]))
        .unwrap()
    }

    fn state_config(path: &Path) -> BootHealthConfig {
        BootHealthConfig {
            luks_mapper: "cryptroot".into(),
            verity_mapper: "nix-store".into(),
            mountpoints: vec![],
            services: vec![],
            accepted_generation_file: path.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn healthy_trial_is_blessed_promoted_and_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let accepted = directory.path().join("accepted-generation");
        write_generation_atomically(&accepted, 1).unwrap();
        let runner = MockRunner::default();

        run_with(
            &runner,
            &state_config(&accepted),
            "ghaf.generation=2",
            &boot_json("ghaf-2.0.0-deadbeef+3-1.efi"),
            false,
        )
        .await
        .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert!(calls.iter().any(|(program, args)| {
            program == BLESS_BOOT_PATH && args == &[OsString::from("good")]
        }));
        assert!(calls.iter().any(|(program, args)| {
            program == "bootctl"
                && args
                    == &[
                        OsString::from("set-default"),
                        OsString::from("ghaf-2.0.0-deadbeef.efi"),
                    ]
        }));
        assert_eq!(read_generation(&accepted).unwrap(), 2);
    }

    #[tokio::test]
    async fn unhealthy_trial_is_rearmed_and_rebooted() {
        let directory = tempfile::tempdir().unwrap();
        let accepted = directory.path().join("accepted-generation");
        write_generation_atomically(&accepted, 1).unwrap();
        let runner = MockRunner {
            failure: Some("systemctl".into()),
            ..MockRunner::default()
        };

        assert!(
            run_with(
                &runner,
                &BootHealthConfig {
                    services: vec!["microvm@broken.service".into()],
                    ..state_config(&accepted)
                },
                "ghaf.generation=2",
                &boot_json("ghaf-2.0.0-deadbeef+2-2.efi"),
                false,
            )
            .await
            .is_err()
        );
        let calls = runner.calls.lock().unwrap();
        assert!(calls.iter().any(|(program, args)| {
            program == "bootctl"
                && args
                    == &[
                        OsString::from("set-oneshot"),
                        OsString::from("ghaf-2.0.0-deadbeef.efi"),
                    ]
        }));
        assert!(calls.iter().any(|(program, args)| {
            program == "systemctl" && args.first() == Some(&OsString::from("reboot"))
        }));
        assert_eq!(read_generation(&accepted).unwrap(), 1);
    }

    #[tokio::test]
    async fn exhausted_trial_is_not_rearmed() {
        let directory = tempfile::tempdir().unwrap();
        let accepted = directory.path().join("accepted-generation");
        write_generation_atomically(&accepted, 1).unwrap();
        let runner = MockRunner {
            failure: Some("systemctl".into()),
            ..MockRunner::default()
        };

        assert!(
            run_with(
                &runner,
                &BootHealthConfig {
                    services: vec!["microvm@broken.service".into()],
                    ..state_config(&accepted)
                },
                "ghaf.generation=2",
                &boot_json("ghaf-2.0.0-deadbeef+0-3.efi"),
                false,
            )
            .await
            .is_err()
        );
        let calls = runner.calls.lock().unwrap();
        assert!(!calls.iter().any(|(program, args)| {
            program == "bootctl" && args.first() == Some(&OsString::from("set-oneshot"))
        }));
        assert_eq!(read_generation(&accepted).unwrap(), 1);
    }

    #[tokio::test]
    async fn healthy_fallback_does_not_promote_or_advance_state() {
        let directory = tempfile::tempdir().unwrap();
        let accepted = directory.path().join("accepted-generation");
        write_generation_atomically(&accepted, 2).unwrap();
        let runner = MockRunner::default();

        run_with(
            &runner,
            &state_config(&accepted),
            "ghaf.generation=1",
            &boot_json("ghaf-1.0.0-deadbeef.efi"),
            false,
        )
        .await
        .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert!(!calls.iter().any(|(program, args)| {
            program == "bootctl" && args.first() == Some(&OsString::from("set-default"))
        }));
        assert_eq!(read_generation(&accepted).unwrap(), 2);
    }

    #[test]
    fn parses_running_generation() {
        assert_eq!(
            parse_running_generation("quiet ghaf.generation=7").unwrap(),
            7
        );
        assert!(parse_running_generation("quiet").is_err());
        assert!(parse_running_generation("ghaf.generation=0").is_err());
    }
}
