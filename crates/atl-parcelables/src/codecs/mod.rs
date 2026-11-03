// SPDX-License-Identifier: MIT
//! Hand-written codecs, one file per Java class at its Java path: the converter's generated parcelables.rs re-exports each instead of an opaque type (bp2ninja aidlcrates.go).
//! Modules keep the Java class names, so `codecs::<java path>::<Name>::<Name>` is derived from the class alone.
#![allow(non_snake_case)]

pub mod android {
    pub mod app {
        pub mod ActivityManager_TaskDescription;
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
    pub mod os {
        pub mod Bundle;
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
