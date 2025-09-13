/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
*/

use std::{
    collections::HashMap,
    error::Error,
    ffi::{CStr, CString},
    str::FromStr,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use libc::_exit;

use serde::Deserialize;

use nix::{
    sys::signal::kill, sys::signal::Signal, sys::wait::waitpid,
    sys::wait::WaitPidFlag, sys::wait::WaitStatus, unistd::execv, unistd::fork,
    unistd::ForkResult, unistd::Pid,
};

use rustutils::system_properties;

use log::{debug, error, info};

use vendor_qti_qvirt::aidl::vendor::qti::qvirt::{
    IVirtualMachine::{BnVirtualMachine, IVirtualMachine, ERROR_VM_START},
    IVirtualMachineCallback::IVirtualMachineCallback,
    VirtualMachineState::VirtualMachineState,
};
use vendor_qti_qvirt::binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, Status,
    Strong, ThreadState,
};

// ======================================================================
// HELPERS
// ======================================================================

static VM_BINARY_FILE: &str = "/system_ext/bin/qcrosvm";
static DEFAULT_BOOT_COMPLETE_TIMEOUT: u16 = 60;

fn boot_complete_timeout_default() -> u16 {
    DEFAULT_BOOT_COMPLETE_TIMEOUT
}

#[derive(Default, Debug, PartialEq, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlState {
    #[default]
    Create = 0,
    Start = 1,
    Stop = 2,
    Restart = 3,
    Panic = 4,
    NotSupported = 5,
}

impl FromStr for ControlState {
    type Err = ();
    fn from_str(input: &str) -> Result<ControlState, Self::Err> {
        match input {
            "create" => Ok(ControlState::Create),
            "start" => Ok(ControlState::Start),
            "stop" => Ok(ControlState::Stop),
            "restart" => Ok(ControlState::Restart),
            "panic" => Ok(ControlState::Panic),
            _ => Err(()),
        }
    }
}

#[derive(Default, Debug, Clone, Deserialize)]
#[serde(rename = "disk")]
pub struct DiskProperties {
    pub image: String,
    pub label: u32,
    pub read_write: bool,
}

#[derive(Default, Debug, Clone, Deserialize)]
pub struct VmParameters {
    pub name: String,
    pub enable: bool,
    #[serde(skip)]
    pub legacy: bool,
    #[serde(rename = "boot_ops")]
    pub boot_operation: ControlState,
    #[serde(default)]
    pub disk: Vec<DiskProperties>,
    #[serde(default)]
    pub try_count: u8,
    pub boot_wait_time: u8,
    #[serde(default = "boot_complete_timeout_default")]
    pub boot_complete_timeout: u16,
    pub no_fs_dependency: bool,
    pub autostart: bool,
    #[serde(default)]
    pub on_demand_start_supported: bool,
}

#[derive(Default)]
pub struct VmInstance {
    pub vm_state: VirtualMachineState,
    pub vm_parameters: VmParameters,

    pub autostart_done: bool,

    // Callbacks for the clients of this VM
    pub virtual_machine_callbacks: Vec<Strong<(dyn IVirtualMachineCallback)>>,
    // Death Recipient Objects for the clients (kept here to stay alive)
    // Upon client death, remove their callback.
    pub death_id: AtomicUsize,
    pub death_recipients: HashMap<usize, DeathRecipient>,
}
impl VmInstance {
    pub fn new(vm_parameters: VmParameters) -> Self {
        let inst = Self {
            vm_state: VirtualMachineState::NOT_STARTED,
            vm_parameters: vm_parameters,
            autostart_done: false,
            virtual_machine_callbacks: Vec::new(),
            death_id: AtomicUsize::new(0),
            death_recipients: HashMap::new(),
        };
        inst.set_vm_status_property("NOT_STARTED");
        return inst;
    }

