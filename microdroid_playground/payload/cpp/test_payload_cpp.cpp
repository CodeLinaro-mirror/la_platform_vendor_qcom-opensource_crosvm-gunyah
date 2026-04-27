/*
 * Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

#include <android/binder_manager.h>
#include <log/log.h>
#include <android/binder_process.h>
#include <iostream>
#include <string>
#include <android-base/logging.h>
#include <android-base/file.h>
#include <android-base/properties.h>
#include <android-base/result.h>
#include <android-base/scopeguard.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <sys/capability.h>
#include <sys/system_properties.h>
#include <unistd.h>
#include <vm_main.h>
#include <vm_payload.h>
#include "aidl/vendor/qti/microdroid_test/BnTestPayload.h"


using android::base::Result;
using aidl::vendor::qti::microdroid_test::BnTestPayload;

constexpr char TAG[] = "TestPayloadVMcpp";

void on_ready(void* param){
    AVmPayload_notifyPayloadReady();
}

Result<void> try_main(){
    class TestPayload: public BnTestPayload{
        public:

        // static android::base::Result<void> init();
        ::ndk::ScopedAStatus addInteger(int32_t in_a, int32_t in_b, int32_t* _aidl_return) override{
            *_aidl_return = in_a + in_b;
            ALOGI("%s, TestPayload addInteger: %d + %d = %d ",TAG, in_a, in_b, *_aidl_return);
            return ndk::ScopedAStatus::ok();
        }

        ::ndk::ScopedAStatus hello(const std::string& in_message, std::string* _aidl_return) override {
            *_aidl_return = "Hello " + in_message;
            return ndk::ScopedAStatus::ok();
        }

        ::ndk::ScopedAStatus quit() override{
            ALOGI("TestPayload quits...");
            exit(0);
        }
    };
    auto testService = ndk::SharedRefBase::make<TestPayload>();
    AIBinder* service = testService->asBinder().get();
    __android_log_write(ANDROID_LOG_INFO, TAG, "Notifying payload ready");
    auto callback = []([[maybe_unused]] void* param) { AVmPayload_notifyPayloadReady(); };
    AVmPayload_runVsockRpcServer(testService->asBinder().get(), testService->SERVICE_PORT, callback,
                                 nullptr);

    return{};
}

extern "C" int AVmPayload_main() {
    ALOGI("%s, Ssatter found payload", TAG);
    __system_property_set("debug.microdroid.app.run", "true");
    if (auto res = try_main(); res.ok()) {
        return 0;
    } else {
         __android_log_write(ANDROID_LOG_ERROR, TAG, res.error().message().c_str());
        return 1;
    }

    return 0;
}
