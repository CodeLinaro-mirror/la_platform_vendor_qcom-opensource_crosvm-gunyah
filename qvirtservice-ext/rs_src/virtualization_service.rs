/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
*/

use crate::virtual_machine::{VirtualMachine, VmInstance, VmParameters};

use crate::utils::UEvent;

use rustutils::system_properties;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use std::os::unix::io::RawFd;
use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use libc::_exit;

use log::{debug, error, info};

use serde_json::Value;

use vendor_qti_qvirt::aidl::vendor::qti::qvirt::{
    IVirtualMachine::IVirtualMachine,
    IVirtualizationService::{
        BnVirtualizationService, BpVirtualizationService,
        IVirtualizationService,
    },
};
use vendor_qti_qvirt::binder::{
    BinderFeatures, ExceptionCode, Interface, Status, Strong, ThreadState,
};

static VENDOR_CONFIG_FILE: &str = "/vendor/etc/qvirtmgr-vndr.json";
static BOOT_COMPLETE_PROP: &str = "sys.boot_completed";

pub struct VmInstanceWrapper {
    // VmInstance
    // - Arc -> Shared between threads (here, autostart, getVm).
    // - Mutex -> Needs to be mutable anywhere (autostart_complete, etc.)
    instance: Arc<Mutex<VmInstance>>,
    enabled: bool,
    boot_complete_timeout: u16,
}
//                          < name , VmInstanceWrapper>
type VmInstanceMap = HashMap<String, VmInstanceWrapper>;

pub struct VirtualizationService {
    // VmInstanceMap
    // - Arc -> Shared between threads (here, uevent).
    // - !Mutex -> Only modified in here once in constructor.
    vm_instance_map: Arc<VmInstanceMap>,
}

impl VirtualizationService {
    pub fn to_binder(self) -> Strong<dyn IVirtualizationService> {
        BnVirtualizationService::new_binder(self, BinderFeatures::default())
    }

    pub fn get_descriptor() -> String {
        BpVirtualizationService::get_descriptor().to_string()
    }

    pub fn virtualization_service() -> Self {
        let mut vm_instance_map: VmInstanceMap = Default::default();

        // Store the vm's thread handle for the no_fs_dependent ones.
        let mut no_fs_dependent_handles = Vec::new();

        // Store autostart vms
        let mut autostart_vms = Vec::<(String, bool)>::new();

        // Parse the config file
        if let Ok(vm_parameters_list) = Self::parse_vm_config_file() {
            info!("Parsing VM Config File Success.");

            for vm_param in vm_parameters_list {
                // Save data now for easy access (before locked in mutex).
                let name = vm_param.name.clone();
                let enabled = vm_param.enable.clone();
                let boot_complete_timeout =
                    vm_param.boot_complete_timeout.clone();

                // Save autostart vms for easy access.
                if vm_param.autostart && vm_param.enable {
                    autostart_vms.push((
                        name.clone(),
                        vm_param.no_fs_dependency.clone(),
                    ));
                }

                // Create a VmInstance and put the vm_params inside.
                // When getVm called, give out a VirtualMachine wrapping a ref to the instance
                let wrpr = VmInstanceWrapper {
                    instance: Arc::new(Mutex::new(VmInstance::new(vm_param))),
                    enabled: enabled,
                    boot_complete_timeout: boot_complete_timeout,
                };

                vm_instance_map.insert(name, wrpr); // Put a ref count in the map
            }
        } else {
            unsafe {
                _exit(1);
            }
        }

        let service = VirtualizationService {
            vm_instance_map: Arc::new(vm_instance_map),
        };

        // Create the uevent thread
        let vm_instance_map_clone = service.vm_instance_map.clone();
        thread::spawn(move || {
            Self::uevent_listener(vm_instance_map_clone);
        });

        // Launch autostart VMs
        for (name, no_fs_dependency) in autostart_vms {
            let wrapper = service.vm_instance_map.get(&name).unwrap();
            let timeout_for_thread = wrapper.boot_complete_timeout.clone();
            let instance_for_thread = wrapper.instance.clone();
            if no_fs_dependency {
                let handle = thread::spawn(move || {
                    if let Ok(mut vm) = instance_for_thread.lock() {
                        vm.launch_autostart_vm();
                    }
                });
                no_fs_dependent_handles.push(handle);
            } else {
                // Wait for timeout if fs dependent
                thread::spawn(move || {
                    let mut watcher = system_properties::PropertyWatcher::new(BOOT_COMPLETE_PROP).unwrap();
                    if let Ok(_) = watcher.wait_for_value(
                        "1",
                        Some(Duration::new(timeout_for_thread.into(), 0)),
                    ) {
                        debug!("System boot completed.");
                        // Only aquire lock after wait for boot complete.
                        // Allows clients to register cbs during the wait.
                        if let Ok(mut vm) = instance_for_thread.lock() {
                            vm.launch_autostart_vm();
                        }
                    } else {
                        error!("Timed out checking for sys.boot_completed.");
                        if let Ok(mut vm) = instance_for_thread.lock() {
                            vm.autostart_done = true;
                        }
                    }
                });
            }
        }

        // Join the no_fs_dependent threads
        for handle in no_fs_dependent_handles {
            let _res = handle.join();
        }

        return service;
    }