    pub fn notify_clients(&mut self, event: &str) -> () {
        if event == "create" {
            info!(
                "Event=create received for {}, state change to RUNNING",
                self.vm_parameters.name
            );
            self.vm_state = VirtualMachineState::RUNNING;
            self.set_vm_status_property("RUNNING");
        } else if event == "destroy" {
            info!(
                "Event=destroy received for {}, state change to STOPPED",
                self.vm_parameters.name
            );
            self.vm_state = VirtualMachineState::STOPPED;
            self.set_vm_status_property("STOPPED");
        }

        // notify_clients
        if self.virtual_machine_callbacks.is_empty() {
            info!(
                "No clients registered for {} callback yet.",
                self.vm_parameters.name
            );
            return;
        }
        info!(
            "Notifying {} clients of {}",
            self.virtual_machine_callbacks.len(),
            self.vm_parameters.name
        );

        for callback in &self.virtual_machine_callbacks {
            match callback.onStatusChanged(self.vm_state) {
                Ok(_) => {
                    debug!("notify_clients: {}-CallbackObject[ClientId: {:?}]->onStatusChanged({:?}): success",
                        self.vm_parameters.name, callback.as_binder(), self.vm_state);
                }
                _ => {
                    debug!("notify_clients: {}-CallbackObject[ClientId]->onStatusChanged({:?}): success",
                        self.vm_parameters.name, self.vm_state);
                }
            }
        }
    }

    fn wait_for_exit(
        pid: &i32,
        force_exit: Option<bool>,
    ) -> Result<(), Box<dyn Error>> {
        // Send kill if forcing exit
        if force_exit.unwrap_or(false)
            && (waitpid(
                Pid::from_raw(*pid),
                Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED),
            ) == Ok(WaitStatus::StillAlive))
        {
            info!("Sending 'SIGKILL' signal to PID={pid}");
            kill(Pid::from_raw(*pid), Signal::SIGKILL)?;
        }

        if let Ok(status) = waitpid(
            Pid::from_raw(*pid),
            Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED),
        ) {
            match status {
                WaitStatus::Exited(pid, s) => {
                    if s > 0 {
                        error!("PID:{pid} exit not success (0), result {s}");
                        return Err("Result {s}".into());
                    }
                }
                WaitStatus::Signaled(pid, sig, _) => {
                    info!("PID:{pid} terminated with signal: {sig}");
                }
                _ => { /* pass through */ }
            };
        }
        return Ok(());
    }

    fn boot_vm(&mut self) -> Result<i32, Box<dyn Error>> {
        let mut pid: i32 = 0;

        if self.vm_parameters.boot_operation != ControlState::Start {
            // Boot operation in JSON file not set to start
            return Ok(pid);
        }

        let mut args: Vec<CString> = Vec::new();
        args.push(CString::new(VM_BINARY_FILE)?);
        for disk_parameter in &self.vm_parameters.disk {
            args.push(CString::new(format!(
                "--disk={},label={},rw={}",
                disk_parameter.image,
                disk_parameter.label,
                disk_parameter.read_write
            ))?);
        }
        args.push(CString::new(format!("--vm={}", self.vm_parameters.name))?);

        // Do fork and exec
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                pid = child.into();
                thread::sleep(Duration::new(
                    self.vm_parameters.boot_wait_time.into(),
                    0,
                ));

                // Wait for qcrosvm to exit
                match Self::wait_for_exit(&pid, Some(false)) {
                    Ok(_) => {
                        return Ok(pid);
                    }
                    Err(_) => {
                        error!("{}: qcrosvm exited unexpectedly", self.vm_parameters.name);
                        return Err("qcrosvm exited unexpectedly".into());
                    }
                }
            }
            Ok(ForkResult::Child) => {
                let args_obj: Vec<&CStr> =
                    args.iter().map(|c| c.as_c_str()).collect();
                match execv(args_obj[0], &args_obj) {
                    Ok(_) => {}
                    Err(_) => {
                        error!("+----------------------------------------+");
                        error!(
                            "\t{}: launch failed. exiting...",
                            self.vm_parameters.name
                        );
                        error!("+----------------------------------------+");
                    }
                }
                unsafe {
                    _exit(1);
                }
            }
            Err(e) => {
                error!("Fork failed = {}", e);
                return Err("Fork failed".into());
            }
        }
    }

    fn boot_sequence(&mut self) -> Result<i32, Box<dyn Error>> {
        let mut boot_try = 0u8;
        loop {
            boot_try += 1;

            match self.boot_vm() {
                Ok(pid) => {
                    info!(
                        "Boot operation completed for {}, pid={}",
                        self.vm_parameters.name, pid
                    );
                    return Ok(pid);
                }
                Err(response) => {
                    if boot_try < self.vm_parameters.try_count {
                        error!(
                            "VM boot operation failed for {}, trying again",
                            self.vm_parameters.name
                        );
                        thread::sleep(Duration::new(
                            self.vm_parameters.boot_wait_time.into(),
                            0,
                        ));
                        continue;
                    } else {
                        error!(
                            "{} Boot Failed!!! No. of attempts: {boot_try}",
                            self.vm_parameters.name
                        );
                        return Err(response);
                    }
                }
            }
        }
    }

    fn launch_vm(&mut self) -> Result<i32, Box<dyn Error>> {
        info!("Requested launchVM for {}", self.vm_parameters.name);
        if self.vm_state == VirtualMachineState::RUNNING {
            info! {"{} is already in running state.", self.vm_parameters.name};
            return Ok(0);
        }
        return self.boot_sequence();
    }

    pub fn launch_autostart_vm(&mut self) -> () {
        debug!("In launch_autostart_vm() for {}", self.vm_parameters.name);

        if let Err(e) = self.launch_vm() {
            error!(
                "launch_autostart_vm: {} failed with reason: {e}",
                self.vm_parameters.name
            );
        }

        self.autostart_done = true;
    }

    fn set_vm_status_property(&self, vm_status: &str) -> () {
        let vm_status_prop =
            format!("vendor.qvirtmgr.{}.status", self.vm_parameters.name);
        let _res = system_properties::write(&vm_status_prop, vm_status); // Ignore the result.
    }
}

