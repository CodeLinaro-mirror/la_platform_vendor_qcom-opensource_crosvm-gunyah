/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

//! A module meant to abstract the process of starting the VM and connecting
//! to the payload

#![allow(non_snake_case)]
use crate::timeouts::TIMEOUTS;
use crate::*;
use anyhow::{anyhow, bail, Context, Result};
// use log::error;
use std::fmt::Debug;
// use std::fs::OpenOptions;
use binder::{ParcelFileDescriptor, Strong};
use binder::{Result as BinderResult, Status};
use glob::glob;
use log::{info, warn};
use rustutils::android::system_properties;
use std::fs::File;
use std::path::{Path, PathBuf};
use vmclient::{DeathReason, ErrorCode, VmInstance, VmWaitError};

use vendor_qcom_microdroid_test::aidl::vendor::qti::microdroid_test::ITestPayload::{
    ITestPayload, SERVICE_PORT,
};

///Converts results to Binder results that implement Binder Status
pub fn to_binder_result<T, E: Debug>(result: Result<T, E>) -> BinderResult<T> {
    result.map_err(|e| {
        let message = format!("{:?}", e);
        warn!("Returning binder error: {}", &message);
        Status::new_service_specific_error_str(-1, Some(message))
    })
}


/// This struct represents the Client owning the VmInstance.
pub struct Client(pub VmInstance);

/// CPU topology configuration for a virtual machine.
#[derive(Default, Debug, Clone)]
pub enum VmCpuTopology {
    /// Run VM with 1 vCPU only.
    #[default]
    OneCpu,
    /// Run VM vCPU topology matching that of the host.
    MatchHost,
}

/// Parameters to be used when creating a virtual machine instance.
#[derive(Default, Debug, Clone)]
pub struct VmParameters {
    /// The name of VM for identifying.
    pub name: String,
    /// Whether the VM should be debuggable.
    pub debug_mode: bool,
    /// CPU topology of the VM. Defaults to 1 vCPU.
    pub cpu_topology: VmCpuTopology,
    /// List of task profiles to apply to the VM
    pub task_profiles: Vec<String>,
    /// If present, overrides the amount of RAM to give the VM
    pub memory_mib: Option<i32>,
    /// Whether the VM prefers staged APEXes or activated ones (false; default)
    pub prefer_staged: bool,
}

impl Client {

    /// Start a new TestPayload VM instance using the specified instance image file and parameters.
    pub fn start(
        service: &dyn IVirtualizationService,
        instance_image: File,
        idsig: &Path,
        apk: &str,
        config_path: &str,
    ) -> Result<Self> {
        let protected_vm = want_protected_vm()?;

        let instance_fd = ParcelFileDescriptor::new(instance_image);

        let apk_dir = Path::new("/data/local/tmp/");

        let config_apk = locate_config_apk(apk_dir, apk)?;
        let apk_fd = File::open(config_apk).context("Failed to open config APK file")?;
        let apk_fd = ParcelFileDescriptor::new(apk_fd);
        let idsig_fd = prepare_idsig(service, &apk_fd, idsig)?;
        let config_path = config_path.to_owned();
        println!("Config path: {}", config_path);
        let cpu_topology = VmCpuTopology::MatchHost;
        let task_profiles = vec!["SCHED_SP_COMPUTE".to_string()];
        // let memory_mib = Some(compos_memory_mib()?);
        let mut parameters = VmParameters { cpu_topology, task_profiles, debug_mode: true, ..Default::default() };
        parameters.name = "Test_Payload_VM".to_owned();
        let debug_level = if parameters.debug_mode { DebugLevel::FULL } else { DebugLevel::NONE };

        // let cpu_topology = CpuTopology::MATCH_HOST;
        parameters.memory_mib = Some(256);

        //Make a AppConfig

        let config = VirtualMachineConfig::AppConfig(VirtualMachineAppConfig {
            name: parameters.name.clone(),
            apk: Some(apk_fd),
            idsig: Some(idsig_fd),
            instanceImage: Some(instance_fd),
            encryptedStorageImage: None,
            payload: Payload::PayloadConfig(VirtualMachinePayloadConfig{
                payloadBinaryName: config_path,
                ..Default::default()
                }),
            debugLevel: debug_level,
            extraIdsigs: Default::default(),
            protectedVm: protected_vm,
            memoryMib: parameters.memory_mib.unwrap_or(0), // 0 means use the default
            cpuOptions: Default::default(),
            customConfig: Default::default(),
            ..Default::default()
        });


        // Let logs go to logcat.
        let (console_out_fd, console_in_fd, log_fd) = (None, None, None);
        // let callback = Box::new(Callback {});
        let dump_dt = None;
        let instance = VmInstance::create(service, &config, console_out_fd, console_in_fd, log_fd, dump_dt)
            .context("Failed to create VM")?;
        let callback = Box::new(Callback {});
        instance.start(Some(callback)).context("Failed to start VM")?;

        let ready = instance.wait_until_ready(TIMEOUTS.vm_max_time_to_ready);
        if ready == Err(VmWaitError::Finished) && debug_level != DebugLevel::NONE {
            // The payload has (unexpectedly) finished, but the VM is still running. Give it
            // some time to shutdown to maximize our chances of getting useful logs.
            if let Some(death_reason) =
                instance.wait_for_death_with_timeout(TIMEOUTS.vm_max_time_to_exit)
            {
                bail!("VM died during startup - reason {:?}", death_reason);
            }
        }

        ready?;

        Ok(Self(instance))
    }

