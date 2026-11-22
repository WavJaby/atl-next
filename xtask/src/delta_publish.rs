// SPDX-License-Identifier: MIT
//! Publishing a ram build dir (delta.rs) as a base: the overlay's merged view materialised on the pool without mounting it = a reflink copy of the lower with the upper applied on top ([`apply`]).
//! Upper semantics (kernel overlayfs, as mounted by delta::mount_options: `userxattr`, which forces `redirect_dir=nofollow` and no metacopy; probed on 6.6 2026-11-22): a char device 0:0 is a whiteout (the lower entry is gone); a dir with `user.overlay.opaque=y` hides the lower dir under it (replaced, not merged); any other dir merges; files and symlinks replace the lower entry whole (copy-up copies the full file).
//! `x` as the opaque value (kernel >= 6.7) only marks a dir holding xattr whiteouts: a regular file with `overlay.whiteout`. Both forms are honoured. A redirect or metacopy xattr means a mount this code does not model: refused, never guessed, so a wrong base is never published.
//! Overlay's own xattrs (`origin`, `impure`, `opaque`, ...) never reach the base: a base is a lower later, where a stale one would change the merge.
//! Kept per entry, because builds read them: content, mode, mtime and atime to the nanosecond (ninja and cargo compare mtimes; a shifted mtime rebuilds every child), symlink targets, other xattrs, hard links among upper files.

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// What an upper entry does to the lower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Deletes the lower entry.
    Whiteout,
    /// Replaces the lower entry with this dir and its upper contents only.
    OpaqueDir,
    /// Merges into the lower dir (or replaces a lower non-dir).
    Dir,
    /// Replaces the lower entry with a copy (regular file or symlink).
    Copy,
    /// Left out of the result: top-level names the caller excludes.
    Skip,
}

/// Classifies one upper entry ([`Kind`]); `rel` is its path below the upper root.
pub type Classify<'a> = dyn Fn(&Path, &Path, &std::fs::Metadata) -> Result<Kind> + 'a;

/// The overlay xattr namespaces: `user.` under `userxattr` (unprivileged mounts, delta.rs), `trusted.` otherwise.
const OVERLAY_PREFIXES: [&str; 2] = ["user.overlay.", "trusted.overlay."];

fn overlay_attr(name: &str) -> Option<&str> {
    OVERLAY_PREFIXES.iter().find_map(|p| name.strip_prefix(p))
}

/// Xattr names of `path`, not following a symlink. A filesystem without xattrs has none.
fn xattr_names(path: &Path) -> Result<Vec<String>> {
    let mut buf = vec![0u8; 4096];
    loop {
        match rustix::fs::llistxattr(path, &mut buf[..]) {
            Ok(n) => {
                return Ok(buf[..n]
                    .split(|b| *b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect());
            }
            Err(rustix::io::Errno::RANGE) => buf.resize(buf.len() * 4, 0),
            Err(rustix::io::Errno::NOTSUP) => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("listxattr {}", path.display())),
        }
    }
}

fn xattr(path: &Path, name: &str) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    loop {
        match rustix::fs::lgetxattr(path, name, &mut buf[..]) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            Err(rustix::io::Errno::RANGE) => buf.resize(buf.len() * 16, 0),
            Err(e) => return Err(e).with_context(|| format!("getxattr {name} {}", path.display())),
        }
    }
}

