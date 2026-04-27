/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

//! This module is to implement the actual service of ITestPayload

#![allow(non_snake_case)]
use log::{warn, info};
use anyhow::Result;
use std::fmt::Debug;
use std::str;
use vendor_qcom_microdroid_test::aidl::vendor::qti::microdroid_test::ITestPayload::{
    BnTestPayload, ITestPayload,
};
use binder::{BinderFeatures, Interface, Result as BinderResult, Status, Strong};

/// Function to convert Result types to Binder Result types
pub fn to_binder_result<T, E: Debug>(result: Result<T, E>) -> BinderResult<T> {
    result.map_err(|e| {
        let message = format!("{:?}", e);
        warn!("Returning binder error: {}", &message);
        Status::new_service_specific_error_str(-1, Some(message))
    })
}


/// Creates a new Binder Object given the class that inherits the interface
pub fn new_binder() -> BinderResult<Strong<dyn ITestPayload>> {
    let service = TestPayload{};
    Ok(BnTestPayload::new_binder(service, BinderFeatures::default()))
}

/// A Struct to implement the ITestPayload interface
pub struct TestPayload{}

impl Interface for TestPayload{

}



impl ITestPayload for TestPayload {
    fn addInteger(&self, _arg_a: i32, _arg_b: i32) -> binder::Result<i32>{
        info!("Test Payload AddInteger:{:?} + {:?} = {:?}",_arg_a, _arg_b, _arg_a + _arg_b);
       Ok( _arg_a + _arg_b)
    }

    fn hello(&self, _arg_message: &str) -> binder::Result<String>{
        info!("Test Payload Hello: Hello {:?}",_arg_message);
        Ok("Hello ".to_owned() + _arg_message)
    }


    fn quit(&self) -> std::result::Result<(), binder::Status> {
        info!("Test Payload service is Shutting Down");
        std::process::exit(0);

    }
}