    /// Create and return an RPC Binder connection to the Comp OS service in the VM.
    pub fn connect_test_payload_service(&self) -> Result<Strong<dyn ITestPayload>> {
        self.0.connect_service(SERVICE_PORT.try_into().unwrap()).context("Connecting to TestPayload service")
    }

    /// Shutting down the sanity VM
    pub fn shutdown_server(&self, service: Strong<dyn ITestPayload>) {
        info!("Requesting TestPayload VM to shutdown");

        let _ = service.quit(); // If this fails, the VM is probably dying anyway
        info!("Finished Server VM shutdown");
        self.wait_for_shutdown();
    }

    /// Wait for the instance to shut down. If it fails to shutdown within a reasonable time the
    /// instance is dropped, which forcibly terminates it.
    /// This should only be called when the instance has been requested to quit, or we believe that
    /// it is already in the process of exiting due to some failure.
    fn wait_for_shutdown(&self) {
        let death_reason = self.0.wait_for_death_with_timeout(TIMEOUTS.vm_max_time_to_exit);
        match death_reason {
            Some(DeathReason::Shutdown) => info!("VM has exited normally"),
            Some(reason) => warn!("VM died with reason {:?}", reason),
            None => warn!("VM failed to exit, dropping"),
        }
    }
}

fn locate_config_apk(apk_dir: &Path, apk: &str) -> Result<PathBuf> {
    // Our config APK will be in a directory under app, but the name of the directory is at the
    // discretion of the build system. So just look in each sub-directory until we find it.
    // (In practice there will be exactly one directory, so this shouldn't take long.)
    let mut apk_name = apk.to_owned();
    apk_name.push_str("*.apk");
    let apk_path = Path::new(&apk_name);
    // println!("APK path {}", apk_path.to_str().unwrap_or("no path made"));
    let app_glob = apk_dir.join("app").join("**").join(apk_path);
    let mut entries: Vec<PathBuf> =
        glob(app_glob.to_str().ok_or_else(|| anyhow!("Invalid path: {}", app_glob.display()))?)
            .context("failed to glob")?
            .filter_map(|e| e.ok())
            .collect();
    if entries.len() > 1 {
        bail!("Found more than one apk matching {}", app_glob.display());
    }
    // let mut path = PathBuf::new();
    match entries.pop() {
        Some(path) => {
            info!("Entry found {}", path.clone().into_os_string().into_string().unwrap());
            Ok(path)
        },
        None => Err(anyhow!("No apks match {}", app_glob.display())),
    }
}

fn prepare_idsig(
    service: &dyn IVirtualizationService,
    apk_fd: &ParcelFileDescriptor,
    idsig_path: &Path,
) -> Result<ParcelFileDescriptor> {
    if !idsig_path.exists() {
        // Prepare idsig file via VirtualizationService
        let idsig_file = File::create(idsig_path).context("Failed to create idsig file")?;
        let idsig_fd = ParcelFileDescriptor::new(idsig_file);
        service
            .createOrUpdateIdsigFile(apk_fd, &idsig_fd)
            .context("Failed to update idsig file")?;
    }

    // Open idsig as read-only
    let idsig_file = File::open(idsig_path).context("Failed to open idsig file")?;
    let idsig_fd = ParcelFileDescriptor::new(idsig_file);
    Ok(idsig_fd)
}

fn want_protected_vm() -> Result<bool> {
    let have_protected_vm =
        system_properties::read_bool("ro.boot.hypervisor.protected_vm.supported", false)?;
    if have_protected_vm {
        info!("Starting protected VM");
        return Ok(true);
    }

    let is_debug_build = system_properties::read("ro.debuggable")?.as_deref().unwrap_or("0") == "1";
    if !is_debug_build {
        bail!("Protected VM not supported, unable to start VM");
    }

    let have_non_protected_vm =
        system_properties::read_bool("ro.boot.hypervisor.vm.supported", false)?;
    if have_non_protected_vm {
        warn!("Protected VM not supported, falling back to non-protected on debuggable build");
        return Ok(false);
    }

    bail!("No VM support available")
}



struct Callback {}
impl vmclient::VmCallback for Callback {
    fn on_payload_started(&self, cid: i32) {
        log::info!("VM payload started, cid = {}", cid);
    }

    fn on_payload_ready(&self, cid: i32) {
        log::info!("VM payload ready, cid = {}", cid);
    }

    fn on_payload_finished(&self, cid: i32, exit_code: i32) {
        log::warn!("VM payload finished, cid = {}, exit code = {}", cid, exit_code);
    }

    fn on_error(&self, cid: i32, error_code: ErrorCode, message: &str) {
        log::warn!("VM error, cid = {}, error code = {:?}, message = {}", cid, error_code, message);
    }

    fn on_died(&self, cid: i32, death_reason: DeathReason) {
        log::warn!("VM died, cid = {}, reason = {:?}", cid, death_reason);
    }
}
