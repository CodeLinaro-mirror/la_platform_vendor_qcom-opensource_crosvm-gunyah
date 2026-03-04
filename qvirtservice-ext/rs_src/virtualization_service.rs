/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
*/

use crate::virtual_machine::{VirtualMachine, VmInstance, VmParameters, UeventInfo};

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

pub struct VmInstanceWrapper {
    // VmInstance
    // - Arc -> Shared between threads (here, autostart, getVm).
    // - Mutex -> Needs to be mutable anywhere (autostart_complete, etc.)
    instance: Arc<Mutex<VmInstance>>,
    enabled: bool,
    fs_dependency_timeout: u16,
}

pub struct FsDependency {
    no_fs_dependency: bool,
    fs_dependency_prop: String,
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

        let mut autostart_vms = Vec::<(String, FsDependency)>::new();

        // Parse the config file
        if let Ok(vm_parameters_list) = Self::parse_vm_config_file() {


            for vm_param in vm_parameters_list {
                // Save data now for easy access (before locked in mutex).
                let name = vm_param.name.clone();
                let enabled = vm_param.enable.clone();
                let no_fs_dependency = vm_param.no_fs_dependency.clone();
                let fs_dependency_timeout = vm_param.fs_dependency_timeout.clone();
                let fs_dependency_prop = vm_param.fs_dependency_prop.clone();

                // Save autostart vms for easy access.
                if vm_param.autostart && vm_param.enable {
                    autostart_vms.push((
                        name.clone(),
                        FsDependency {
                            no_fs_dependency: no_fs_dependency,
                            fs_dependency_prop: fs_dependency_prop,
                        },
                    ));
                }

                // Create a VmInstance and put the vm_params inside.
                // When getVm called, give out a VirtualMachine wrapping a ref to the instance
                let wrpr = VmInstanceWrapper {
                    instance: Arc::new(Mutex::new(VmInstance::new(vm_param))),
                    enabled: enabled,
                    fs_dependency_timeout: fs_dependency_timeout,
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
        for (name, fs_dep) in &autostart_vms {
            if let Some(wrapper) = service.vm_instance_map.get(name.as_str()) {
                let timeout_for_thread = wrapper.fs_dependency_timeout.clone();
                let instance_for_thread = wrapper.instance.clone();
                if fs_dep.no_fs_dependency {
                    let handle = thread::spawn(move || {
                        let mut vm_ssr_enablecheck:bool = false;
                        let mut vm_autostart_done:bool = false;
                        let mut vm_autoshutdown_enablecheck:bool = false;
                        if let Ok(mut vm) = instance_for_thread.lock() {
                            vm.launch_autostart_vm();
                            vm_ssr_enablecheck = vm.vm_parameters.vm_ssr_enable;
                            vm_autostart_done = vm.autostart_done;
                            vm_autoshutdown_enablecheck = vm.vm_parameters.vm_autoshutdown_enable;
                            if vm.vm_parameters.vm_ssr_enable && vm.autostart_done{
                                info!("autostart done, going to call autostart_connectvm");
                                match vm.autostart_connectvm() {
                                    Ok(0)=>{
                                        info!("VM userpspace connection is successful");
                                    },
                                    Ok(_)=>{
                                        info!("VM userspace connection is not successful, Will be placed in crashed state");
                                    },
                                    Err(response) => {
                                        error!(
                                            "VM: {} has been removed: {response}",
                                            vm.vm_parameters.name
                                        );
                                    }
                                }
                            }
                        }
                        if vm_ssr_enablecheck && vm_autoshutdown_enablecheck && vm_autostart_done {
                            let vm_instance = instance_for_thread.clone();
                            info!("Auto Shutdown Thread has been initiated");
                            VmInstance::auto_shutdown_thread_handle_initiator(vm_instance);
                        }
                    });
                    no_fs_dependent_handles.push(handle);
                } else {
                    let fs_dep_prop = fs_dep.fs_dependency_prop.clone();
                    //Wait for timeout if fs dependent
                    thread::spawn(move || {
                        let mut vm_ssr_enablecheck:bool = false;
                        let mut vm_autoshutdown_enablecheck:bool = false;
                        let mut watcher = match system_properties::PropertyWatcher::new(&fs_dep_prop) {
                            Ok(w) => w,
                            Err(e) => {
                                error!("Failed to create property watcher for {}: {}", fs_dep_prop, e);
                                if let Ok(mut vm) = instance_for_thread.lock() {
                                    vm.autostart_done = true;
                                }
                                return;
                            }
                        };
                        if let Ok(_) = watcher.wait_for_value(
                            "1",
                            Some(Duration::new(timeout_for_thread.into(), 0)),
                        ) {
                            debug!("Fs Dependency satisfied");
                            // Only aquire lock after wait for boot complete.
                            // Allows clients to register cbs during the wait.
                            if let Ok(mut vm) = instance_for_thread.lock() {
                                vm.launch_autostart_vm();
                                vm_ssr_enablecheck = vm.vm_parameters.vm_ssr_enable;
                                vm_autoshutdown_enablecheck = vm.vm_parameters.vm_autoshutdown_enable;
                                if vm.vm_parameters.vm_ssr_enable{
                                    match vm.autostart_connectvm() {
                                        Ok(-1)=>{
                                            info!("VM userspace connect is not supported");
                                        },
                                        Ok(0)=>{
                                            info!("VM userpspace connection is successful");
                                        },
                                        Ok(_)=>{
                                            info!("VM userspace connection is not successful, Will be placed in crashed state");
                                        },
                                        Err(response) => {
                                            error!(
                                                "Client: {} has been removed: {response}",
                                                vm.vm_parameters.name
                                            );
                                        }
                                    }
                                }
                            }
                            if vm_ssr_enablecheck && vm_autoshutdown_enablecheck {
                                let vm_instance = instance_for_thread.clone();
                                info!("Auto Shutdown Thread has been initiated");
                                VmInstance::auto_shutdown_thread_handle_initiator(vm_instance);
                            }
                        } else {
                            error!("Timed out checking for {}.", fs_dep_prop);
                            if let Ok(mut vm) = instance_for_thread.lock() {
                                vm.autostart_done = true;
                            }
                        }
                    });
                }
            } else {
                error!("VM '{}' not found in instance map, skipping autostart", name);
            }
        }

        // Join the no_fs_dependent threads
        for handle in no_fs_dependent_handles {
            let _res = handle.join();
        }

        return service;
    }

    fn parse_event(msg: Vec<u8>) -> Option<UeventInfo> {
        if let Ok(msg) = String::from_utf8(msg) {
            let mut pairs: Vec<&str> =
                msg.trim_matches('\0').split('\0').collect();
            pairs.retain(|s| s.contains("="));

            let mut event = None;
            let mut vm_name = None;
            let mut event_reason = None;
            for pair in pairs {
                let p: Vec<&str> = pair.splitn(2, "=").collect();
                match p[0] {
                    "EVENT" => event = Some(p[1].to_string()),
                    "vm_name" => vm_name = Some(p[1].to_string()),
                    "vm_exit" => event_reason = Some(p[1].to_string()),
                    _ => continue,
                };
            }
            let uevent_info = UeventInfo::new(vm_name.clone().unwrap_or_else(|| String::new()),event.clone().unwrap_or_else(|| String::new()),event_reason.clone().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0));
            if !uevent_info.event.is_empty() {
                info!("These are the uevent_info parameter VM Name: {}, \n \t\t UEVENT Received: {}, \n \t\t Uevent Reason: {}", uevent_info.vm_name, uevent_info.event, uevent_info.event_reason.to_string());
            }
            return Some(uevent_info);
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
                match vm_instance_map.get(&event.vm_name) {
                    Some(wrpr) => {
                        if let Ok(mut instance) = wrpr.instance.lock() {
                            debug!("Initiating request to notify state change to {} clients", &event.vm_name);
                            instance.notify_clients(&event.event, &event.event_reason.to_string());
                        }
                    }
                    None => {
                        if !event.vm_name.is_empty() {
                            error!("Invalid vm_name received from uevent");
                        }
                    }
                }
            }
        }
    }

