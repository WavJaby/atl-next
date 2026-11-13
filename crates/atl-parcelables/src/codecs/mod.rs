// SPDX-License-Identifier: MIT
//! Hand-written codecs, one file per Java class at its Java path: the converter's generated parcelables.rs re-exports each instead of an opaque type (bp2ninja aidlcrates.go).
//! Modules keep the Java class names, so `codecs::<java path>::<Name>::<Name>` is derived from the class alone.
#![allow(non_snake_case)]

pub mod android {
    pub mod app {
        pub mod ActivityManager_TaskDescription;
        pub mod ApplicationErrorReport_ParcelableCrashInfo;
        pub mod ContentProviderHolder;
        pub mod NotificationChannel;
        pub mod PendingIntent;
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
            pub mod ConfigurationInfo;
            pub mod InstrumentationInfo;
            pub mod PackageInfo;
            pub mod ParceledListSlice;
            pub mod PermissionInfo;
            pub mod ProviderInfo;
            pub mod ProviderInfoList;
            pub mod ServiceInfo;
        }
        pub mod res {
            pub mod CompatibilityInfo;
            pub mod Configuration;
        }
    }
    pub mod graphics {
        pub mod Rect;
    }
    pub mod hardware {
        pub mod OverlayProperties;
    }
    pub mod media {
        pub mod AudioAttributes;
        pub mod PlayerBase_PlayerIdCard;
    }
    pub mod net {
        pub mod LinkProperties;
        pub mod Network;
        pub mod NetworkCapabilities;
        pub mod NetworkInfo;
        pub mod NetworkRequest;
        pub mod Uri;
    }
    pub mod os {
        pub mod Bundle;
        pub mod Message;
        pub mod Messenger;
        pub mod storage {
            pub mod StorageVolume;
        }
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
        pub mod PointerIcon;
        pub mod inputmethod {
            pub mod EditorInfo;
            pub mod ImeTracker_Token;
            pub mod InputBinding;
            pub mod SurroundingText;
        }
        pub mod SurfaceControl;
        pub mod SurfaceControl_Transaction;
        pub mod WindowManager_LayoutParams;
        pub mod WindowRelayoutResult;
    }
    pub mod window {
        pub mod ClientWindowFrames;
        pub mod ImeOnBackInvokedDispatcher;
        pub mod SizeConfigurationBuckets;
    }
}

pub mod java {
    pub mod io {
        pub mod FileDescriptor;
    }
    pub mod lang {
        pub mod CharSequence;
    }
    pub mod util {
        pub mod Map;
    }
}

pub mod com {
    pub mod android {
        pub mod internal {
            pub mod infra {
                pub mod AndroidFuture;
            }
            pub mod inputmethod {
                pub mod InputBindResult;
                pub mod InputConnectionCommandHeader;
                pub mod InputMethodInfoSafeList;
            }
        }
    }
}