// ======================================================================
// CORE IMPLEMENTATION
// ======================================================================
pub struct VirtualMachine {
    // to_binder consumes VirtualMachine (unique_ptr).
    // As result, cannot store it in virtualization_service.rs map.
    // Instead, put a reference to VmInstance in each VirtualMachine.
    // Store the VmInstance and give out a new IVirtualMachine wrapper each time.

    // vm_instance
    // - Arc -> multple threads calling start, etc.
    // - Mutex -> Only single access at a time.
    pub vm_instance: Arc<Mutex<VmInstance>>,
}

impl VirtualMachine {
    pub fn to_binder(self) -> Strong<dyn IVirtualMachine> {
        BnVirtualMachine::new_binder(self, BinderFeatures::default())
    }
}

impl Interface for VirtualMachine {}

// All operations must be atomic.
impl IVirtualMachine for VirtualMachine {
    fn getState(&self) -> Result<VirtualMachineState, Status> {
        // Holds lock until state is returned
        if let Ok(instance) = self.vm_instance.lock() {
            info!(
                "Requested getState for {} from pid={}. State is {:?}",
                instance.vm_parameters.name,
                ThreadState::get_calling_pid(),
                instance.vm_state
            );
            return Ok(instance.vm_state);
        }
        // Strange case, mutex poisoned.
        return Err(Status::new_exception_str(
            ExceptionCode::SERVICE_SPECIFIC,
            Some("Internal Error"),
        ));
    }

