// SPDX-License-Identifier: MIT
//! Installer slices M3-M4a: the full package parse on AOSP's install path, `ParsingPackageUtils.parsePackage` over the package dir (the cluster branch, as InstallPackageHelper.java:1513 reaches it through PackageParser2),
//! then `PackageInfoCommonUtils.generate` (AOSP's ParsedPackage -> PackageInfo outside PMS state), returned as upstream parcels in the frames of atl-services-types `installer_output`.
//! The ParsingPackageUtils.Callback is the api-impl forwarder `org.atl.installer.ParseCallback` (on the VM's class path); every answer it gives is decided here ([`Device`]).

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use atl_services_types::installer_output::{Kind, write_frame};

use crate::ffi::{JObject, JValue, Vm};

/// PackageParser2's install flags (InstallPackageHelper.java:1502-1505): PARSE_CHATTY (1 << 31) | PARSE_ENFORCE_CODE (1 << 6) (ParsingPackageUtils.java:243, :252); not on external storage.
/// PARSE_ENFORCE_CODE is read by ScanPackageUtils.assertCodePolicy (InstallPackageHelper.java:4626), services.jar, not by the parser.
const PARSE_FLAGS: i32 = i32::MIN | 1 << 6;
/// PackageManager GET_ACTIVITIES | GET_RECEIVERS | GET_SERVICES | GET_PROVIDERS | GET_META_DATA | GET_PERMISSIONS | MATCH_DIRECT_BOOT_UNAWARE | MATCH_DIRECT_BOOT_AWARE (PackageManager.java):
/// every component (generate keeps one only if a direct-boot flag matches it, PackageInfoCommonUtils.isMatch:630-644), their meta-data and the permission arrays.
const INFO_FLAGS: i64 = 0x1 | 0x2 | 0x4 | 0x8 | 0x80 | 0x1000 | 0x0004_0000 | 0x0008_0000;
/// The user generate fills per-user fields for: the only one (UserHandle.USER_SYSTEM).
const USER: i32 = 0;

/// The device-side answers the parse asks for: PMS's in system_server, here the installer's (each one an owner decision, listed in the M3 state note).
pub struct Device {
    /// `hasFeature` (PMS mPackageParserCallback -> hasSystemFeature(feature, 0), PackageManagerService.java:2026): the features this device declares, which atld's PackageManager must answer identically.
    pub features: Vec<String>,
}

/// One parse's result: the generated PackageInfo and the ParsedPackage, local references of the caller's JNI frame.
pub struct Parsed {
    info: JObject,
    pkg: JObject,
}

