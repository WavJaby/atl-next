// SPDX-License-Identifier: MIT
//! Hand-written codecs, one file per Java class at its Java path: the converter's generated parcelables.rs re-exports each instead of an opaque type (bp2ninja aidlcrates.go).
//! Modules keep the Java class names, so `codecs::<java path>::<Name>::<Name>` is derived from the class alone.
#![allow(non_snake_case)]

pub mod android {
    pub mod app {
        pub mod ActivityManager_TaskDescription;
        pub mod ApplicationErrorReport_ParcelableCrashInfo;
        pub mod ContentProviderHolder;
        pub mod servertransaction {
            pub mod ClientTransaction;
        }
    }
    pub mod content {
        pub mod ComponentName;
        pub mod Intent;
        pub mod IntentFilter;
        pub mod pm {
            pub mod ActivityInfo;
            pub mod ApplicationInfo;
            pub mod InstrumentationInfo;
            pub mod PackageInfo;
            pub mod ParceledListSlice;
            pub mod ProviderInfo;
            pub mod ProviderInfoList;
        }
        pub mod res {
            pub mod CompatibilityInfo;
            pub mod Configuration;
        }
    }
    pub mod graphics {
        pub mod Rect;
    }
    pub mod os {
        pub mod Bundle;
    }
    pub mod util {
        pub mod MergedConfiguration;
    }
    pub mod view {
        pub mod DisplayInfo;
        pub mod InputChannel;
        pub mod InputDevice;
        pub mod InsetsSourceControl_Array;
        pub mod InsetsState;
        pub mod SurfaceControl;
        pub mod WindowManager_LayoutParams;
        pub mod WindowRelayoutResult;
    }
    pub mod window {
        pub mod ClientWindowFrames;
    }
}

pub mod java {
    pub mod io {
        pub mod FileDescriptor;
    }
    pub mod util {
        pub mod Map;
    }
}
