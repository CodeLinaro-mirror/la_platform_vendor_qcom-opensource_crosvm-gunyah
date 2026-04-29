/*
 * Copyright 2021 The Android Open Source Project
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * Changes from Qualcomm Technologies, Inc. are provided under the following license:
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

//! Timeouts for common situations, with support for longer timeouts when using nested
//! virtualization.

use anyhow::Error;
use lazy_static::lazy_static;
use std::time::Duration;
use rustutils::android::system_properties;

/// Holder for the various timeouts we use.
#[derive(Debug, Copy, Clone)]
pub struct Timeouts {
    /// Total time that odrefresh may take to perform compilation
    pub odrefresh_max_execution_time: Duration,
    /// Time allowed for the CompOS VM to start up and become ready.
    pub vm_max_time_to_ready: Duration,
    /// Time we wait for a VM to exit once the payload has finished.
    pub vm_max_time_to_exit: Duration,
}

/// Return whether we will be running our VM in a VM, which causes the nested VM to run very slowly.
fn is_nested_virtualization() -> Result<bool, Error> {
    // Nested virtualization occurs when we run KVM inside the cuttlefish VM or when
    // we run trusty within qemu.
    let checks = [
        ("ro.product.vendor.device", "vsoc_"), // vsoc_x86, vsoc_x86_64, vsoc_x86_64_only, ...
        ("ro.hardware", "qemu_"),              // qemu_trusty, ...
    ];

    for (property, prefix) in checks {
        if let Some(value) = system_properties::read(property)? {
            if value.starts_with(prefix) {
                return Ok(true);
            }
        }
    }

    // No match -> not nested
    Ok (false)
}

lazy_static! {
/// The timeouts that are appropriate on the current platform.
    pub static ref TIMEOUTS: Timeouts = if is_nested_virtualization().unwrap() {
        // Nested virtualization is slow.
        EXTENDED_TIMEOUTS
    } else {
        NORMAL_TIMEOUTS
    };
}

/// The timeouts that we use normally.
const NORMAL_TIMEOUTS: Timeouts = Timeouts {
    // Note: the source of truth for this odrefresh timeout is art/odrefresh/odrefresh.cc.
    odrefresh_max_execution_time: Duration::from_secs(300),
    vm_max_time_to_ready: Duration::from_secs(15),
    vm_max_time_to_exit: Duration::from_secs(5),
};




/// The timeouts that we use when running under nested virtualization.
const EXTENDED_TIMEOUTS: Timeouts = Timeouts {
    // Note: the source of truth for this odrefresh timeout is art/odrefresh/odrefresh.cc.
    odrefresh_max_execution_time: Duration::from_secs(480),
    vm_max_time_to_ready: Duration::from_secs(120),
    vm_max_time_to_exit: Duration::from_secs(20),
};
