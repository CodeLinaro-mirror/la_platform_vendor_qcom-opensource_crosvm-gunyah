/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
*/

mod utils;
mod virtual_machine;
mod virtualization_service;

use crate::virtualization_service::VirtualizationService;
use log::Level;

fn main() {
    binder::ProcessState::set_thread_pool_max_thread_count(12);
    binder::ProcessState::start_thread_pool();

    let _init_success = logger::init(
        logger::Config::default()
            .with_tag_on_device("qvirtservice-shutdown_rs")
            .with_min_level(Level::Error)
    );

    let virt_service = VirtualizationService::virtualization_service();
    let virt_service_bndr = virt_service.to_binder();
    let descriptor = VirtualizationService::get_descriptor();
    binder::add_service(
        &format!("{}/default", descriptor),
        virt_service_bndr.as_binder(),
    )
    .expect("Failed to register service.");

    // Do not return
    binder::ProcessState::join_thread_pool()
}