    fn parse_event(msg: Vec<u8>) -> Option<(String, String)> {
        if let Ok(msg) = String::from_utf8(msg) {
            let mut pairs: Vec<&str> =
                msg.trim_matches('\0').split('\0').collect();
            pairs.retain(|s| s.contains("="));

            let mut event = None;
            let mut vm_name = None;
            for pair in pairs {
                let p: Vec<&str> = pair.splitn(2, "=").collect();
                match p[0] {
                    "EVENT" => event = Some(p[1].to_string()),
                    "vm_name" => vm_name = Some(p[1].to_string()),
                    _ => continue,
                };
            }
            if let (Some(e), Some(n)) = (event, vm_name) {
                return Some((e, n));
            }
        }
        return None;
    }

    fn handle_uevent_event(
        uevent_fd: RawFd,
        vm_instance_map: &Arc<VmInstanceMap>,
    ) -> () {
        let mut msg = Vec::<u8>::new();
        while let Ok(_res) =
            UEvent::uevent_kernel_multicast_recv(uevent_fd, &mut msg, 1024)
        {
            if let Some(event) = Self::parse_event(msg.clone()) {
                match vm_instance_map.get(&event.1) {
                    Some(wrpr) => {
                        if let Ok(mut instance) = wrpr.instance.lock() {
                            debug!("Initiating request to notify state change to {} clients", event.1);
                            instance.notify_clients(&event.0);
                        }
                    }
                    None => {
                        error!("Invalid vm_name received from uevent");
                    }
                }
            }
        }
    }

    fn uevent_listener(vm_instance_map: Arc<VmInstanceMap>) -> () {
        info!("started uevent listener thread");
        let uevent_fd = UEvent::uevent_open_socket(64 * 1024, true).unwrap();
        fcntl(uevent_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();

        loop {
            let mut ufd = [PollFd::new(uevent_fd, PollFlags::POLLIN)];
            if let Ok(nr) = poll(&mut ufd, -1) {
                if nr < 0 {
                    continue;
                }
                if ufd[0].revents().unwrap().contains(PollFlags::POLLIN) {
                    Self::handle_uevent_event(
                        uevent_fd.clone(),
                        &vm_instance_map,
                    );
                }
            }
        }
    }

    // Returns a vector of vmParameters.
    fn parse_vm_config_file() -> Result<Vec<VmParameters>, Box<dyn Error>> {
        let mut vm_parameters_list = Vec::<VmParameters>::new();

        // Uses serde_json to deserialize directly into strongly typed VmParameters object.
        let vendor_config_file = File::open(VENDOR_CONFIG_FILE)?;
        let root: Value = serde_json::from_reader(vendor_config_file).unwrap();
        let json_config_array: &Vec<Value> = root
            .get("qvirtmgr")
            .and_then(|mgr| mgr.get("vm_config"))
            .and_then(|arr| arr.as_array())
            .ok_or("VM Configuration is invalid.")?;
        for config in json_config_array {
            match serde_json::from_value::<VmParameters>(config.to_owned()) {
                Ok(vm_param) => {
                    vm_parameters_list.push(vm_param);
                }
                Err(e) => {
                    // Just skip any malformed vm config.
                    let name: &str = config
                        .get("name")
                        .and_then(|val| val.as_str())
                        .unwrap_or("No Name Specified");
                    error!("Skipping entry for '{}'. Err: {e}", name);
                    continue;
                }
            }
        }
        Ok(vm_parameters_list)
    }
}

impl Interface for VirtualizationService {}

impl IVirtualizationService for VirtualizationService {
    fn getVm(
        &self,
        vm_name: &str,
    ) -> Result<Strong<(dyn IVirtualMachine)>, Status> {
        info!(
            "getVm: Requested vm handle for {} from pid={}",
            vm_name,
            ThreadState::get_calling_pid()
        );

        if let Some(instance_wrpr) = self.vm_instance_map.get(vm_name) {
            if !instance_wrpr.enabled {
                error!("getVm: enable bit is false for {vm_name}, rejecting request.");
                return Err(Status::new_exception_str(
                    ExceptionCode::UNSUPPORTED_OPERATION,
                    Some("enable bit false."),
                ));
            }
            // Create a VirtualMachine which wraps the Arc<Mutex<VmInstance>>
            let virtual_machine = VirtualMachine {
                vm_instance: instance_wrpr.instance.to_owned(),
            }; // Increments ref count of instance_obj
            return Ok(virtual_machine.to_binder());
        } else {
            error!("getVm: Invalid name argument passed, rejecting request.");
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("Invalid name"),
            ));
        }
    }
}
