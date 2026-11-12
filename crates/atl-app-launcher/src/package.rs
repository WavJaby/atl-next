// SPDX-License-Identifier: MIT
//! Installer slice M3: the full package parse on AOSP's install path, `ParsingPackageUtils.parsePackage` over the package dir (the cluster branch, as InstallPackageHelper.java:1513 reaches it through PackageParser2),
//! then `PackageInfoCommonUtils.generate` (AOSP's ParsedPackage -> PackageInfo outside PMS state), read back field by field over JNI into the installer's output lines.
//! The ParsingPackageUtils.Callback is the api-impl forwarder `org.atl.installer.ParseCallback` (on the VM's class path); every answer it gives is decided here ([`Device`]).

use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};

use crate::ffi::{JObject, JValue, Vm};

/// PackageParser2's install flags (InstallPackageHelper.java:1502-1505): PARSE_CHATTY (1 << 31) | PARSE_ENFORCE_CODE (1 << 6) (ParsingPackageUtils.java:243, :252); not on external storage.
/// PARSE_ENFORCE_CODE is read by ScanPackageUtils.assertCodePolicy (InstallPackageHelper.java:4626), services.jar, not by the parser.
const PARSE_FLAGS: i32 = i32::MIN | 1 << 6;
/// PackageManager GET_ACTIVITIES | GET_RECEIVERS | GET_SERVICES | GET_PROVIDERS | GET_PERMISSIONS | MATCH_DIRECT_BOOT_UNAWARE | MATCH_DIRECT_BOOT_AWARE (PackageManager.java):
/// every component (generate keeps one only if a direct-boot flag matches it, PackageInfoCommonUtils.isMatch:630-644) and the permission arrays.
const INFO_FLAGS: i64 = 0x1 | 0x2 | 0x4 | 0x8 | 0x1000 | 0x0004_0000 | 0x0008_0000;
/// ApplicationInfo.FLAG_EXTRACT_NATIVE_LIBS (ApplicationInfo.java).
const FLAG_EXTRACT_NATIVE_LIBS: i32 = 1 << 28;
/// The user generate fills per-user fields for: the only one (UserHandle.USER_SYSTEM).
const USER: i32 = 0;

/// The device-side answers the parse asks for: PMS's in system_server, here the installer's (each one an owner decision, listed in the M3 state note).
pub struct Device {
    /// `hasFeature` (PMS mPackageParserCallback -> hasSystemFeature(feature, 0), PackageManagerService.java:2026): the features this device declares, which atld's PackageManager must answer identically.
    pub features: Vec<String>,
}

/// One parse's result, as output lines (`key value...`; installer.rs documents them).
pub fn parse(vm: &Vm, dir: &Path, device: &Device) -> Result<Vec<String>> {
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
    Walk::new(vm).lines(info, pkg)
}

/// The read-back of a generated PackageInfo into output lines.
struct Walk<'vm> {
    vm: &'vm Vm,
    out: Vec<String>,
}

impl<'vm> Walk<'vm> {
    fn new(vm: &'vm Vm) -> Self {
        Walk {
            vm,
            out: Vec::new(),
        }
    }

    /// The object field `name` (type `sig`) of `obj`, whose class is `class`.
    fn obj(
        &self,
        class: &std::ffi::CStr,
        obj: JObject,
        name: &std::ffi::CStr,
        sig: &std::ffi::CStr,
    ) -> Result<JObject> {
        let c = self.vm.find_class(class)?;
        let f = self.vm.field(c, name, sig)?;
        self.vm.object_field(obj, f)
    }

    fn str(
        &self,
        class: &std::ffi::CStr,
        obj: JObject,
        name: &std::ffi::CStr,
    ) -> Result<Option<String>> {
        let s = self.obj(class, obj, name, c"Ljava/lang/String;")?;
        self.vm.string(s)
    }

    fn int(&self, class: &std::ffi::CStr, obj: JObject, name: &std::ffi::CStr) -> Result<i32> {
        let c = self.vm.find_class(class)?;
        let f = self.vm.field(c, name, c"I")?;
        self.vm.int_field(obj, f)
    }

    fn bool(&self, class: &std::ffi::CStr, obj: JObject, name: &std::ffi::CStr) -> Result<bool> {
        let c = self.vm.find_class(class)?;
        let f = self.vm.field(c, name, c"Z")?;
        self.vm.bool_field(obj, f)
    }

    /// The elements of an object array; empty for null.
    fn elements(&self, array: JObject) -> Result<Vec<JObject>> {
        if array.is_null() {
            return Ok(Vec::new());
        }
        (0..self.vm.array_len(array)?)
            .map(|i| self.vm.array_get(array, i))
            .collect()
    }

