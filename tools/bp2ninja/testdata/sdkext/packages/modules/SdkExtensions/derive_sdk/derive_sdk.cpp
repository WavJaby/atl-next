// SPDX-License-Identifier: MIT
// Synthetic stand-in in derive_sdk.cpp's declaration shapes, for sdkext_test.go. Not AOSP's text: the real file is read from the tree.

static const std::unordered_map<std::string, SdkModule> kApexNameToModule = {
    {"com.android.art", SdkModule::ART},
    {"com.android.adservices", SdkModule::AD_SERVICES},
    {"com.android.extservices", SdkModule::EXT_SERVICES},
    {"com.android.mediaprovider", SdkModule::MEDIA_PROVIDER},
};

static const std::unordered_set<SdkModule> kRModules = {SdkModule::EXT_SERVICES, SdkModule::MEDIA_PROVIDER};

static const std::unordered_set<SdkModule> kSModules = {SdkModule::ART};

static const std::unordered_set<SdkModule> kTModules = {SdkModule::AD_SERVICES};

static const std::unordered_set<SdkModule> kUModules = {};

static const std::unordered_set<SdkModule> kVModules = {};

static const std::unordered_set<SdkModule> kBModules = {};