/// The full parse of `dir`.
pub fn parse(vm: &Vm, dir: &Path, device: &Device) -> Result<Parsed> {
    let callback = vm.find_class(c"org/atl/installer/ParseCallback")?;
    let callback_init = vm.method(
        callback,
        c"<init>",
        c"([Ljava/lang/String;Ljava/util/Set;Ljava/util/Set;)V",
        false,
    )?;
    let array_set = vm.find_class(c"android/util/ArraySet")?;
    let set_init = vm.method(array_set, c"<init>", c"()V", false)?;
    // SystemConfig's hidden-api-whitelisted-app and install-constraints-allowed lists (PackageManagerService.java:2030-2037): no sysconfig on this device, so empty
    let hidden_api_apps = vm.new_object(array_set, set_init, &[], "new ArraySet")?;
    let constraints = vm.new_object(array_set, set_init, &[], "new ArraySet")?;
    let features = vm.strings(&device.features)?;
    let callback = vm.new_object(
        callback,
        callback_init,
        &[
            JValue { l: features },
            JValue { l: hidden_api_apps },
            JValue { l: constraints },
        ],
        "new ParseCallback",
    )?;

    // PackageParser2's own ctor values (PackageParser2.java:78-95): default DisplayMetrics; split permissions from PermissionManager, else an empty list (no permission service in this process: the empty list, as PackageParser2 does without an Application)
    let metrics_class = vm.find_class(c"android/util/DisplayMetrics")?;
    let metrics_init = vm.method(metrics_class, c"<init>", c"()V", false)?;
    let metrics = vm.new_object(metrics_class, metrics_init, &[], "new DisplayMetrics")?;
    let to_defaults = vm.method(metrics_class, c"setToDefaults", c"()V", false)?;
    vm.call_void(metrics, to_defaults, "DisplayMetrics.setToDefaults")?;
    let list_class = vm.find_class(c"java/util/ArrayList")?;
    let list_init = vm.method(list_class, c"<init>", c"()V", false)?;
    let split_permissions = vm.new_object(list_class, list_init, &[], "new ArrayList")?;

    let utils_class = vm.find_class(c"com/android/internal/pm/pkg/parsing/ParsingPackageUtils")?;
    let utils_init = vm.method(
        utils_class,
        c"<init>",
        c"([Ljava/lang/String;Landroid/util/DisplayMetrics;Ljava/util/List;Lcom/android/internal/pm/pkg/parsing/ParsingPackageUtils$Callback;)V",
        false,
    )?;
    // separateProcesses: PMS's from the debug.separate_processes property, null when unset (the default)
    let utils = vm.new_object(
        utils_class,
        utils_init,
        &[
            JValue {
                l: core::ptr::null_mut(),
            },
            JValue { l: metrics },
            JValue {
                l: split_permissions,
            },
            JValue { l: callback },
        ],
        "new ParsingPackageUtils",
    )?;
    let input = vm.parse_input()?;
    let parse = vm.method(
        utils_class,
        c"parsePackage",
        c"(Landroid/content/pm/parsing/result/ParseInput;Ljava/io/File;I)Landroid/content/pm/parsing/result/ParseResult;",
        false,
    )?;
    let file = vm.file(dir)?;
    let result = vm.call_object(
        utils,
        parse,
        &[
            JValue { l: input },
            JValue { l: file },
            JValue { i: PARSE_FLAGS },
        ],
        "ParsingPackageUtils.parsePackage",
    )?;
    let parsing = vm.parse_result(result, "ParsingPackageUtils.parsePackage")?;
    let parsing_class = vm.find_class(c"com/android/internal/pm/pkg/parsing/ParsingPackage")?;
    let hide = vm.method(
        parsing_class,
        c"hideAsParsed",
        c"()Lcom/android/internal/pm/parsing/pkg/ParsedPackage;",
        false,
    )?;
    let pkg = vm.call_object(parsing, hide, &[], "ParsingPackage.hideAsParsed")?;
    let common = vm.find_class(c"com/android/internal/pm/parsing/PackageInfoCommonUtils")?;
    let generate = vm.method(
        common,
        c"generate",
        c"(Lcom/android/server/pm/pkg/AndroidPackage;JI)Landroid/content/pm/PackageInfo;",
        true,
    )?;
    let info = vm.call_static_object(
        common,
        generate,
        &[
            JValue { l: pkg },
            JValue { j: INFO_FLAGS },
            JValue { i: USER },
        ],
        "PackageInfoCommonUtils.generate",
    )?;
    ensure!(
        !info.is_null(),
        "PackageInfoCommonUtils.generate returned null"
    );
    Ok(Parsed { info, pkg })
}

/// The native-code choice of the install path (`Cluster::install_natives`), as PMS's PackageInfoUtils.generateApplicationInfo copies it from the package setting onto the ApplicationInfo.
pub struct Abi<'a> {
    pub primary: Option<&'a str>,
    /// the dir the libraries were copied to; None when none were (extractNativeLibs=false)
    pub lib_dir: Option<&'a Path>,
}