    fn strings(&self, array: JObject) -> Result<Vec<String>> {
        self.elements(array)?
            .into_iter()
            .map(|s| self.vm.string(s)?.context("null string element"))
            .collect()
    }

    fn line(&mut self, text: String) {
        self.out.push(text);
    }

    fn lines(mut self, info: JObject, pkg: JObject) -> Result<Vec<String>> {
        const PI: &std::ffi::CStr = c"android/content/pm/PackageInfo";
        const AI: &std::ffi::CStr = c"android/content/pm/ApplicationInfo";
        let vm = self.vm;
        let name = self
            .str(PI, info, c"packageName")?
            .context("no packageName")?;
        self.line(format!("package {name}"));
        let pi = vm.find_class(PI)?;
        let long_version = vm.method(pi, c"getLongVersionCode", c"()J", false)?;
        let version = vm.call_long(info, long_version, "PackageInfo.getLongVersionCode")?;
        self.line(format!("version-code {version}"));
        let version_name = self.str(PI, info, c"versionName")?;
        self.line(format!(
            "version-name {}",
            version_name.as_deref().unwrap_or("none")
        ));
        let app = self.obj(
            PI,
            info,
            c"applicationInfo",
            c"Landroid/content/pm/ApplicationInfo;",
        )?;
        ensure!(!app.is_null(), "PackageInfo without applicationInfo");
        let min = self.int(AI, app, c"minSdkVersion")?;
        let target = self.int(AI, app, c"targetSdkVersion")?;
        self.line(format!("min-sdk {min}"));
        self.line(format!("target-sdk {target}"));
        let class = self.str(AI, app, c"className")?;
        self.line(format!("app-class {}", class.as_deref().unwrap_or("none")));
        let flags = self.int(AI, app, c"flags")?;
        self.line(format!("app-flags {flags:#x}"));
        self.line(format!(
            "extract-native-libs {}",
            flags & FLAG_EXTRACT_NATIVE_LIBS != 0
        ));
        let process = self.str(AI, app, c"processName")?;
        self.line(format!(
            "app-process {}",
            process.as_deref().unwrap_or("none")
        ));
        let theme = self.int(AI, app, c"theme")?;
        self.line(format!("app-theme {theme:#x}"));

        let requested = self.obj(PI, info, c"requestedPermissions", c"[Ljava/lang/String;")?;
        for p in self.strings(requested)? {
            self.line(format!("uses-permission {p}"));
        }
        let permissions = self.obj(
            PI,
            info,
            c"permissions",
            c"[Landroid/content/pm/PermissionInfo;",
        )?;
        for p in self.elements(permissions)? {
            let pi_class = c"android/content/pm/PermissionInfo";
            let name = self
                .str(c"android/content/pm/PackageItemInfo", p, c"name")?
                .context("unnamed permission")?;
            let level = self.int(pi_class, p, c"protectionLevel")?;
            self.line(format!("permission {name} protection-level={level:#x}"));
        }

        let activity_array = c"[Landroid/content/pm/ActivityInfo;";
        for (kind, field, sig) in [
            ("activity", c"activities", activity_array),
            ("receiver", c"receivers", activity_array),
            ("service", c"services", c"[Landroid/content/pm/ServiceInfo;"),
            (
                "provider",
                c"providers",
                c"[Landroid/content/pm/ProviderInfo;",
            ),
        ] {
            let array = self.obj(PI, info, field, sig)?;
            for c in self.elements(array)? {
                let line = vm.frame(|| self.component(kind, c))?;
                self.line(line);
            }
        }
        for (kind, getter) in [
            ("activity", c"getActivities"),
            ("receiver", c"getReceivers"),
            ("service", c"getServices"),
            ("provider", c"getProviders"),
        ] {
            let lines = vm.frame(|| self.filters(pkg, kind, getter))?;
            self.out.extend(lines);
        }
        Ok(self.out)
    }

