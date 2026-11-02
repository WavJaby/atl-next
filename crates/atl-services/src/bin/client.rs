// SPDX-License-Identifier: MIT
//! atl-services-client: `xtask test binder`'s client of the M2 services, through binderd and AOSP servicemanager. Prints `client: pass|FAIL <check> <detail>`; exit 0 = all passed.
//! - `app`: the calls an app process makes, as the app uid: getService of each; attachApplication -> bindApplication with the package's ApplicationInfo; finishAttachApplication ->
//!   scheduleTransaction(LaunchActivityItem) of its launcher activity; getDisplayId on the controller it carries; the PackageManager subset (an absent package reads as null);
//!   getContentProvider("settings") and the provider's `call` (null Bundle), an absent authority (null); an unimplemented method (UNKNOWN_TRANSACTION, counted by the services);
//!   an @EnforcePermission method (SECURITY: the hook denies an app).
//! - `system`: the same @EnforcePermission method as the system uid passes the hook and reaches the unimplemented default (UNKNOWN_TRANSACTION).
//! - `codes`: the transaction codes the gate's `service call` uses (from the stubs, so they follow the AIDL).

extern crate atl_parcelables;
extern crate binder;

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use atl_aidl::aidl::android::app::IActivityClientController::IActivityClientController;
use atl_aidl::aidl::android::app::IActivityManager::IActivityManager;
use atl_aidl::aidl::android::app::IApplicationThread::{BnApplicationThread, IApplicationThread};
use atl_aidl::aidl::android::content::pm::IPackageManager::{IPackageManager, transactions};
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::content::pm::ApplicationInfo;
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::java::util::Map;
use atl_services::settings;
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, StatusCode, Strong};
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    App {
        /// The package the services expect to start.
        #[arg(long)]
        package: String,
        /// Its uid (this process's binder uid).
        #[arg(long)]
        uid: i32,
        /// Its launcher activity.
        #[arg(long)]
        activity: String,
        /// The expected start's startSeq.
        #[arg(long)]
        start_seq: i64,
    },
    System,
    Codes,
}

/// How long a oneway callback (bindApplication, scheduleTransaction) may take to arrive: one binder hop, well under 100 ms; 5 s leaves room for a loaded machine.
const CALLBACK: Duration = Duration::from_secs(5);
/// A package nothing installs.
const ABSENT: &str = "com.example.absent";
/// An @EnforcePermission method of IPackageManager outside the M2 subset: getMoveStatus, @EnforcePermission("MOUNT_UNMOUNT_FILESYSTEMS").
const MOVE_ID: i32 = 1;

enum Event {
    Bind(Option<String>, Option<Box<ApplicationInfo>>),
    Launch(Option<ClientTransaction>),
}

/// The app's IApplicationThread: records the two callbacks M2 sends.
struct Thread(Mutex<Sender<Event>>);

impl Interface for Thread {}

impl IApplicationThread for Thread {
    fn r#unimplemented(&self, _method: &'static str) -> Status {
        StatusCode::UNKNOWN_TRANSACTION.into()
    }
    #[allow(clippy::too_many_arguments)]
    fn r#bindApplication(
        &self,
        name: Option<&str>,
        info: Option<&ApplicationInfo>,
        _: Option<&str>,
        _: Option<&str>,
        _: bool,
        _: Option<&atl_parcelables::android::content::pm::ProviderInfoList>,
        _: Option<&atl_parcelables::android::content::ComponentName>,
        _: Option<&atl_parcelables::android::app::ProfilerInfo>,
        _: Option<&atl_parcelables::android::os::Bundle>,
        _: Option<
            &Strong<
                dyn atl_aidl::aidl::android::app::IInstrumentationWatcher::IInstrumentationWatcher,
            >,
        >,
        _: Option<
            &Strong<
                dyn atl_aidl::aidl::android::app::IUiAutomationConnection::IUiAutomationConnection,
            >,
        >,
        _: i32,
        _: bool,
        _: bool,
        _: bool,
        _: bool,
        _: Option<&atl_parcelables::android::content::res::Configuration>,
        _: Option<&atl_parcelables::android::content::res::CompatibilityInfo>,
        _: Option<&Map>,
        _: Option<&atl_parcelables::android::os::Bundle>,
        _: Option<&str>,
        _: Option<&atl_parcelables::android::content::AutofillOptions>,
        _: Option<&atl_parcelables::android::content::ContentCaptureOptions>,
        _: Option<&[i64]>,
        _: Option<&[i64]>,
        _: Option<&atl_parcelables::android::os::SharedMemory>,
        _: Option<&atl_parcelables::java::io::FileDescriptor>,
        _: i64,
        _: i64,
    ) -> binder::Result<()> {
        let tx = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = tx.send(Event::Bind(
            name.map(str::to_owned),
            info.cloned().map(Box::new),
        ));
        Ok(())
    }
    fn r#scheduleTransaction(&self, t: Option<&ClientTransaction>) -> binder::Result<()> {
        let tx = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = tx.send(Event::Launch(t.cloned()));
        Ok(())
    }
}

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            println_out(&format!("client: pass {name} {detail}"));
        } else {
            self.failed += 1;
            println_out(&format!("client: FAIL {name} {detail}"));
        }
    }
}