impl Parsed {
    /// The installer's output frames: the PackageInfo with `abi` set on its ApplicationInfo (every component's ApplicationInfo is the same object, PackageInfoCommonUtils.generate:235),
    /// then per component with intent filters its name and ParsedIntentInfo list (installer_output documents the frames).
    pub fn frames(&self, vm: &Vm, abi: &Abi<'_>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        vm.frame(|| {
            let pi = vm.find_class(c"android/content/pm/PackageInfo")?;
            let app_field = vm.field(
                pi,
                c"applicationInfo",
                c"Landroid/content/pm/ApplicationInfo;",
            )?;
            let app = vm.object_field(self.info, app_field)?;
            ensure!(!app.is_null(), "PackageInfo without applicationInfo");
            let ai = vm.find_class(c"android/content/pm/ApplicationInfo")?;
            let string = c"Ljava/lang/String;";
            let primary = abi.primary.map(|a| vm.new_string(a)).transpose()?;
            let lib_dir = abi
                .lib_dir
                .map(|d| vm.new_string(&d.to_string_lossy()))
                .transpose()?;
            vm.set_object_field(
                app,
                vm.field(ai, c"primaryCpuAbi", string)?,
                primary.unwrap_or(core::ptr::null_mut()),
            )?;
            vm.set_object_field(
                app,
                vm.field(ai, c"nativeLibraryDir", string)?,
                lib_dir.unwrap_or(core::ptr::null_mut()),
            )?;
            let write = vm.method(pi, c"writeToParcel", c"(Landroid/os/Parcel;I)V", false)?;
            let bytes = marshal(vm, |parcel| {
                vm.call_void_with(
                    self.info,
                    write,
                    &[JValue { l: parcel }, JValue { i: 0 }],
                    "PackageInfo.writeToParcel",
                )
            })?;
            write_frame(&mut out, Kind::Package, &bytes)?;
            Ok(())
        })?;
        for (kind, getter) in [
            (Kind::ActivityIntents, c"getActivities"),
            (Kind::ReceiverIntents, c"getReceivers"),
            (Kind::ServiceIntents, c"getServices"),
            (Kind::ProviderIntents, c"getProviders"),
        ] {
            vm.frame(|| self.intents(vm, kind, getter, &mut out))?;
        }
        Ok(out)
    }

    /// One frame per component of `getter`'s list that has intent filters: `writeString(name)`, `writeTypedList(intents)`, as ParsedComponentImpl.writeToParcel writes those two fields.
    fn intents(
        &self,
        vm: &Vm,
        kind: Kind,
        getter: &std::ffi::CStr,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let android_package = vm.find_class(c"com/android/server/pm/pkg/AndroidPackage")?;
        let get = vm.method(android_package, getter, c"()Ljava/util/List;", false)?;
        let list = vm.call_object(self.pkg, get, &[], "AndroidPackage component list")?;
        let list_class = vm.find_class(c"java/util/List")?;
        let size = vm.method(list_class, c"size", c"()I", false)?;
        let get_at = vm.method(list_class, c"get", c"(I)Ljava/lang/Object;", false)?;
        let component = vm.find_class(c"com/android/internal/pm/pkg/component/ParsedComponent")?;
        let get_name = vm.method(component, c"getName", c"()Ljava/lang/String;", false)?;
        let get_intents = vm.method(component, c"getIntents", c"()Ljava/util/List;", false)?;
        let parcel_class = vm.find_class(c"android/os/Parcel")?;
        let write_string = vm.method(
            parcel_class,
            c"writeString",
            c"(Ljava/lang/String;)V",
            false,
        )?;
        let write_list = vm.method(
            parcel_class,
            c"writeTypedList",
            c"(Ljava/util/List;)V",
            false,
        )?;
        for i in 0..vm.call_int(list, size, &[], "List.size")? {
            vm.frame(|| {
                let c = vm.call_object(list, get_at, &[JValue { i }], "List.get")?;
                let intents = vm.call_object(c, get_intents, &[], "ParsedComponent.getIntents")?;
                if vm.call_int(intents, size, &[], "List.size")? == 0 {
                    return Ok(());
                }
                let name = vm.call_object(c, get_name, &[], "ParsedComponent.getName")?;
                let bytes = marshal(vm, |parcel| {
                    vm.call_void_with(
                        parcel,
                        write_string,
                        &[JValue { l: name }],
                        "Parcel.writeString",
                    )?;
                    vm.call_void_with(
                        parcel,
                        write_list,
                        &[JValue { l: intents }],
                        "Parcel.writeTypedList",
                    )
                })?;
                write_frame(out, kind, &bytes).context("frame")
            })?;
        }
        Ok(())
    }
}

/// `Parcel.obtain()`, `fill`, `marshall()`, `recycle()`: the bytes `fill` wrote.
fn marshal(vm: &Vm, fill: impl FnOnce(JObject) -> Result<()>) -> Result<Vec<u8>> {
    let class = vm.find_class(c"android/os/Parcel")?;
    let obtain = vm.method(class, c"obtain", c"()Landroid/os/Parcel;", true)?;
    let marshall = vm.method(class, c"marshall", c"()[B", false)?;
    let recycle = vm.method(class, c"recycle", c"()V", false)?;
    let parcel = vm.call_static_object(class, obtain, &[], "Parcel.obtain")?;
    ensure!(!parcel.is_null(), "Parcel.obtain returned null");
    let bytes = fill(parcel).and_then(|()| {
        let array = vm.call_object(parcel, marshall, &[], "Parcel.marshall")?;
        vm.bytes(array)
    });
    vm.call_void(parcel, recycle, "Parcel.recycle")?;
    bytes
}
