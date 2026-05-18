/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

//! This crate is the rust based implementation of ITestPayload service.
//! It implements some simple APIs to just test where all the service
//! touch points are for creating payloads in a Microdroid Instance.

#![allow(non_snake_case)]

pub mod test_payload;
use log::{debug, error};
use std::os::raw::c_void;
use std::panic;
use std::ptr;
use vm_payload_rs::{
    AIBinder,
    AVmPayload_notifyPayloadReady, AVmPayload_runVsockRpcServer,
};
use anyhow::Result;
use binder::unstable_api::AsNative;
use vendor_qcom_microdroid_test::aidl::vendor::qti::microdroid_test::ITestPayload::SERVICE_PORT;

#[no_mangle]
/// Entry point for .so Payloads
pub extern "C" fn AVmPayload_main(){
    android_logger::init_once(
        android_logger::Config::default().with_tag("TestPayloadVM").with_max_level(log::LevelFilter::Debug),
    );
    // Redirect panic messages to logcat.
    panic::set_hook(Box::new(|panic_info| {
        error!("{}", panic_info);
    }));

    if let Err(e) = try_main() {
        error!("failed with {:?}", e);
        std::process::exit(1);
    }
}

/// A Result protected main to catch any propagated errors
pub fn try_main() -> Result<()> {

    debug!("Ssatter sanity is starting as a rpc service.");
    let param = ptr::null_mut();
    let mut service = test_payload::new_binder()?.as_binder();

    // SAFETY:
    // We need to use an unsafe block when calling a C API
    unsafe {
        // SAFETY: We hold a strong pointer, so the raw pointer remains valid. The bindgen AIBinder
        // is the same type as sys::AIBinder.
        let service = service.as_native_mut() as *mut AIBinder;
        // SAFETY: It is safe for on_ready to be invoked at any time, with any parameter.
        AVmPayload_runVsockRpcServer(service, SERVICE_PORT.try_into()?, Some(on_ready), param);
    }

}

extern "C" fn on_ready(_param: *mut c_void) {
    // SAFETY: Invokes a method from the bindgen library `vm_payload_bindgen` which is safe to
    // call at any time.
    unsafe { AVmPayload_notifyPayloadReady() };
}