/// The production classifier: the kernel's rules above; `skip` = top-level names left out.
pub fn classify<'a>(
    skip: &'a [&'a str],
) -> impl Fn(&Path, &Path, &std::fs::Metadata) -> Result<Kind> + 'a {
    move |path, rel, meta| {
        if rel.parent() == Some(Path::new("")) && rel.to_str().is_some_and(|r| skip.contains(&r)) {
            return Ok(Kind::Skip);
        }
        let ft = meta.file_type();
        if ft.is_char_device() {
            if meta.rdev() == 0 {
                return Ok(Kind::Whiteout);
            }
            bail!(
                "{}: a character device other than an overlay whiteout (0:0)",
                path.display()
            );
        }
        let mut opaque = false;
        for name in xattr_names(path)? {
            match overlay_attr(&name) {
                Some("opaque") if ft.is_dir() => match xattr(path, &name)?.as_slice() {
                    b"y" => opaque = true,
                    // kernel >= 6.7: the dir holds xattr whiteouts, it is not opaque
                    b"x" => {}
                    v => bail!(
                        "{}: {name} = {:?}: unknown overlay opaque value",
                        path.display(),
                        String::from_utf8_lossy(v)
                    ),
                },
                Some("whiteout") if ft.is_file() => return Ok(Kind::Whiteout),
                Some(a @ ("redirect" | "metacopy")) => bail!(
                    "{}: overlay {a} xattr: the upper was written by a mount with redirect_dir or metacopy, which publishing does not model (delta::mount_options mounts with userxattr, which disables both)",
                    path.display()
                ),
                _ => {}
            }
        }
        if ft.is_dir() {
            Ok(if opaque { Kind::OpaqueDir } else { Kind::Dir })
        } else if ft.is_file() || ft.is_symlink() || ft.is_fifo() || ft.is_socket() {
            // fifos and sockets too: gates leave them in build dirs (`app/bus/bus`, a session bus), and a pool publish (`cp -a`) copies the node
            Ok(Kind::Copy)
        } else {
            bail!(
                "{}: a block device in an upper: not publishable",
                path.display()
            )
        }
    }
}

/// What [`apply`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Regular files copied (hard links to an earlier copy not counted) and their bytes.
    pub files: u64,
    pub bytes: u64,
    pub links: u64,
    pub symlinks: u64,
    /// Fifo and socket nodes.
    pub nodes: u64,
    pub dirs: u64,
    pub opaque: u64,
    pub whiteouts: u64,
}

impl Applied {
    pub fn line(&self) -> String {
        format!(
            "{} files ({:.2} GiB), {} hard links, {} symlinks, {} fifo/socket nodes, {} dirs ({} opaque), {} whiteouts",
            self.files,
            self.bytes as f64 / (1u64 << 30) as f64,
            self.links,
            self.symlinks,
            self.nodes,
            self.dirs,
            self.opaque,
            self.whiteouts
        )
    }
}

/// Removes whatever is at `path` (a dir recursively); nothing there is fine.
fn remove_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
    .with_context(|| format!("removing {}", path.display()))
}

fn timestamps(meta: &std::fs::Metadata) -> rustix::fs::Timestamps {
    let ts = |s: i64, ns: i64| rustix::fs::Timespec {
        tv_sec: s,
        tv_nsec: ns as _,
    };
    rustix::fs::Timestamps {
        last_access: ts(meta.atime(), meta.atime_nsec()),
        last_modification: ts(meta.mtime(), meta.mtime_nsec()),
    }
}

/// Gives `dst` the xattrs (overlay's own left out), mode and times of `src`; a symlink gets xattrs and times only.
fn copy_meta(src: &Path, dst: &Path, meta: &std::fs::Metadata) -> Result<()> {
    for name in xattr_names(src)? {
        if overlay_attr(&name).is_some() {
            continue;
        }
        let v = xattr(src, &name)?;
        rustix::fs::lsetxattr(dst, name.as_str(), &v, rustix::fs::XattrFlags::empty())
            .with_context(|| format!("setxattr {name} {}", dst.display()))?;
    }
    if !meta.file_type().is_symlink() {
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))
            .with_context(|| format!("chmod {}", dst.display()))?;
    }
    rustix::fs::utimensat(
        rustix::fs::CWD,
        dst,
        &timestamps(meta),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .with_context(|| format!("setting the times of {}", dst.display()))
}