fn println_out(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn is_unknown_transaction<T>(r: &binder::Result<T>) -> bool {
    matches!(r, Err(s) if s.exception_code() == ExceptionCode::TRANSACTION_FAILED && s.transaction_error() == StatusCode::UNKNOWN_TRANSACTION)
}

fn is_security<T>(r: &binder::Result<T>) -> bool {
    matches!(r, Err(s) if s.exception_code() == ExceptionCode::SECURITY)
}

fn app(package: &str, uid: i32, activity: &str, start_seq: i64) -> Result<usize> {
    let mut r = Report { failed: 0 };
    ProcessState::start_thread_pool();
    let am: Strong<dyn IActivityManager> =
        binder::check_interface("activity").context("getService activity")?;
    let pm: Strong<dyn IPackageManager> =
        binder::check_interface("package").context("getService package")?;
    r.check("getService", true, "activity, package");

    let (tx, rx): (Sender<Event>, Receiver<Event>) = channel();
    let thread = BnApplicationThread::new_binder(Thread(Mutex::new(tx)), BinderFeatures::default());

    am.attachApplication(Some(&thread), start_seq)
        .context("attachApplication")?;
    match rx.recv_timeout(CALLBACK) {
        Ok(Event::Bind(name, Some(info))) => r.check(
            "bindApplication",
            name.as_deref() == Some(package)
                && info.item.package_name.as_deref() == Some(package)
                && info.uid == uid,
            format!(
                "process {name:?}, package {:?}, uid {}",
                info.item.package_name, info.uid
            ),
        ),
        _ => r.check(
            "bindApplication",
            false,
            "no bindApplication with an ApplicationInfo",
        ),
    }

    am.finishAttachApplication(start_seq, 0)
        .context("finishAttachApplication")?;
    let launch = match rx.recv_timeout(CALLBACK) {
        Ok(Event::Launch(Some(t))) => t.items.into_iter().next().flatten(),
        _ => None,
    };
    match launch {
        Some(ClientTransactionItem::LaunchActivity(item)) => {
            let name = item
                .info
                .as_ref()
                .and_then(|i| i.component.item.name.clone());
            let component = item.intent.as_ref().and_then(|i| i.component.clone());
            r.check(
                "scheduleTransaction",
                name.as_deref() == Some(activity)
                    && component.is_some_and(|c| c.package == package && c.class == activity)
                    && item.activity_token.is_some(),
                format!("LaunchActivityItem {name:?}"),
            );
            match item
                .activity_client_controller
                .map(|b| b.into_interface::<dyn IActivityClientController>())
            {
                Some(Ok(c)) => {
                    let id = c.getDisplayId(item.activity_token.as_ref());
                    r.check("getDisplayId", matches!(id, Ok(0)), format!("{id:?}"));
                }
                other => r.check(
                    "getDisplayId",
                    false,
                    format!("no controller: {:?}", other.map(|o| o.err())),
                ),
            }
        }
        _ => r.check("scheduleTransaction", false, "no LaunchActivityItem"),
    }

    let ai = pm.getApplicationInfo(Some(package), 0, 0);
    r.check(
        "getApplicationInfo",
        matches!(&ai, Ok(Some(i)) if i.uid == uid),
        format!("{:?}", ai.map(|i| i.map(|i| i.uid))),
    );
    let absent = pm.getApplicationInfo(Some(ABSENT), 0, 0);
    r.check(
        "getApplicationInfo:null",
        matches!(absent, Ok(None)),
        format!("{ABSENT}: {:?}", absent.map(|i| i.is_some())),
    );
    let pi = pm.getPackageInfo(Some(package), 0, 0);
    r.check(
        "getPackageInfo",
        matches!(&pi, Ok(Some(p)) if p.package_name.as_deref() == Some(package) && p.application_info.as_ref().is_some_and(|a| a.uid == uid)),
        format!("{:?}", pi.map(|p| p.map(|p| p.package_name))),
    );
    let feature = pm.hasSystemFeature(Some("android.hardware.touchscreen"), 0);
    r.check(
        "hasSystemFeature",
        matches!(feature, Ok(false)),
        format!("{feature:?}"),
    );
    let pkgs = pm.getPackagesForUid(uid);
    r.check(
        "getPackagesForUid",
        matches!(&pkgs, Ok(Some(v)) if v == &[Some(package.to_owned())]),
        format!("{pkgs:?}"),
    );
    let map = Map {
        entries: vec![(Some("/data/app/base.apk".into()), Some("PCL[]".into()))],
    };
    let dex = pm.notifyDexLoad(Some(package), Some(&map), Some("x86_64"));
    r.check("notifyDexLoad", dex.is_ok(), format!("{dex:?}"));

    let holder = am.getContentProvider(Some(&thread), Some(package), Some("settings"), 0, true);
    match holder {
        Ok(Some(h)) if h.info.authority.as_deref() == Some("settings") => {
            r.check("getContentProvider", true, "settings");
            match h.provider {
                Some(b) => {
                    let called = call_settings(b);
                    r.check(
                        "IContentProvider.call",
                        matches!(called, Ok((0, -1))),
                        format!("(exception, bundle length) {called:?}"),
                    );
                }
                None => r.check("IContentProvider.call", false, "no provider binder"),
            }
        }
        other => r.check(
            "getContentProvider",
            false,
            format!("{:?}", other.map(|h| h.map(|h| h.info.authority))),
        ),
    }
    let none = am.getContentProvider(
        Some(&thread),
        Some(package),
        Some("com.example.absent"),
        0,
        true,
    );
    r.check(
        "getContentProvider:null",
        matches!(none, Ok(None)),
        format!("{:?}", none.map(|h| h.is_some())),
    );

    let limit = am.getProcessLimit();
    r.check(
        "unimplemented",
        is_unknown_transaction(&limit),
        format!("getProcessLimit: {limit:?}"),
    );
    let denied = pm.getMoveStatus(MOVE_ID);
    r.check(
        "enforce:deny",
        is_security(&denied),
        format!("getMoveStatus as uid {uid}: {denied:?}"),
    );
    Ok(r.failed)
}

/// IContentProvider.call on the settings provider (ContentProviderNative's wire format: interface token, AttributionSource, authority, method, arg, extras); returns the reply's
/// exception status and Bundle length (-1 = null). The provider reads none of the arguments, so the AttributionSource is not written.
fn call_settings(b: binder::SpIBinder) -> Result<(i32, i32)> {
    use binder::binder_impl::IBinderInternal;
    let p: Strong<dyn settings::IContentProvider> = b.into_interface()?;
    let binder = p.as_binder();
    let mut data = binder.prepare_transact()?;
    data.write("settings")?;
    data.write("GET_system")?;
    data.write("font_scale")?;
    data.write(&-1i32)?;
    let reply = binder.submit_transact(settings::CALL_TRANSACTION, data, 0)?;
    Ok((reply.read()?, reply.read()?))
}

fn system() -> Result<usize> {
    let mut r = Report { failed: 0 };
    let pm: Strong<dyn IPackageManager> =
        binder::check_interface("package").context("getService package")?;
    let allowed = pm.getMoveStatus(MOVE_ID);
    r.check(
        "enforce:allow",
        is_unknown_transaction(&allowed),
        format!("getMoveStatus as the system uid: {allowed:?}"),
    );
    Ok(r.failed)
}

fn main() -> Result<()> {
    let failed = match Args::parse().mode {
        Mode::App {
            package,
            uid,
            activity,
            start_seq,
        } => app(&package, uid, &activity, start_seq)?,
        Mode::System => system()?,
        Mode::Codes => {
            println_out(&format!(
                "code getApplicationInfo {}",
                transactions::r#getApplicationInfo
            ));
            println_out(&format!(
                "code hasSystemFeature {}",
                transactions::r#hasSystemFeature
            ));
            0
        }
    };
    if failed > 0 {
        bail!("{failed} checks failed");
    }
    Ok(())
}
