/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

package vendor.qti.microdroid_test;


/**
 * This is the service exposed by the test payload, called by the test app.
 * {@hide}
 */
interface ITestPayload {
    const long SERVICE_PORT = 6432;

    /* add two integers. */
    int addInteger(int a, int b);

    /* Prints out hello world and returns that string */
    String hello(String message);

    /**
     * Request the service to exit, triggering the termination of the VM. This may cause any
     * requests in flight to fail.
     */
    oneway void quit();

}