/// Applies overlay upper `upper` onto `dst`, a copy of its lower: afterwards `dst` holds the merged view. `dst`'s own mode and times are left to the caller (a base's top dir mtime ranks it).
pub fn apply(upper: &Path, dst: &Path, classify: &Classify<'_>) -> Result<Applied> {
    let mut done = Applied::default();
    let mut linked: HashMap<(u64, u64), PathBuf> = HashMap::new();
    apply_dir(upper, dst, Path::new(""), classify, &mut done, &mut linked)?;
    Ok(done)
}

/// Publishes lower + upper as the absent dir `dst`: a copy of `lower` (out::copy_tree: reflink where the pool allows, top-level `skip` left out) under a temp name, `upper` applied ([`classify`]: `skip` and the build dir's `.base` left out), `finish` run on it (the caller's extra files), its mtime set to now (out::bases ranks bases by it), renamed into place last.
/// So an interrupted publish leaves only a `.`-prefixed temp dir, which out::bases never takes for a base, and a `dst` that appeared meanwhile is never overwritten (rename onto a non-empty dir fails).
pub fn publish(
    lower: &Path,
    upper: &Path,
    dst: &Path,
    skip: &[&str],
    finish: &dyn Fn(&Path) -> Result<()>,
) -> Result<(crate::out::Tier, Applied)> {
    let parent = dst.parent().context("publish target has no parent")?;
    let name = dst
        .file_name()
        .context("publish target has no name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.pub-{}", std::process::id()));
    remove_any(&tmp)?;
    let mut leave_out = skip.to_vec();
    leave_out.push(crate::out::BASE_FILE);
    let made = (|| {
        let tier = crate::out::copy_tree(lower, &tmp, None, skip)?;
        let done = apply(upper, &tmp, &classify(&leave_out))?;
        finish(&tmp)?;
        std::fs::File::open(&tmp)
            .and_then(|d| d.set_modified(std::time::SystemTime::now()))
            .with_context(|| format!("setting the mtime of {}", tmp.display()))?;
        std::fs::rename(&tmp, dst)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), dst.display()))?;
        Ok((tier, done))
    })();
    if made.is_err() {
        let _ = remove_any(&tmp);
    }
    made
}