    fn registerCallback(
        &self,
        callback: &Strong<(dyn IVirtualMachineCallback)>,
    ) -> Result<(), Status> {
        // Holds lock until cb registered
        if let Ok(mut instance) = self.vm_instance.lock() {
            info!(
                "Requested Callback Registration for {} from pid={}, uid={}",
                instance.vm_parameters.name,
                ThreadState::get_calling_pid(),
                ThreadState::get_calling_uid()
            );

            // Check if the callback is duplicate
            if instance
                .virtual_machine_callbacks
                .iter()
                .any(|cb| cb == callback)
            {
                error!("Duplicate request from CallbackObject for {} from pid={}, uid={}",
                    instance.vm_parameters.name, ThreadState::get_calling_pid(), ThreadState::get_calling_uid());
                return Err(Status::new_exception_str(
                    ExceptionCode::ILLEGAL_ARGUMENT,
                    Some("The callback is already registered"),
                ));
            }
            instance.virtual_machine_callbacks.push(callback.clone());

            // Register death notification to remove the cb when client dies.
            let id = instance.death_id.fetch_add(1, Ordering::SeqCst); // Generate unique ID for the hashmap.
            let pid = ThreadState::get_calling_pid();
            let uid = ThreadState::get_calling_uid();
            let vm_instance_clone = self.vm_instance.clone();
            let callback_clone = callback.clone();
            // Create the death notification callback
            let mut death_recipient = DeathRecipient::new(move || {
                // Find and remove the stored callback and DeathRecipient.
                if let Ok(mut vm_instance) = vm_instance_clone.lock() {
                    info!(
                        "Recieved death notification for {} - client {id} with pid={pid}, uid={uid}!",
                        vm_instance.vm_parameters.name);
                    if let Some(idx) = vm_instance
                        .virtual_machine_callbacks
                        .iter()
                        .position(|cb| *cb == callback_clone)
                    {
                        debug!(
                            "Cleared the callback object for {} - client {id} with pid={pid}, uid={uid}!",
                            vm_instance.vm_parameters.name);
                        vm_instance.virtual_machine_callbacks.remove(idx);
                    } else {
                        // Strange case
                        debug!(
                            "Callback object not found for {} - client {id} with pid={pid}, uid={uid}!",
                            vm_instance.vm_parameters.name);
                    }
                    // If death notification triggered, it is guaranteed that this death recipient is stale.
                    // Remove it regardless of if cb was found. It cannot be triggered again.
                    vm_instance.death_recipients.remove(&id);
                }
            });
            let mut cb_binder = callback.as_binder();
            cb_binder.link_to_death(&mut death_recipient)?;

            debug!(
                "Registered callback for {} - client {id} with pid={pid}, uid={uid}!",
                instance.vm_parameters.name);

            // Add DeathRecipient to a vector to keep it alive.
            instance.death_recipients.insert(id, death_recipient);
            return Ok(());
        }
        // Strange case, mutex poisoned.
        return Err(Status::new_exception_str(
            ExceptionCode::SERVICE_SPECIFIC,
            Some("Internal Error"),
        ));
    }

    fn start(&self) -> Result<(), Status> {
        if let Ok(mut instance) = self.vm_instance.lock() {
            // Holds lock until completely done with boot.
            info!(
                "Requested start for {} from pid={}",
                instance.vm_parameters.name,
                ThreadState::get_calling_pid()
            );

            // Check if on demand VM
            if !(instance.vm_parameters.enable
                && instance.vm_parameters.on_demand_start_supported)
            {
                error!("Request received from pid={} to start a Vm that doesn't support on-demand start, rejecting it.",
                        ThreadState::get_calling_pid());
                debug!(
                    "enable: {}, on_demand_start_supported: {}",
                    instance.vm_parameters.enable,
                    instance.vm_parameters.on_demand_start_supported
                );
                return Err(Status::new_exception(
                    ExceptionCode::UNSUPPORTED_OPERATION,
                    None,
                ));
            }

            // Check if autostart and not complete yet
            if instance.vm_parameters.autostart
                && !instance.vm_parameters.no_fs_dependency
                && !instance.autostart_done
            {
                error!("autostart enabled for this VM, requested start from pid={} while bootup ongoing, rejecting it.",
                       ThreadState::get_calling_pid());
                debug!(
                    "autostart: {}, FSDependency: {}, autostart_done: {}",
                    instance.vm_parameters.autostart,
                    instance.vm_parameters.no_fs_dependency,
                    instance.autostart_done
                );
                return Err(Status::new_service_specific_error_str(ERROR_VM_START,
                       Some("autostart enabled for this VM, requested start while bootup ongoing")));
            }

            // Check for unsupported restart request
            if instance.vm_state == VirtualMachineState::STOPPED {
                error!("Request received from pid={} to start a Vm which is in stopped state, rejecting it.",
                        ThreadState::get_calling_pid());
                return Err(Status::new_service_specific_error_str(ERROR_VM_START,
                        Some("Cannot start a Vm which is in STOPPED state.")));
            }

            // Launch it!
            match instance.launch_vm() {
                Ok(_) => return Ok(()),
                Err(response) => {
                    error!(
                        "start: {} launch failed with reason: {response}",
                        instance.vm_parameters.name
                    );
                    return Err(Status::new_service_specific_error_str(
                        ERROR_VM_START,
                        Some(response.to_string()),
                    ));
                }
            }
        }
        // Strange case, mutex poisoned.
        return Err(Status::new_exception_str(
            ExceptionCode::SERVICE_SPECIFIC,
            Some("Internal Error"),
        ));
    }
}