    fn uevent_listener(vm_instance_map: Arc<VmInstanceMap>) -> () {
        info!("started uevent listener thread");
        let uevent_fd = match UEvent::uevent_open_socket(64 * 1024, true) {
            Ok(fd) => fd,
            Err(e) => {
                error!("Failed to open uevent socket: {}", e);
                return;
            }
        };
        if let Err(e) = fcntl(uevent_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)) {
            error!("Failed to set socket to non-blocking mode: {}", e);
            return;
        }
        loop {
            let mut ufd = [PollFd::new(uevent_fd, PollFlags::POLLIN)];
            if let Ok(nr) = poll(&mut ufd, -1) {
                if nr < 0 {
                    continue;
                }
                if let Some(revents) = ufd[0].revents() {
                    if revents.contains(PollFlags::POLLIN) {
                        Self::handle_uevent_event(uevent_fd, &vm_instance_map);
                    }
                }
            }
        }
    }

    // Returns a vector of vmParameters.
    fn parse_vm_config_file() -> Result<Vec<VmParameters>, Box<dyn Error>> {
        let mut vm_parameters_list = Vec::<VmParameters>::new();

        // Uses serde_json to deserialize directly into strongly typed VmParameters object.
        let vendor_config_file = File::open(VENDOR_CONFIG_FILE)?;
        let root: Value = match serde_json::from_reader(vendor_config_file){
            Ok(parsed) => parsed,
            Err(e) => {
                    error!("Parsing of JSON file is incorrect : {}",e);
                    return Err(format!("Error parsing JSON: {}",e).into());
                }
        };
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