fn apply_dir(
    upper: &Path,
    dst: &Path,
    rel: &Path,
    classify: &Classify<'_>,
    done: &mut Applied,
    linked: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    let from = upper.join(rel);
    let mut names: Vec<_> = std::fs::read_dir(&from)
        .with_context(|| format!("reading {}", from.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    for name in names {
        let r = rel.join(&name);
        let (src, to) = (upper.join(&r), dst.join(&r));
        let meta = std::fs::symlink_metadata(&src)
            .with_context(|| format!("reading {}", src.display()))?;
        match classify(&src, &r, &meta)? {
            Kind::Skip => {}
            Kind::Whiteout => {
                remove_any(&to)?;
                done.whiteouts += 1;
            }
            kind @ (Kind::Dir | Kind::OpaqueDir) => {
                let keep =
                    kind == Kind::Dir && std::fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir());
                if !keep {
                    remove_any(&to)?;
                    std::fs::create_dir(&to)
                        .with_context(|| format!("creating {}", to.display()))?;
                }
                apply_dir(upper, dst, &r, classify, done, linked)?;
                // after the children: writing them moved the dir's mtime
                copy_meta(&src, &to, &meta)?;
                done.dirs += 1;
                done.opaque += u64::from(kind == Kind::OpaqueDir);
            }
            Kind::Copy => {
                // unlink first, never write through: the lower copy's other hard links keep the old content, as in the merged view (copy-up breaks lower links)
                remove_any(&to)?;
                if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(&src)
                        .with_context(|| format!("reading {}", src.display()))?;
                    std::os::unix::fs::symlink(&target, &to)
                        .with_context(|| format!("linking {}", to.display()))?;
                    done.symlinks += 1;
                } else if meta.file_type().is_fifo() || meta.file_type().is_socket() {
                    // the node only (unprivileged: mknod needs no capability for these types)
                    rustix::fs::mknodat(
                        rustix::fs::CWD,
                        &to,
                        rustix::fs::FileType::from_raw_mode(meta.mode()),
                        rustix::fs::Mode::from_raw_mode(meta.mode() & 0o7777),
                        0,
                    )
                    .with_context(|| format!("mknod {}", to.display()))?;
                    done.nodes += 1;
                } else if let Some(first) = (meta.nlink() > 1)
                    .then(|| linked.get(&(meta.dev(), meta.ino())))
                    .flatten()
                {
                    std::fs::hard_link(first, &to).with_context(|| {
                        format!("linking {} -> {}", to.display(), first.display())
                    })?;
                    done.links += 1;
                    continue;
                } else {
                    std::fs::copy(&src, &to).with_context(|| {
                        format!("copying {} -> {}", src.display(), to.display())
                    })?;
                    if meta.nlink() > 1 {
                        linked.insert((meta.dev(), meta.ino()), to.clone());
                    }
                    done.files += 1;
                    done.bytes += meta.len();
                }
                copy_meta(&src, &to, &meta)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// One line per entry below `.`: dirs `path d mode mtime`, others `path type mode mtime size target`, then a checksum per regular file. mtime to the nanosecond (`%T@`); a dir's size is filesystem-specific and left out; the root is the caller's.
    const LIST: &str = "cd \"$1\" && { find . -mindepth 1 \\( -type d -printf '%P d %m %T@\\n' \\) -o -printf '%P %y %m %T@ %s %l\\n'; find . -type f -exec cksum {} +; } | LC_ALL=C sort > \"$2\"";

    /// What a build does to a base, each change an overlay case: append + old mtime (copy-up), chmod, delete a file and a dir (whiteouts), rename a file (copy-up + whiteout) and a dir (redirect_dir=nofollow: mv copies, then whiteout), replace a dir and a nested dir (opaque), a file by a dir and a dir by a file, retarget a symlink, modify one name of a lower hard-link pair, new dirs with an upper hard link, an old dir mtime set after its children.
    const BUILD: &str = "set -e; cd \"$1\"
echo more >> a; touch -d 2001-02-03T04:05:06.123456789 a
chmod 700 keep
rm d/x; echo n > d/new; mv d/y d/z
rm -rf gone
rm -rf rep && mkdir rep && echo r2 > rep/n
rm -rf d/sub && mkdir d/sub && echo s2 > d/sub/t
rm f2d && mkdir f2d && echo in > f2d/in
rm -rf d2f && echo now-a-file > d2f
ln -sfn new-target s
echo changed >> h1
mv mvd mvd2
mkdir -p newdir/sub && echo f > newdir/sub/f && ln newdir/sub/f newdir/hl
mkfifo newdir/pipe
touch -d 2003-01-01 newdir
";

    fn sh(script: &str, args: &[&Path]) {
        let ok = Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .args(args)
            .status()
            .expect("sh")
            .success();
        assert!(ok, "{script}");
    }

    /// Runs `script` with `args` as uid 0 of a fresh user + mount namespace (the delta.rs stage-mount setup).
    fn in_userns(script: &str, args: &[&Path]) {
        let ok = Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--propagation",
                "private",
                "--",
                "sh",
                "-c",
            ])
            .arg(script)
            .arg("sh")
            .args(args)
            .status()
            .expect("unshare")
            .success();
        assert!(ok, "{script}");
    }

    struct Fixture {
        root: PathBuf,
        store: PathBuf,
        lower: PathBuf,
        upper: PathBuf,
        /// The merged view as the build saw it (listed inside the rw mount).
        merged: String,
    }

    /// Removes `d` whole: overlay leaves its work dir mode 000.
    fn clear(d: &Path) {
        if d.exists() {
            let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(d).status();
            let _ = std::fs::remove_dir_all(d);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            clear(&self.root);
            clear(&self.store);
        }
    }

    fn fixture(tag: &str) -> Fixture {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("xtask-publish-{tag}-{pid}"));
        let store = Path::new("/dev/shm").join(format!("xtask-publish-{tag}-{pid}"));
        clear(&root);
        clear(&store);
        let lower = root.join("lower");
        let (upper, work, mnt) = (store.join("upper"), store.join("work"), root.join("mnt"));
        for d in [&lower, &upper, &work, &mnt] {
            std::fs::create_dir_all(d).unwrap();
        }
        sh(
            "set -e; cd \"$1\"; mkdir -p d/sub rep gone/deep d2f mvd; echo a > a; echo k > keep; echo x > d/x; echo y > d/y; echo s > d/sub/s; echo r > rep/r; echo g > gone/deep/g; echo f > f2d; echo i > d2f/i; echo m > mvd/m; ln -s old-target s; echo h > h1; ln h1 h2; touch -d 1999-12-31T23:59:59.5 a d/sub/s d rep",
            &[&lower],
        );
        let list = root.join("merged.list");
        let opts = format!(
            "lowerdir={},upperdir={},workdir={},userxattr",
            lower.display(),
            upper.display(),
            work.display()
        );
        let (build, lister) = (root.join("build.sh"), root.join("list.sh"));
        std::fs::write(&build, BUILD).unwrap();
        std::fs::write(&lister, LIST).unwrap();
        in_userns(
            "set -e; mount -t overlay overlay -o \"$1\" \"$2\"; sh \"$4\" \"$2\"; sh \"$5\" \"$2\" \"$3\"",
            &[Path::new(&opts), &mnt, &list, &build, &lister],
        );
        let merged = std::fs::read_to_string(&list).unwrap();
        Fixture {
            root,
            store,
            lower,
            upper,
            merged,
        }
    }

    fn listing(f: &Fixture, dir: &Path, name: &str) -> String {
        let list = f.root.join(name);
        sh(LIST, &[dir, &list]);
        std::fs::read_to_string(&list).unwrap()
    }

    /// Gate: publish(lower, upper) equals the merged view the build saw, entry by entry (type, mode, mtime ns, size, link target, content); the expected listing is the kernel's merge, taken in the mount, independent of this code.
    /// Also the read-only oracle the runtime gate uses on real builds (lowerdir=upper:lower, no upper) gives the same listing.
    #[test]
    fn publish_equals_the_merged_view() {
        let f = fixture("gate");
        for case in [
            "rep/n",
            "d/sub/t",
            "f2d/in",
            "d2f now-a-file",
            "newdir/hl",
            "mvd2/m",
        ] {
            let path = case.split(' ').next().unwrap();
            assert!(
                f.merged.contains(path),
                "fixture lacks {case}: {}",
                f.merged
            );
        }
        assert!(
            !f.merged.contains("gone") && !f.merged.contains("d/x ") && !f.merged.contains("rep/r")
        );
        // the upper holds what this test exists for: whiteouts and an opaque dir
        let wh = std::fs::symlink_metadata(f.upper.join("gone")).unwrap();
        assert!(wh.file_type().is_char_device() && wh.rdev() == 0);
        assert_eq!(
            xattr(&f.upper.join("rep"), "user.overlay.opaque").unwrap(),
            b"y"
        );
        let dst = f.root.join("published");
        let (_, done) = publish(&f.lower, &f.upper, &dst, &[], &|_| Ok(())).unwrap();
        // whiteouts: d/x, d/y (mv), gone, mvd (mv of a dir); opaque: rep, d/sub, f2d; links: newdir/hl
        assert!(
            done.whiteouts >= 4 && done.opaque >= 3 && done.links == 1,
            "{done:?}"
        );
        let got = listing(&f, &dst, "published.list");
        assert_eq!(got, f.merged, "published vs merged");
        // touch -d ...06.123456789: find's %T@ prints 10 fraction digits
        assert!(got.contains(".1234567890 "), "a's mtime to the ns: {got}");
        // no overlay xattr reaches the base
        for p in [dst.join("rep"), dst.join("a"), dst.join("d")] {
            assert!(
                xattr_names(&p)
                    .unwrap()
                    .iter()
                    .all(|n| overlay_attr(n).is_none()),
                "{}",
                p.display()
            );
        }
        assert!(got.contains("newdir/pipe p "), "the fifo: {got}");
        assert_eq!(ro_listing(&f), f.merged, "ro oracle vs rw merged");
    }

    /// The kernel's merge of the fixture's layers read-only (lowerdir=upper:lower, nothing written): the oracle gate.sh uses on real builds.
    fn ro_listing(f: &Fixture) -> String {
        let ro_mnt = f.root.join("ro");
        std::fs::create_dir_all(&ro_mnt).unwrap();
        let ro_list = f.root.join("ro.list");
        let opts = format!(
            "ro,lowerdir={}:{},userxattr",
            f.upper.display(),
            f.lower.display()
        );
        in_userns(
            "set -e; mount -t overlay overlay -o \"$1\" \"$2\"; sh \"$4\" \"$2\" \"$3\"",
            &[Path::new(&opts), &ro_mnt, &ro_list, &f.root.join("list.sh")],
        );
        std::fs::read_to_string(&ro_list).unwrap()
    }

    /// A socket node left in the upper (a gate's session bus, `app/bus/bus`, stopped real publishes before this) is published as a node, as `cp -a` publishes it from a pool dir.
    #[test]
    fn a_socket_in_the_upper_is_published() {
        let f = fixture("socket");
        drop(std::os::unix::net::UnixListener::bind(f.upper.join("newdir/sock")).unwrap());
        let want = ro_listing(&f);
        assert!(want.contains("newdir/sock s "), "{want}");
        let dst = f.root.join("published");
        let (_, done) = publish(&f.lower, &f.upper, &dst, &[], &|_| Ok(())).unwrap();
        assert_eq!(done.nodes, 2, "the fifo and the socket: {done:?}");
        assert_eq!(listing(&f, &dst, "published.list"), want);
    }

    /// Red controls: the same gate with whiteouts ignored, or opaque dirs merged like plain dirs, goes red.
    #[test]
    fn gate_is_red_without_whiteouts_or_opaque() {
        let f = fixture("red");
        let base = classify(&[]);
        let no_whiteouts = |p: &Path, r: &Path, m: &std::fs::Metadata| {
            base(p, r, m).map(|k| if k == Kind::Whiteout { Kind::Skip } else { k })
        };
        let no_opaque = |p: &Path, r: &Path, m: &std::fs::Metadata| {
            base(p, r, m).map(|k| if k == Kind::OpaqueDir { Kind::Dir } else { k })
        };
        for (name, cl) in [
            ("no-whiteouts", &no_whiteouts as &Classify<'_>),
            ("no-opaque", &no_opaque),
        ] {
            let dst = f.root.join(name);
            sh("cp -a \"$1\" \"$2\"", &[&f.lower, &dst]);
            apply(&f.upper, &dst, cl).unwrap();
            let got = listing(&f, &dst, &format!("{name}.list"));
            assert_ne!(got, f.merged, "{name}: the gate must see the difference");
        }
    }

    /// The upper of a mount this code does not model is refused, not guessed.
    #[test]
    fn redirect_and_metacopy_are_refused() {
        let dir =
            Path::new("/dev/shm").join(format!("xtask-publish-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        rustix::fs::lsetxattr(
            dir.join("d"),
            "user.overlay.redirect",
            b"/x",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
        rustix::fs::lsetxattr(
            dir.join("f"),
            "user.overlay.metacopy",
            b"",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
        let c = classify(&[]);
        for n in ["d", "f"] {
            let p = dir.join(n);
            let e = c(&p, Path::new(n), &std::fs::symlink_metadata(&p).unwrap()).unwrap_err();
            assert!(
                format!("{e:#}").contains("redirect_dir or metacopy"),
                "{e:#}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