    /// `<kind> <name> key=value...`: the ComponentInfo fields services read, plus the kind's own.
    fn component(&self, kind: &str, c: JObject) -> Result<String> {
        const CI: &std::ffi::CStr = c"android/content/pm/ComponentInfo";
        let name = self
            .str(c"android/content/pm/PackageItemInfo", c, c"name")?
            .context("unnamed component")?;
        let exported = self.bool(CI, c, c"exported")?;
        let enabled = self.bool(CI, c, c"enabled")?;
        let process = self.str(CI, c, c"processName")?;
        let mut line = format!(
            "{kind} {name} exported={exported} enabled={enabled} process={}",
            process.as_deref().unwrap_or("none")
        );
        match kind {
            "activity" | "receiver" => {
                const AC: &std::ffi::CStr = c"android/content/pm/ActivityInfo";
                let launch_mode = self.int(AC, c, c"launchMode")?;
                let theme = self.int(AC, c, c"theme")?;
                let config = self.int(AC, c, c"configChanges")?;
                let orientation = self.int(AC, c, c"screenOrientation")?;
                let flags = self.int(AC, c, c"flags")?;
                let permission = self.str(AC, c, c"permission")?;
                line += &format!(
                    " launch-mode={launch_mode} theme={theme:#x} config-changes={config:#x} orientation={orientation} flags={flags:#x} permission={}",
                    permission.as_deref().unwrap_or("none")
                );
            }
            "service" => {
                let permission = self.str(c"android/content/pm/ServiceInfo", c, c"permission")?;
                line += &format!(" permission={}", permission.as_deref().unwrap_or("none"));
            }
            "provider" => {
                const PR: &std::ffi::CStr = c"android/content/pm/ProviderInfo";
                let authority = self.str(PR, c, c"authority")?;
                let read = self.str(PR, c, c"readPermission")?;
                let write = self.str(PR, c, c"writePermission")?;
                line += &format!(
                    " authority={} read-permission={} write-permission={}",
                    authority.as_deref().unwrap_or("none"),
                    read.as_deref().unwrap_or("none"),
                    write.as_deref().unwrap_or("none")
                );
            }
            _ => bail!("unknown component kind {kind}"),
        }
        Ok(line)
    }

    /// `filter <kind> <component> actions=a,b categories=c schemes=s types=t`, one per intent filter of each component of `kind` in the ParsedPackage (PackageInfo carries none).
    fn filters(&self, pkg: JObject, kind: &str, getter: &std::ffi::CStr) -> Result<Vec<String>> {
        let vm = self.vm;
        let android_package = vm.find_class(c"com/android/server/pm/pkg/AndroidPackage")?;
        let get = vm.method(android_package, getter, c"()Ljava/util/List;", false)?;
        let list = vm.call_object(pkg, get, &[], "AndroidPackage component list")?;
        let list_class = vm.find_class(c"java/util/List")?;
        let size = vm.method(list_class, c"size", c"()I", false)?;
        let get_at = vm.method(list_class, c"get", c"(I)Ljava/lang/Object;", false)?;
        let component = vm.find_class(c"com/android/internal/pm/pkg/component/ParsedComponent")?;
        let get_name = vm.method(component, c"getName", c"()Ljava/lang/String;", false)?;
        let get_intents = vm.method(component, c"getIntents", c"()Ljava/util/List;", false)?;
        let intent_info =
            vm.find_class(c"com/android/internal/pm/pkg/component/ParsedIntentInfo")?;
        let get_filter = vm.method(
            intent_info,
            c"getIntentFilter",
            c"()Landroid/content/IntentFilter;",
            false,
        )?;
        let filter_class = vm.find_class(c"android/content/IntentFilter")?;
        let parts = [
            ("actions", c"countActions", c"getAction"),
            ("categories", c"countCategories", c"getCategory"),
            ("schemes", c"countDataSchemes", c"getDataScheme"),
            ("types", c"countDataTypes", c"getDataType"),
        ]
        .into_iter()
        .map(|(key, count, get)| {
            Ok((
                key,
                vm.method(filter_class, count, c"()I", false)?,
                vm.method(filter_class, get, c"(I)Ljava/lang/String;", false)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::new();
        let n = vm.call_int(list, size, &[], "List.size")?;
        for i in 0..n {
            let lines = vm.frame(|| {
                let c = vm.call_object(list, get_at, &[JValue { i }], "List.get")?;
                let name = vm.call_object(c, get_name, &[], "ParsedComponent.getName")?;
                let name = vm.string(name)?.context("unnamed component")?;
                let intents = vm.call_object(c, get_intents, &[], "ParsedComponent.getIntents")?;
                let mut lines = Vec::new();
                for j in 0..vm.call_int(intents, size, &[], "List.size")? {
                    let info = vm.call_object(intents, get_at, &[JValue { i: j }], "List.get")?;
                    let filter =
                        vm.call_object(info, get_filter, &[], "ParsedIntentInfo.getIntentFilter")?;
                    let mut line = format!("filter {kind} {name}");
                    for (key, count, get) in &parts {
                        let mut values = Vec::new();
                        for k in 0..vm.call_int(filter, *count, &[], "IntentFilter count")? {
                            let v = vm.call_object(
                                filter,
                                *get,
                                &[JValue { i: k }],
                                "IntentFilter get",
                            )?;
                            values.push(vm.string(v)?.context("null filter value")?);
                        }
                        line += &format!(" {key}={}", values.join(","));
                    }
                    lines.push(line);
                }
                Ok(lines)
            })?;
            out.extend(lines);
        }
        Ok(out)
    }
}
