/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

//! This crate is to be a rust based test client for Test Payload VM.
//! It spawns a microdroid instance using the EmptyTestPayloadApp
//! and connect to the libtest_payload_rs service.

#![allow(non_snake_case)]
pub mod vm_client_lib;
pub mod timeouts;
use binder::{ParcelFileDescriptor, Strong};
use android_system_virtualizationservice::aidl::android::system::virtualizationservice::{
    IVirtualizationService::IVirtualizationService,
    VirtualMachineAppConfig::{DebugLevel::DebugLevel, Payload::Payload, VirtualMachineAppConfig},
    VirtualMachinePayloadConfig::VirtualMachinePayloadConfig,
    VirtualMachineConfig::VirtualMachineConfig,PartitionType::PartitionType, VirtualMachineState::VirtualMachineState,
};
use vendor_qcom_microdroid_test::aidl::vendor::qti::microdroid_test::ITestPayload::ITestPayload;
use tempfile::TempDir;
use log::{info, error};
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use binder::LazyServiceGuard;
use std::panic;
use vm_client_lib::Client;

use std::{thread, time::Duration};

/// The root of the  data directory available for private use by the CompOS APEX.
pub const DATA_ROOT: &str = "/data/local/tmp";

/// Name of the instance image file if there isn't one already
pub const INSTANCE_IMAGE_FILE: &str = "instance.img";

/// The file that holds the idsig for the Test Payload APK.
pub const IDSIG_FILE: &str = "idsig";

/// Constant string for the name of the Payload Library Binary Name
pub const TEST_PAYLOAD_NAME: &str = "libtest_payload_rs.so";


fn main() {
    println!("Hello, world!");
    let debuggable = env!("TARGET_BUILD_VARIANT") != "user";
    let log_level = if debuggable { log::LevelFilter::Debug } else { log::LevelFilter::Info };
    android_logger::init_once(
        android_logger::Config::default().with_tag("microdroid_native_test").with_max_level(log_level),
    );

    // Redirect panic messages to logcat.
    panic::set_hook(Box::new(|panic_info| {
        log::error!("{}", panic_info);
    }));

    if let Err(e) = try_main() {
        error!("failed with {:?}", e);
        std::process::exit(1);
    }

}

fn try_main() ->  Result<()>{
    let test_payload = start_microdroid()?;
    loop {
        if let Ok(VirtualMachineState::READY) = test_payload.vm_instance.0.state(){
            info!("Test Payload is ready ");
            break;
        }
    }

    let sum = test_payload.service.addInteger(2,3)?;
    info!("Sum: {}",sum);
    let msg = test_payload.service.hello("Host")?;
    info!("Message: {}",msg);

    test_payload.service.quit()?;

    while test_payload.vm_instance.0.state()? != VirtualMachineState::DEAD {
        thread::sleep(Duration::from_secs(5));
    }
    info!("Test Payload has died, shutting down client");


    Ok(())
}

/// Struct to hold the VM instance and the Payload service
pub struct TestPayload {
    service: Strong<dyn ITestPayload>,
    #[allow(dead_code)] // Keeps VirtualizationService & the VM alive
    vm_instance: Client,

    #[allow(dead_code)] // Keeps composd process alive
    lazy_service_guard: LazyServiceGuard,

}

/// Struct to hold the contents/paths needed to formulate the VM instance
pub struct TestInstance{
    #[allow(dead_code)]
    instance_name: String,

    instance_root: PathBuf,
    instance_image: PathBuf,
    idsig: PathBuf,
}

impl TestInstance{

    /// Create a new default instance of TestInstance
    pub fn new() -> TestInstance{
        TestInstance{
            instance_name: String::new(),
            instance_root: PathBuf::new(),
            instance_image: PathBuf::new(),
            idsig: PathBuf::new(),
        }
    }
}

impl Default for TestInstance {
    fn default() -> Self {
        Self::new()
    }
}

impl TestInstance{
    fn create_instance_image(
        &self,
        virtualization_service: &dyn IVirtualizationService,
    ) -> Result<()> {
        let instance_image = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&self.instance_image)
            .context("Creating instance image file")?;
        println!("created instance image from current image");
        let instance_image = ParcelFileDescriptor::new(instance_image);
        println!("created new parcel file descriptor");
        let size = 10 * 1024 * 1024;
        virtualization_service
            .initializeWritablePartition(&instance_image, size, PartitionType::ANDROID_VM_INSTANCE)
            .context("Writing instance image file")?;
        Ok(())
    }

    fn start_test_payload_vm(
        &self,
        virtualization_service: &dyn IVirtualizationService,
    ) -> Result<TestPayload> {
        let instance_image = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.instance_image)
            .context("Failed to open instance image")?;
        let vm_instance = Client::start(
            virtualization_service,
            instance_image,
            &self.idsig,
            "EmptyTestPayloadApp",
            TEST_PAYLOAD_NAME,
        )
        .context("Starting VM")?;

        let service = vm_instance.connect_test_payload_service().context("Connecting to TestPayload VM")?;


        Ok(TestPayload {
            vm_instance,
            service,
            lazy_service_guard: Default::default(),
        })
    }
}


fn start_microdroid() -> Result<TestPayload>{

    let temp_dir = vm_client_lib::to_binder_result(TempDir::new())?;
    let instance_name= temp_dir.into_path().into_os_string().into_string().unwrap();
    let instance_root= Path::new(DATA_ROOT).join(instance_name.as_str());
    let cloner = instance_root.clone();
    let instance_root_path= cloner.as_path();

    let test_payload_instance = TestInstance{
        instance_name,
        instance_root,
        instance_image: instance_root_path.join(INSTANCE_IMAGE_FILE),
        idsig: instance_root_path.join(IDSIG_FILE),
    };

    let _ = fs::create_dir(&test_payload_instance.instance_root);
    // println!("Created this directory: {}, dir: {}", test_payload_instance.instance_root.exists(), test_payload_instance.instance_root.clone().into_os_string().into_string().unwrap());
    // println!("Found Instance Image: {}, img file: {}", test_payload_instance.instance_image.exists(), test_payload_instance.instance_image.clone().into_os_string().into_string().unwrap());

    let virtmgr = vm_client_lib::to_binder_result(vmclient::VirtualizationService::new().context("Failed to spawn VirtualizationService"))?;
    let virtualization_service = vm_client_lib::to_binder_result(virtmgr.connect().context("Failed to connect to VirtualizationService"))?;
    println!("Set up Virtualization service");
    vm_client_lib::to_binder_result(test_payload_instance.create_instance_image(&*virtualization_service))?;
    println!("Created instance image");
    // Delete existing idsig files. Ignore error in case idsig doesn't exist.
    let _ = fs::remove_file(&test_payload_instance.idsig);


    let test_payload = vm_client_lib::to_binder_result(test_payload_instance.start_test_payload_vm(&*virtualization_service))?;
    Ok(test_payload)
}
