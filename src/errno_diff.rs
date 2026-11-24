// errno-diff suite: differential POSIX syscall errno & divergence contract probe.
// Executes a catalog of filesystem syscall edge cases in parallel against a
// reference local filesystem directory (`--ref-dir`, e.g. ext4 on Linux or tmpdir
// on macOS) and the target mountpoint, comparing application-visible errnos across
// three contract classes:
//   - `parity`:     target errno MUST equal reference errno observed on this host
//   - `diverge`:    target MUST return its documented divergence errno/shape
//                   (downgraded to `INFO` when `--diverge-as-record` is set, e.g. on
//                   a local-only overlay branch)
//   - `crosslayer`: cross-layer rename/hardlink (`--alt-root`) MUST return `EXDEV`
//   - `record`:     informational measurement only (no pass/fail judgment)

use crate::common::{CaseEntry, detect_fstype, host_name, iso8601, remove_dir_all_bounded};
use log::{info, warn};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(clap::Args)]
pub struct Args {
    /// reference local directory (auto-detects $EXT4_BASE, /mnt/ebs, $HOME, or temp_dir)
    #[arg(long)]
    pub ref_dir: Option<PathBuf>,

    /// require the reference directory to reside on an `ext4` filesystem (Linux CI gate)
    #[arg(long)]
    pub require_ext4: bool,

    /// alternate directory on the target mount's other layer to test cross-layer EXDEV (family X)
    #[arg(long)]
    pub alt_root: Option<PathBuf>,

    /// registered git-workspace root on the target mount to test `.git` hardlinks (family W)
    #[arg(long)]
    pub git_ws_root: Option<PathBuf>,

    /// downgrade `diverge`-class rows to informational records (for local-overlay pass)
    #[arg(long)]
    pub diverge_as_record: bool,

    /// evaluate all `diverge`-class rows as strict POSIX `parity` (`ref == target`)
    #[arg(long)]
    pub strict_posix: bool,

    /// optional suffix appended to per-family case IDs and table banners (e.g. `-local`)
    #[arg(long, default_value = "")]
    pub suffix: String,

    /// write the human-readable markdown comparison table to this file
    #[arg(long)]
    pub table_out: Option<PathBuf>,

    /// only run families matching these letters (comma list, e.g. A,B,D)
    #[arg(long, value_delimiter = ',')]
    pub families: Vec<String>,

    /// verbose logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// write the enriched JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Obs {
    Errno(i32),
    Stages(BTreeMap<&'static str, i32>),
}

impl Obs {
    fn fmt(&self) -> String {
        match self {
            Obs::Errno(0) => "OK".into(),
            Obs::Errno(e) => format!("{e}({})", errno_name(*e)),
            Obs::Stages(m) => m
                .iter()
                .map(|(k, v)| format!("{k}={}", Obs::Errno(*v).fmt()))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowClass {
    Parity,
    DivergeErrno(i32),
    DivergeDeferredEio,
    CrossLayerExdev,
    Record,
}

impl RowClass {
    fn label(self) -> &'static str {
        match self {
            RowClass::Parity => "parity",
            RowClass::DivergeErrno(_) | RowClass::DivergeDeferredEio => "diverge",
            RowClass::CrossLayerExdev => "crosslayer",
            RowClass::Record => "record",
        }
    }
}

struct SideCtx<'a> {
    alt_dir: Option<&'a Path>,
    git_ws_dir: Option<&'a Path>,
}

struct ProbeCase {
    id: &'static str,
    fam: &'static str,
    cls: RowClass,
    note: &'static str,
    run: fn(&Path, &SideCtx<'_>) -> Obs,
}

fn errno_name(e: i32) -> &'static str {
    match e {
        0 => "OK",
        x if x == libc::ENOENT => "ENOENT",
        x if x == libc::EEXIST => "EEXIST",
        x if x == libc::EISDIR => "EISDIR",
        x if x == libc::ENOTDIR => "ENOTDIR",
        x if x == libc::ENOTEMPTY => "ENOTEMPTY",
        x if x == libc::EINVAL => "EINVAL",
        x if x == libc::EACCES => "EACCES",
        x if x == libc::EPERM => "EPERM",
        x if x == libc::EXDEV => "EXDEV",
        x if x == libc::ELOOP => "ELOOP",
        x if x == libc::ENAMETOOLONG => "ENAMETOOLONG",
        x if x == libc::EIO => "EIO",
        x if x == libc::ENOSYS => "ENOSYS",
        x if x == libc::ENOTSUP => "ENOTSUP",
        x if x == libc::EOPNOTSUPP => "EOPNOTSUPP",
        x if x == libc::ERANGE => "ERANGE",
        x if x == libc::EFBIG => "EFBIG",
        x if x == libc::EBADF => "EBADF",
        x if x == libc::EROFS => "EROFS",
        x if x == libc::ENXIO => "ENXIO",
        x if x == libc::EILSEQ => "EILSEQ",
        x if x == libc::ENOTCONN => "ENOTCONN",
        x if x == libc::ENOSPC => "ENOSPC",
        x if x == libc::EDQUOT => "EDQUOT",
        x if x == libc::EMLINK => "EMLINK",
        x if x == libc::EOVERFLOW => "EOVERFLOW",
        x if x == libc::EAGAIN => "EAGAIN",
        x if x == libc::EWOULDBLOCK => "EWOULDBLOCK",
        x if x == libc::EBUSY => "EBUSY",
        x if x == libc::ESTALE => "ESTALE",
        x if x == libc::ETXTBSY => "ETXTBSY",
        x if x == libc::ESPIPE => "ESPIPE",
        #[cfg(target_os = "linux")]
        x if x == libc::ENODATA => "ENODATA",
        #[cfg(target_os = "macos")]
        x if x == libc::ENOATTR => "ENOATTR",
        _ => "ERR",
    }
}

fn last_errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

fn cpath_bytes(base: &Path, leaf: &[u8]) -> CString {
    let mut v = base.as_os_str().as_bytes().to_vec();
    if !v.ends_with(b"/") {
        v.push(b'/');
    }
    v.extend_from_slice(leaf);
    CString::new(v).unwrap()
}

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn cvt_rc(rc: libc::c_int) -> i32 {
    if rc == 0 { 0 } else { last_errno() }
}

fn cvt_ssize(rc: isize) -> i32 {
    if rc >= 0 { 0 } else { last_errno() }
}

fn mkfile(p: &Path, data: &[u8]) {
    let _ = fs::write(p, data);
}

fn mkdir(p: &Path) {
    let _ = fs::create_dir(p);
}

fn stat_errno(p: &Path) -> i32 {
    let cp = cpath(p);
    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
    cvt_rc(unsafe { libc::stat(cp.as_ptr(), &mut sb) })
}

fn mkdir_errno(p: &Path, mode: libc::mode_t) -> i32 {
    let cp = cpath(p);
    cvt_rc(unsafe { libc::mkdir(cp.as_ptr(), mode) })
}

fn rmdir_errno(p: &Path) -> i32 {
    let cp = cpath(p);
    cvt_rc(unsafe { libc::rmdir(cp.as_ptr()) })
}

fn unlink_errno(p: &Path) -> i32 {
    let cp = cpath(p);
    cvt_rc(unsafe { libc::unlink(cp.as_ptr()) })
}

fn open_close_errno(p: &Path, flags: libc::c_int, mode: libc::c_uint) -> i32 {
    let cp = cpath(p);
    let fd = unsafe { libc::open(cp.as_ptr(), flags, mode) };
    if fd < 0 {
        last_errno()
    } else {
        unsafe { libc::close(fd) };
        0
    }
}

fn rename_errno(src: &Path, dst: &Path) -> i32 {
    let cs = cpath(src);
    let cd = cpath(dst);
    cvt_rc(unsafe { libc::rename(cs.as_ptr(), cd.as_ptr()) })
}

const RENAME_NOREPLACE_FLAG: u32 = 1;
const RENAME_EXCHANGE_FLAG: u32 = 2;

fn rename_flags_errno(src: &Path, dst: &Path, flags: u32) -> i32 {
    let cs = cpath(src);
    let cd = cpath(dst);
    #[cfg(target_os = "linux")]
    unsafe {
        let rc = libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            cs.as_ptr(),
            libc::AT_FDCWD,
            cd.as_ptr(),
            flags as libc::c_uint,
        );
        if rc == 0 { 0 } else { last_errno() }
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut dflags: libc::c_uint = 0;
        if flags & RENAME_NOREPLACE_FLAG != 0 {
            dflags |= libc::RENAME_EXCL;
        }
        if flags & RENAME_EXCHANGE_FLAG != 0 {
            dflags |= libc::RENAME_SWAP;
        }
        cvt_rc(libc::renamex_np(cs.as_ptr(), cd.as_ptr(), dflags))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (cs, cd, flags);
        libc::ENOSYS
    }
}

fn link_errno(src: &Path, dst: &Path) -> i32 {
    let cs = cpath(src);
    let cd = cpath(dst);
    cvt_rc(unsafe { libc::link(cs.as_ptr(), cd.as_ptr()) })
}

fn symlink_errno(target: &str, linkpath: &Path) -> i32 {
    let ct = CString::new(target.as_bytes()).unwrap();
    let cl = cpath(linkpath);
    cvt_rc(unsafe { libc::symlink(ct.as_ptr(), cl.as_ptr()) })
}

fn readlink_errno(p: &Path) -> (i32, Vec<u8>) {
    let cp = cpath(p);
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::readlink(cp.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n < 0 {
        (last_errno(), Vec::new())
    } else {
        buf.truncate(n as usize);
        (0, buf)
    }
}

fn truncate_errno(p: &Path, len: libc::off_t) -> i32 {
    let cp = cpath(p);
    cvt_rc(unsafe { libc::truncate(cp.as_ptr(), len) })
}

fn setxattr_raw(p: &Path, name: &[u8], val: &[u8], flags: libc::c_int) -> i32 {
    let cp = cpath(p);
    let cn = CString::new(name).unwrap();
    #[cfg(target_os = "linux")]
    unsafe {
        cvt_rc(libc::setxattr(
            cp.as_ptr(),
            cn.as_ptr(),
            val.as_ptr() as *const libc::c_void,
            val.len(),
            flags,
        ))
    }
    #[cfg(target_os = "macos")]
    unsafe {
        cvt_rc(libc::setxattr(
            cp.as_ptr(),
            cn.as_ptr(),
            val.as_ptr() as *const libc::c_void,
            val.len(),
            0,
            flags,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (cp, cn, val, flags);
        libc::ENOSYS
    }
}

fn getxattr_raw(p: &Path, name: &[u8], buf: &mut [u8]) -> (i32, usize) {
    let cp = cpath(p);
    let cn = CString::new(name).unwrap();
    #[cfg(target_os = "linux")]
    let n = unsafe {
        libc::getxattr(
            cp.as_ptr(),
            cn.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    #[cfg(target_os = "macos")]
    let n = unsafe {
        libc::getxattr(
            cp.as_ptr(),
            cn.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            0,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let n: isize = -1;
    if n < 0 { (last_errno(), 0) } else { (0, n as usize) }
}

fn listxattr_raw(p: &Path, size: usize) -> i32 {
    let cp = cpath(p);
    let mut buf = vec![0u8; size.max(1)];
    #[cfg(target_os = "linux")]
    let n = unsafe { libc::listxattr(cp.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, size) };
    #[cfg(target_os = "macos")]
    let n = unsafe { libc::listxattr(cp.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, size, 0) };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let n: isize = -1;
    cvt_ssize(n)
}

fn removexattr_raw(p: &Path, name: &[u8]) -> i32 {
    let cp = cpath(p);
    let cn = CString::new(name).unwrap();
    #[cfg(target_os = "linux")]
    unsafe {
        cvt_rc(libc::removexattr(cp.as_ptr(), cn.as_ptr()))
    }
    #[cfg(target_os = "macos")]
    unsafe {
        cvt_rc(libc::removexattr(cp.as_ptr(), cn.as_ptr(), 0))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (cp, cn);
        libc::ENOSYS
    }
}

fn xattr_replace_flag() -> libc::c_int {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        libc::XATTR_REPLACE
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        2
    }
}

fn staged_raw_name(ws: &Path, raw_name: &[u8]) -> Obs {
    let mut stages = BTreeMap::new();
    let cp = cpath_bytes(ws, raw_name);
    let fd = unsafe { libc::open(cp.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644 as libc::c_uint) };
    if fd < 0 {
        stages.insert("open", last_errno());
        return Obs::Stages(stages);
    }
    stages.insert("open", 0);
    let w = unsafe { libc::write(fd, b"data".as_ptr() as *const libc::c_void, 4) };
    stages.insert("write", cvt_ssize(w));
    let fs_rc = unsafe { libc::fsync(fd) };
    stages.insert("fsync", cvt_rc(fs_rc));
    let cl_rc = unsafe { libc::close(fd) };
    stages.insert("close", cvt_rc(cl_rc));
    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
    let st_rc = unsafe { libc::stat(cp.as_ptr(), &mut sb) };
    stages.insert("stat", cvt_rc(st_rc));
    let ul_rc = unsafe { libc::unlink(cp.as_ptr()) };
    stages.insert("unlink", cvt_rc(ul_rc));
    Obs::Stages(stages)
}

fn falloc_probe(ws: &Path, mode: libc::c_int) -> Obs {
    let f = ws.join("f");
    mkfile(&f, &[b'y'; 8192]);
    let cp = cpath(&f);
    let fd = unsafe { libc::open(cp.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Obs::Errno(last_errno());
    }
    #[cfg(target_os = "linux")]
    let e = cvt_rc(unsafe { libc::fallocate(fd, mode, 0, 4096) });
    #[cfg(not(target_os = "linux"))]
    let e = {
        let _ = mode;
        libc::ENOTSUP
    };
    unsafe { libc::close(fd) };
    Obs::Errno(e)
}

fn build_catalog(with_alt: bool, with_gitws: bool) -> Vec<ProbeCase> {
    let mut c = vec![
        // --- A: path resolution ---
        ProbeCase {
            id: "A1-stat-missing-leaf",
            fam: "A",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(stat_errno(&ws.join("nope"))),
        },
        ProbeCase {
            id: "A2-stat-missing-mid",
            fam: "A",
            cls: RowClass::Parity,
            note: "a/ does not exist",
            run: |ws, _| Obs::Errno(stat_errno(&ws.join("a/b"))),
        },
        ProbeCase {
            id: "A3-stat-through-file",
            fam: "A",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), &[b'x'; 16]);
                Obs::Errno(stat_errno(&ws.join("f/x")))
            },
        },
        ProbeCase {
            id: "A4-name-256-bytes",
            fam: "A",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(mkdir_errno(&ws.join("n".repeat(256)), 0o755)),
        },
        ProbeCase {
            id: "A5-name-exactly-255",
            fam: "A",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(mkdir_errno(&ws.join("n".repeat(255)), 0o755)),
        },
        ProbeCase {
            id: "A6-path-beyond-pathmax",
            fam: "A",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(stat_errno(&ws.join("d".repeat(4096)).join("x"))),
        },
        ProbeCase {
            id: "A7-trailing-slash-on-file",
            fam: "A",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, &[b'x'; 16]);
                let cp = CString::new(format!("{}/", f.display())).unwrap();
                let mut sb: libc::stat = unsafe { std::mem::zeroed() };
                Obs::Errno(cvt_rc(unsafe { libc::stat(cp.as_ptr(), &mut sb) }))
            },
        },
        // --- B: mkdir / rmdir / unlink ---
        ProbeCase {
            id: "B1-mkdir-existing",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                Obs::Errno(mkdir_errno(&ws.join("d"), 0o755))
            },
        },
        ProbeCase {
            id: "B2-mkdir-missing-parent",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(mkdir_errno(&ws.join("a/b"), 0o755)),
        },
        ProbeCase {
            id: "B3-mkdir-parent-is-file",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), &[b'x'; 16]);
                Obs::Errno(mkdir_errno(&ws.join("f/sub"), 0o755))
            },
        },
        ProbeCase {
            id: "B4-rmdir-missing",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(rmdir_errno(&ws.join("d"))),
        },
        ProbeCase {
            id: "B5-rmdir-a-file",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), &[b'x'; 16]);
                Obs::Errno(rmdir_errno(&ws.join("f")))
            },
        },
        ProbeCase {
            id: "B6-rmdir-nonempty",
            fam: "B",
            cls: RowClass::Parity,
            note: "non-empty dir removal",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                mkfile(&ws.join("d/kid"), &[b'x'; 16]);
                Obs::Errno(rmdir_errno(&ws.join("d")))
            },
        },
        ProbeCase {
            id: "B7-unlink-missing",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(unlink_errno(&ws.join("f"))),
        },
        ProbeCase {
            id: "B8-unlink-directory",
            fam: "B",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                Obs::Errno(unlink_errno(&ws.join("d")))
            },
        },
        ProbeCase {
            id: "B9-unlink-dot",
            fam: "B",
            cls: RowClass::Parity,
            note: "kernel-side check",
            run: |ws, _| {
                let cp = CString::new(format!("{}/.", ws.display())).unwrap();
                Obs::Errno(cvt_rc(unsafe { libc::unlink(cp.as_ptr()) }))
            },
        },
        // --- C: create / open flags ---
        ProbeCase {
            id: "C1-oexcl-existing",
            fam: "C",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), &[b'x'; 16]);
                Obs::Errno(open_close_errno(
                    &ws.join("f"),
                    libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
                    0o644,
                ))
            },
        },
        ProbeCase {
            id: "C2-creat-on-directory",
            fam: "C",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                Obs::Errno(open_close_errno(&ws.join("d"), libc::O_CREAT | libc::O_WRONLY, 0o644))
            },
        },
        ProbeCase {
            id: "C3-creat-missing-parent",
            fam: "C",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(open_close_errno(&ws.join("a/f"), libc::O_CREAT | libc::O_WRONLY, 0o644)),
        },
        ProbeCase {
            id: "C4-open-missing",
            fam: "C",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(open_close_errno(&ws.join("f"), libc::O_RDONLY, 0)),
        },
        ProbeCase {
            id: "C5-open-dir-o-rdwr",
            fam: "C",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                Obs::Errno(open_close_errno(&ws.join("d"), libc::O_RDWR, 0))
            },
        },
        ProbeCase {
            id: "C6-ftruncate-readonly-fd",
            fam: "C",
            cls: RowClass::Parity,
            note: "kernel-side check",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, &[b'x'; 16]);
                let cp = cpath(&f);
                let fd = unsafe { libc::open(cp.as_ptr(), libc::O_RDONLY) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                let e = cvt_rc(unsafe { libc::ftruncate(fd, 4) });
                unsafe { libc::close(fd) };
                Obs::Errno(e)
            },
        },
        ProbeCase {
            id: "C7-onofollow-symlink",
            fam: "C",
            cls: RowClass::Parity,
            note: "kernel-side check",
            run: |ws, _| {
                let l = ws.join("l");
                let _ = symlink_errno("target", &l);
                Obs::Errno(open_close_errno(&l, libc::O_RDONLY | libc::O_NOFOLLOW, 0))
            },
        },
        ProbeCase {
            id: "C8-odirectory-on-file",
            fam: "C",
            cls: RowClass::Parity,
            note: "kernel-side check",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, &[b'x'; 16]);
                Obs::Errno(open_close_errno(&f, libc::O_RDONLY | libc::O_DIRECTORY, 0))
            },
        },
        ProbeCase {
            id: "C9-open-dir-trunc",
            fam: "C",
            cls: RowClass::Parity,
            note: "O_TRUNC on directory -> EISDIR",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                Obs::Errno(open_close_errno(&ws.join("d"), libc::O_RDONLY | libc::O_TRUNC, 0))
            },
        },
        // --- D: rename ---
        ProbeCase {
            id: "D1-file-over-dir",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("dd"));
                mkfile(&ws.join("f"), &[b'x'; 16]);
                Obs::Errno(rename_errno(&ws.join("f"), &ws.join("dd")))
            },
        },
        ProbeCase {
            id: "D2-dir-over-file",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("dd"));
                mkfile(&ws.join("f"), &[b'x'; 16]);
                Obs::Errno(rename_errno(&ws.join("dd"), &ws.join("f")))
            },
        },
        ProbeCase {
            id: "D3-dir-over-nonempty",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d1"));
                mkdir(&ws.join("d2"));
                mkfile(&ws.join("d1/kid"), b"1");
                mkfile(&ws.join("d2/kid"), b"2");
                Obs::Errno(rename_errno(&ws.join("d1"), &ws.join("d2")))
            },
        },
        ProbeCase {
            id: "D4-rename-into-descendant",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d1"));
                mkdir(&ws.join("d1/d2"));
                Obs::Errno(rename_errno(&ws.join("d1"), &ws.join("d1/d2/d3")))
            },
        },
        ProbeCase {
            id: "D5-source-missing",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(rename_errno(&ws.join("old"), &ws.join("new"))),
        },
        ProbeCase {
            id: "D6-dest-parent-missing",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d1"));
                Obs::Errno(rename_errno(&ws.join("d1"), &ws.join("nodir/new")))
            },
        },
        ProbeCase {
            id: "D7-file-over-file",
            fam: "D",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f1"), b"one");
                mkfile(&ws.join("f2"), b"two");
                Obs::Errno(rename_errno(&ws.join("f1"), &ws.join("f2")))
            },
        },
        ProbeCase {
            id: "D8-rename-same-path",
            fam: "D",
            cls: RowClass::Parity,
            note: "kernel: OK no-op",
            run: |ws, _| {
                mkfile(&ws.join("f1"), b"one");
                Obs::Errno(rename_errno(&ws.join("f1"), &ws.join("f1")))
            },
        },
        ProbeCase {
            id: "D9-noreplace-existing",
            fam: "D",
            cls: RowClass::Parity,
            note: "kernel-side EEXIST",
            run: |ws, _| {
                mkfile(&ws.join("f1"), b"1");
                mkfile(&ws.join("f2"), b"2");
                Obs::Errno(rename_flags_errno(
                    &ws.join("f1"),
                    &ws.join("f2"),
                    RENAME_NOREPLACE_FLAG,
                ))
            },
        },
        ProbeCase {
            id: "D10-noreplace-dest-missing",
            fam: "D",
            cls: RowClass::DivergeErrno(libc::EINVAL),
            note: "ext4 succeeds; documented divergence EINVAL",
            run: |ws, _| {
                mkfile(&ws.join("f1"), b"1");
                Obs::Errno(rename_flags_errno(
                    &ws.join("f1"),
                    &ws.join("f2"),
                    RENAME_NOREPLACE_FLAG,
                ))
            },
        },
        ProbeCase {
            id: "D11-exchange-existing",
            fam: "D",
            cls: RowClass::DivergeErrno(libc::EINVAL),
            note: "ext4 succeeds; documented divergence EINVAL",
            run: |ws, _| {
                mkfile(&ws.join("f1"), b"one");
                mkfile(&ws.join("f2"), b"two");
                Obs::Errno(rename_flags_errno(&ws.join("f1"), &ws.join("f2"), RENAME_EXCHANGE_FLAG))
            },
        },
        ProbeCase {
            id: "D12-exchange-dest-missing",
            fam: "D",
            cls: RowClass::Parity,
            note: "kernel-side ENOENT",
            run: |ws, _| {
                mkfile(&ws.join("f1"), b"one");
                Obs::Errno(rename_flags_errno(&ws.join("f1"), &ws.join("f2"), RENAME_EXCHANGE_FLAG))
            },
        },
        // --- E: link ---
        ProbeCase {
            id: "E1-link-directory",
            fam: "E",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkdir(&ws.join("d"));
                Obs::Errno(link_errno(&ws.join("d"), &ws.join("l")))
            },
        },
        ProbeCase {
            id: "E2-link-file",
            fam: "E",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), b"x");
                Obs::Errno(link_errno(&ws.join("f"), &ws.join("l")))
            },
        },
        ProbeCase {
            id: "E3-link-source-missing",
            fam: "E",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(link_errno(&ws.join("f"), &ws.join("l"))),
        },
        ProbeCase {
            id: "E4-link-dest-exists",
            fam: "E",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), b"x");
                mkfile(&ws.join("l"), b"y");
                Obs::Errno(link_errno(&ws.join("f"), &ws.join("l")))
            },
        },
        ProbeCase {
            id: "E5-link-dest-parent-missing",
            fam: "E",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), b"x");
                Obs::Errno(link_errno(&ws.join("f"), &ws.join("nodir/l")))
            },
        },
        // --- F: symlink ---
        ProbeCase {
            id: "F1-symlink-create-readlink",
            fam: "F",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let l = ws.join("l");
                let e = symlink_errno("target-string", &l);
                if e != 0 {
                    return Obs::Errno(e);
                }
                let (re, got) = readlink_errno(&l);
                if re != 0 {
                    Obs::Errno(re)
                } else if got != b"target-string" {
                    Obs::Errno(libc::EINVAL)
                } else {
                    Obs::Errno(0)
                }
            },
        },
        ProbeCase {
            id: "F2-symlink-dest-exists",
            fam: "F",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                mkfile(&ws.join("f"), b"x");
                Obs::Errno(symlink_errno("t", &ws.join("f")))
            },
        },
        ProbeCase {
            id: "F3-symlink-parent-missing",
            fam: "F",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(symlink_errno("t", &ws.join("nodir/l"))),
        },
        ProbeCase {
            id: "F4-open-dangling-symlink",
            fam: "F",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let l = ws.join("l");
                let _ = symlink_errno("nowhere", &l);
                Obs::Errno(open_close_errno(&l, libc::O_RDONLY, 0))
            },
        },
        ProbeCase {
            id: "F5-open-symlink-loop",
            fam: "F",
            cls: RowClass::Parity,
            note: "kernel-side ELOOP",
            run: |ws, _| {
                let l = ws.join("loop");
                let _ = symlink_errno("loop", &l);
                Obs::Errno(open_close_errno(&l, libc::O_RDONLY, 0))
            },
        },
        ProbeCase {
            id: "F6-readlink-regular-file",
            fam: "F",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                Obs::Errno(readlink_errno(&f).0)
            },
        },
        ProbeCase {
            id: "F7-symlink-target-too-long",
            fam: "F",
            cls: RowClass::Record,
            note: "varies by fs",
            run: |ws, _| {
                let long = format!("/{}", "p".repeat(4096));
                Obs::Errno(symlink_errno(&long, &ws.join("l")))
            },
        },
        // --- G: filename charset (deferred-error divergence) ---
        ProbeCase {
            id: "G1-name-backslash",
            fam: "G",
            cls: RowClass::DivergeDeferredEio,
            note: "documented shape: open/write OK, fsync/close EIO",
            run: |ws, _| staged_raw_name(ws, b"back\\slash.txt"),
        },
        ProbeCase {
            id: "G2-name-control-byte",
            fam: "G",
            cls: RowClass::DivergeDeferredEio,
            note: "documented shape: open/write OK, fsync/close EIO",
            run: |ws, _| staged_raw_name(ws, b"ct\x01rl.txt"),
        },
        ProbeCase {
            id: "G3-name-invalid-utf8",
            fam: "G",
            cls: RowClass::DivergeDeferredEio,
            note: "documented shape: open/write OK, fsync/close EIO",
            run: |ws, _| staged_raw_name(ws, b"\xff\xfe.txt"),
        },
        // --- H: write / truncate ---
        ProbeCase {
            id: "H1-write-truncate-basics",
            fam: "H",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"0123456789");
                Obs::Errno(truncate_errno(&f, 4))
            },
        },
        ProbeCase {
            id: "H2-pread-pwrite-neg-offset",
            fam: "H",
            cls: RowClass::Parity,
            note: "negative pread/pwrite offset -> EINVAL",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"01234567");
                let fd = unsafe { libc::open(cpath(&f).as_ptr(), libc::O_RDWR) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                let mut buf = [0u8; 4];
                let r1 = cvt_ssize(unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, 4, -1) });
                let r2 = cvt_ssize(unsafe { libc::pwrite(fd, b"abcd".as_ptr() as *const libc::c_void, 4, -1) });
                unsafe { libc::close(fd) };
                let mut stages = BTreeMap::new();
                stages.insert("pread", r1);
                stages.insert("pwrite", r2);
                Obs::Stages(stages)
            },
        },
        ProbeCase {
            id: "H3-truncate-negative",
            fam: "H",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(truncate_errno(&ws.join("f"), -1)),
        },
        ProbeCase {
            id: "H4-truncate-huge",
            fam: "H",
            cls: RowClass::Record,
            note: "boundary record",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                Obs::Errno(truncate_errno(&f, 1i64 << 62))
            },
        },
        ProbeCase {
            id: "H5-truncate-missing",
            fam: "H",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| Obs::Errno(truncate_errno(&ws.join("f"), 4)),
        },
        ProbeCase {
            id: "H6-rw-bad-fd-mode",
            fam: "H",
            cls: RowClass::Parity,
            note: "read dir -> EISDIR; write O_RDONLY -> EBADF",
            run: |ws, _| {
                let d = ws.join("d");
                let f = ws.join("f");
                mkdir(&d);
                mkfile(&f, b"abcd");
                let dfd = unsafe { libc::open(cpath(&d).as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
                let ffd = unsafe { libc::open(cpath(&f).as_ptr(), libc::O_RDONLY) };
                let mut buf = [0u8; 4];
                let rd_dir = if dfd >= 0 {
                    let e = cvt_ssize(unsafe { libc::read(dfd, buf.as_mut_ptr() as *mut libc::c_void, 4) });
                    unsafe { libc::close(dfd) };
                    e
                } else {
                    last_errno()
                };
                let wr_ro = if ffd >= 0 {
                    let e = cvt_ssize(unsafe { libc::write(ffd, b"xy".as_ptr() as *const libc::c_void, 2) });
                    unsafe { libc::close(ffd) };
                    e
                } else {
                    last_errno()
                };
                let mut stages = BTreeMap::new();
                stages.insert("read_dir", rd_dir);
                stages.insert("write_ro", wr_ro);
                Obs::Stages(stages)
            },
        },
        // --- I: fsync ---
        ProbeCase {
            id: "I1-fsync-readonly-fd",
            fam: "I",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let cp = cpath(&f);
                let fd = unsafe { libc::open(cp.as_ptr(), libc::O_RDONLY) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                let e = cvt_rc(unsafe { libc::fsync(fd) });
                unsafe { libc::close(fd) };
                Obs::Errno(e)
            },
        },
        ProbeCase {
            id: "I2-fsync-directory",
            fam: "I",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let d = ws.join("d");
                mkdir(&d);
                let cp = cpath(&d);
                let fd = unsafe { libc::open(cp.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                let e = cvt_rc(unsafe { libc::fsync(fd) });
                unsafe { libc::close(fd) };
                Obs::Errno(e)
            },
        },
        ProbeCase {
            id: "I3-fdatasync",
            fam: "I",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let cp = cpath(&f);
                let fd = unsafe { libc::open(cp.as_ptr(), libc::O_RDONLY) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                #[cfg(target_os = "linux")]
                let e = cvt_rc(unsafe { libc::fdatasync(fd) });
                #[cfg(not(target_os = "linux"))]
                let e = cvt_rc(unsafe { libc::fsync(fd) });
                unsafe { libc::close(fd) };
                Obs::Errno(e)
            },
        },
        // --- J: permissions ---
        ProbeCase {
            id: "J1-create-in-unwritable-dir",
            fam: "J",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let d = ws.join("d");
                mkdir(&d);
                let cd = cpath(&d);
                unsafe { libc::chmod(cd.as_ptr(), 0o555) };
                let e = open_close_errno(&d.join("new"), libc::O_CREAT | libc::O_WRONLY, 0o644);
                unsafe { libc::chmod(cd.as_ptr(), 0o755) };
                Obs::Errno(e)
            },
        },
        ProbeCase {
            id: "J2-mkdir-in-unwritable-dir",
            fam: "J",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let d = ws.join("d");
                mkdir(&d);
                let cd = cpath(&d);
                unsafe { libc::chmod(cd.as_ptr(), 0o555) };
                let e = mkdir_errno(&d.join("sub"), 0o755);
                unsafe { libc::chmod(cd.as_ptr(), 0o755) };
                Obs::Errno(e)
            },
        },
        ProbeCase {
            id: "J3-traverse-unsearchable-dir",
            fam: "J",
            cls: RowClass::Parity,
            note: "requires default_permissions / --allow-other",
            run: |ws, _| {
                let d = ws.join("d");
                mkdir(&d);
                mkfile(&d.join("f"), b"x");
                let cd = cpath(&d);
                unsafe { libc::chmod(cd.as_ptr(), 0o000) };
                let e = stat_errno(&d.join("f"));
                unsafe { libc::chmod(cd.as_ptr(), 0o755) };
                Obs::Errno(e)
            },
        },
        // --- K: xattr ---
        ProbeCase {
            id: "K1-getxattr-missing",
            fam: "K",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let mut buf = [0u8; 64];
                Obs::Errno(getxattr_raw(&f, b"user.probe", &mut buf).0)
            },
        },
        ProbeCase {
            id: "K2-setxattr-replace-missing",
            fam: "K",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                Obs::Errno(setxattr_raw(&f, b"user.probe", b"v", xattr_replace_flag()))
            },
        },
        ProbeCase {
            id: "K3-user-xattr-roundtrip",
            fam: "K",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let e = setxattr_raw(&f, b"user.probe", b"value", 0);
                if e != 0 {
                    return Obs::Errno(e);
                }
                let mut buf = [0u8; 64];
                let (ge, n) = getxattr_raw(&f, b"user.probe", &mut buf);
                if ge != 0 {
                    Obs::Errno(ge)
                } else if &buf[..n] != b"value" {
                    Obs::Errno(libc::EINVAL)
                } else {
                    Obs::Errno(0)
                }
            },
        },
        ProbeCase {
            id: "K4-unknown-namespace",
            fam: "K",
            cls: if cfg!(target_os = "macos") {
                RowClass::Record
            } else {
                RowClass::Parity
            },
            note: "unknown xattr namespace",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                Obs::Errno(setxattr_raw(&f, b"bogus.ns.probe", b"v", 0))
            },
        },
        ProbeCase {
            id: "K5-getxattr-small-buffer",
            fam: "K",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let _ = setxattr_raw(&f, b"user.probe", b"0123456789", 0);
                let mut buf = [0u8; 4];
                Obs::Errno(getxattr_raw(&f, b"user.probe", &mut buf).0)
            },
        },
        ProbeCase {
            id: "K6-listxattr-small-buffer",
            fam: "K",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let _ = setxattr_raw(&f, b"user.probe", b"v", 0);
                Obs::Errno(listxattr_raw(&f, 1))
            },
        },
        ProbeCase {
            id: "K7-removexattr-missing",
            fam: "K",
            cls: RowClass::Parity,
            note: "",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                Obs::Errno(removexattr_raw(&f, b"user.probe"))
            },
        },
        ProbeCase {
            id: "K8-trusted-ns-nonroot",
            fam: "K",
            cls: if cfg!(target_os = "macos") {
                RowClass::Record
            } else {
                RowClass::Parity
            },
            note: "CAP_SYS_ADMIN check",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                Obs::Errno(setxattr_raw(&f, b"trusted.probe", b"v", 0))
            },
        },
        ProbeCase {
            id: "K9-xattr-name-too-long",
            fam: "K",
            cls: RowClass::Record,
            note: "record answer",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let mut nm = b"user.".to_vec();
                nm.extend_from_slice(&[b'a'; 300]);
                Obs::Errno(setxattr_raw(&f, &nm, b"v", 0))
            },
        },
        // --- L: fallocate & friends ---
        ProbeCase {
            id: "L1-fallocate-default",
            fam: "L",
            cls: RowClass::DivergeErrno(libc::ENOTSUP),
            note: "documented divergence ENOTSUP",
            run: |ws, _| falloc_probe(ws, 0),
        },
        ProbeCase {
            id: "L2-fallocate-punch-hole",
            fam: "L",
            cls: RowClass::DivergeErrno(libc::ENOTSUP),
            note: "documented divergence ENOTSUP",
            run: |ws, _| falloc_probe(ws, 0x02 | 0x01),
        },
        ProbeCase {
            id: "L3-fallocate-collapse",
            fam: "L",
            cls: RowClass::DivergeErrno(libc::ENOTSUP),
            note: "documented divergence ENOTSUP",
            run: |ws, _| falloc_probe(ws, 0x08),
        },
        ProbeCase {
            id: "L4-fallocate-zero-range",
            fam: "L",
            cls: RowClass::DivergeErrno(libc::ENOTSUP),
            note: "documented divergence ENOTSUP",
            run: |ws, _| falloc_probe(ws, 0x10),
        },
        ProbeCase {
            id: "L5-copy-file-range",
            fam: "L",
            cls: RowClass::Record,
            note: "record resulting errno",
            run: |ws, _| {
                let src = ws.join("src");
                let dst = ws.join("dst");
                mkfile(&src, &[b'z'; 4096]);
                mkfile(&dst, b"x");
                let s = unsafe { libc::open(cpath(&src).as_ptr(), libc::O_RDONLY) };
                let d = unsafe { libc::open(cpath(&dst).as_ptr(), libc::O_WRONLY) };
                if s < 0 || d < 0 {
                    let e = last_errno();
                    if s >= 0 {
                        unsafe { libc::close(s) };
                    }
                    if d >= 0 {
                        unsafe { libc::close(d) };
                    }
                    return Obs::Errno(e);
                }
                #[cfg(target_os = "linux")]
                let e = {
                    let mut o1: libc::off64_t = 0;
                    let mut o2: libc::off64_t = 0;
                    let rc = unsafe { libc::copy_file_range(s, &mut o1, d, &mut o2, 4096, 0) };
                    cvt_ssize(rc)
                };
                #[cfg(not(target_os = "linux"))]
                let e = libc::ENOSYS;
                unsafe {
                    libc::close(s);
                    libc::close(d);
                }
                Obs::Errno(e)
            },
        },
        ProbeCase {
            id: "L6-lseek-seek-hole",
            fam: "L",
            cls: RowClass::Record,
            note: "SEEK_HOLE record",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, &[b'w'; 8192]);
                let fd = unsafe { libc::open(cpath(&f).as_ptr(), libc::O_RDONLY) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                let rc = unsafe { libc::lseek(fd, 0, libc::SEEK_HOLE) };
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                let rc: libc::off_t = -1;
                let e = if rc >= 0 { 0 } else { last_errno() };
                unsafe { libc::close(fd) };
                Obs::Errno(e)
            },
        },
        // --- M: special files ---
        ProbeCase {
            id: "M1-mkfifo",
            fam: "M",
            cls: RowClass::Record,
            note: "mkfifo record",
            run: |ws, _| {
                let cp = cpath(&ws.join("fifo"));
                Obs::Errno(cvt_rc(unsafe { libc::mkfifo(cp.as_ptr(), 0o644) }))
            },
        },
        ProbeCase {
            id: "M2-mknod-device-nonroot",
            fam: "M",
            cls: RowClass::Parity,
            note: "kernel-side EPERM (CAP_MKNOD)",
            run: |ws, _| {
                let cp = cpath(&ws.join("dev"));
                Obs::Errno(cvt_rc(unsafe {
                    libc::mknod(cp.as_ptr(), (libc::S_IFCHR | 0o600) as libc::mode_t, 0)
                }))
            },
        },
        ProbeCase {
            id: "M3-open-unlinked-io",
            fam: "M",
            cls: RowClass::Parity,
            note: "open + unlink + write + fstat(nlink=0) + pread",
            run: |ws, _| {
                let f = ws.join("unlinked");
                let cp = cpath(&f);
                let fd = unsafe { libc::open(cp.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644 as libc::c_uint) };
                if fd < 0 {
                    return Obs::Errno(last_errno());
                }
                let u_rc = cvt_rc(unsafe { libc::unlink(cp.as_ptr()) });
                let w_rc = cvt_ssize(unsafe { libc::pwrite(fd, b"alive".as_ptr() as *const libc::c_void, 5, 0) });
                let mut sb: libc::stat = unsafe { std::mem::zeroed() };
                let st_rc = cvt_rc(unsafe { libc::fstat(fd, &mut sb) });
                let mut rbuf = [0u8; 5];
                let r_rc = cvt_ssize(unsafe { libc::pread(fd, rbuf.as_mut_ptr() as *mut libc::c_void, 5, 0) });
                unsafe { libc::close(fd) };
                let ok = u_rc == 0 && w_rc == 0 && st_rc == 0 && sb.st_nlink == 0 && r_rc == 0 && &rbuf == b"alive";
                Obs::Errno(if ok { 0 } else { libc::EIO })
            },
        },
        ProbeCase {
            id: "M4-chmod-utimens-errors",
            fam: "M",
            cls: RowClass::Parity,
            note: "chmod/utimensat ENOENT and ENOTDIR",
            run: |ws, _| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let c_miss = cvt_rc(unsafe { libc::chmod(cpath(&ws.join("missing")).as_ptr(), 0o600) });
                let c_notdir = cvt_rc(unsafe { libc::chmod(cpath(&ws.join("f/child")).as_ptr(), 0o600) });
                let ts = [
                    libc::timespec {
                        tv_sec: 1_700_000_000,
                        tv_nsec: 0,
                    },
                    libc::timespec {
                        tv_sec: 1_700_000_000,
                        tv_nsec: 0,
                    },
                ];
                let u_miss = cvt_rc(unsafe {
                    libc::utimensat(libc::AT_FDCWD, cpath(&ws.join("missing")).as_ptr(), ts.as_ptr(), 0)
                });
                let mut stages = BTreeMap::new();
                stages.insert("chmod_missing", c_miss);
                stages.insert("chmod_notdir", c_notdir);
                stages.insert("utimens_missing", u_miss);
                Obs::Stages(stages)
            },
        },
        ProbeCase {
            id: "M5-flock-nonblock-conflict",
            fam: "M",
            cls: RowClass::Parity,
            note: "flock LOCK_EX|LOCK_NB contention",
            run: |ws, _| {
                let f = ws.join("lockf");
                mkfile(&f, b"x");
                let cp = cpath(&f);
                let fd1 = unsafe { libc::open(cp.as_ptr(), libc::O_RDWR) };
                let fd2 = unsafe { libc::open(cp.as_ptr(), libc::O_RDWR) };
                if fd1 < 0 || fd2 < 0 {
                    let e = last_errno();
                    if fd1 >= 0 {
                        unsafe { libc::close(fd1) };
                    }
                    if fd2 >= 0 {
                        unsafe { libc::close(fd2) };
                    }
                    return Obs::Errno(e);
                }
                let l1 = cvt_rc(unsafe { libc::flock(fd1, libc::LOCK_EX | libc::LOCK_NB) });
                let l2 = cvt_rc(unsafe { libc::flock(fd2, libc::LOCK_EX | libc::LOCK_NB) });
                unsafe {
                    libc::close(fd1);
                    libc::close(fd2);
                }
                let mut stages = BTreeMap::new();
                stages.insert("first", l1);
                stages.insert("contended", l2);
                Obs::Stages(stages)
            },
        },
    ];

    if with_alt {
        c.push(ProbeCase {
            id: "X1-cross-layer-rename",
            fam: "X",
            cls: RowClass::CrossLayerExdev,
            note: "documented cross-layer EXDEV",
            run: |ws, ctx| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let dst = ctx.alt_dir.unwrap().join("x1-target");
                Obs::Errno(rename_errno(&f, &dst))
            },
        });
        c.push(ProbeCase {
            id: "X2-cross-layer-hardlink",
            fam: "X",
            cls: RowClass::CrossLayerExdev,
            note: "documented cross-layer EXDEV",
            run: |ws, ctx| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let dst = ctx.alt_dir.unwrap().join("x2-target");
                Obs::Errno(link_errno(&f, &dst))
            },
        });
    }

    if with_gitws {
        c.push(ProbeCase {
            id: "W1-hardlink-from-gitws-dotgit",
            fam: "W",
            cls: RowClass::Parity,
            note: "git-ws .git hardlink parity",
            run: |ws, ctx| {
                let src = ctx.git_ws_dir.unwrap().join(".git/config");
                Obs::Errno(link_errno(&src, &ws.join("l")))
            },
        });
        c.push(ProbeCase {
            id: "W2-hardlink-into-gitws-dotgit",
            fam: "W",
            cls: RowClass::Parity,
            note: "git-ws .git hardlink parity",
            run: |ws, ctx| {
                let f = ws.join("f");
                mkfile(&f, b"x");
                let dst = ctx.git_ws_dir.unwrap().join(".git/hl-out");
                let _ = fs::remove_file(&dst);
                Obs::Errno(link_errno(&f, &dst))
            },
        });
    }

    c
}

fn is_local_kernel_fs(fstype: &str) -> bool {
    matches!(
        fstype.to_ascii_lowercase().as_str(),
        "ext4" | "xfs" | "btrfs" | "apfs" | "tmpfs" | "ufs" | "zfs"
    )
}

fn is_nonroot_only_probe(id: &str) -> bool {
    matches!(
        id,
        "J1-create-in-unwritable-dir"
            | "J2-mkdir-in-unwritable-dir"
            | "J3-traverse-unsearchable-dir"
            | "K8-trusted-ns-nonroot"
            | "M2-mknod-device-nonroot"
    )
}

fn evaluate(
    id: &str,
    cls: RowClass,
    ref_obs: &Obs,
    tgt_obs: &Obs,
    diverge_as_record: bool,
    strict_posix: bool,
    is_root: bool,
) -> (&'static str, String) {
    if is_root && is_nonroot_only_probe(id) {
        return ("INFO", "euid=0 bypasses non-root DAC/CAP check".into());
    }
    match cls {
        RowClass::Record => ("INFO", String::new()),
        RowClass::DivergeErrno(_) | RowClass::DivergeDeferredEio if strict_posix => (
            if ref_obs == tgt_obs { "PASS" } else { "FAIL" },
            "strict-posix parity".into(),
        ),
        RowClass::DivergeErrno(_) | RowClass::DivergeDeferredEio if diverge_as_record => {
            ("INFO", "divergence doc is remote-branch only".into())
        }
        RowClass::DivergeErrno(doc_errno) => {
            let ok = *tgt_obs == Obs::Errno(doc_errno);
            (
                if ok { "PASS" } else { "FAIL" },
                format!("doc={}", Obs::Errno(doc_errno).fmt()),
            )
        }
        RowClass::DivergeDeferredEio => {
            let ok = match tgt_obs {
                Obs::Stages(m) => {
                    m.get("open") == Some(&0)
                        && m.get("write") == Some(&0)
                        && (m.get("fsync") == Some(&libc::EIO) || m.get("close") == Some(&libc::EIO))
                }
                _ => false,
            };
            (if ok { "PASS" } else { "FAIL" }, "doc-shape=deferred-eio".into())
        }
        RowClass::CrossLayerExdev => {
            let ok = *tgt_obs == Obs::Errno(libc::EXDEV);
            (
                if ok { "PASS" } else { "FAIL" },
                format!("doc={}", Obs::Errno(libc::EXDEV).fmt()),
            )
        }
        RowClass::Parity => (if ref_obs == tgt_obs { "PASS" } else { "FAIL" }, String::new()),
    }
}

fn resolve_ref_base(args: &Args) -> Result<(PathBuf, String), String> {
    if let Some(r) = &args.ref_dir {
        if !r.is_dir() {
            return Err(format!("--ref-dir {} is not a directory", r.display()));
        }
        let fstype = detect_fstype(r);
        if args.require_ext4 && fstype != "ext4" {
            return Err(format!(
                "--ref-dir {} is on {fstype:?}, not ext4 (--require-ext4)",
                r.display()
            ));
        }
        return Ok((r.clone(), fstype));
    }

    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(e) = std::env::var("EXT4_BASE")
        && !e.trim().is_empty()
    {
        cands.push(PathBuf::from(e));
    }
    cands.push(PathBuf::from("/mnt/ebs"));
    if let Some(pw) = crate::common::passwd_home_dir() {
        cands.push(pw);
    }
    cands.push(std::env::temp_dir());

    let mut fallback_tmpfs: Option<(PathBuf, String)> = None;
    for c in cands {
        if !c.is_dir() {
            continue;
        }
        let fstype = detect_fstype(&c);
        if args.require_ext4 && fstype != "ext4" {
            continue;
        }
        if fstype == "tmpfs" {
            if fallback_tmpfs.is_none() {
                fallback_tmpfs = Some((c, fstype));
            }
            continue;
        }
        return Ok((c, fstype));
    }
    if let Some(fb) = fallback_tmpfs {
        return Ok(fb);
    }
    Err("no suitable reference base directory found (pass --ref-dir or set EXT4_BASE)".into())
}

fn run_side(root: &Path, ctx: &SideCtx<'_>, pc: &ProbeCase) -> Obs {
    let ws = root.join(pc.id);
    let _ = fs::create_dir_all(&ws);
    (pc.run)(&ws, ctx)
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let (ref_base, ref_fstype) = resolve_ref_base(args)?;
    let tgt_fstype = detect_fstype(mountpoint);
    let strict_posix = args.strict_posix || is_local_kernel_fs(&tgt_fstype);
    let is_root = unsafe { libc::geteuid() == 0 };

    let started = SystemTime::now();
    let t0 = Instant::now();
    let stamp = format!(
        "{}-{}",
        started.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        std::process::id()
    );

    let ref_root = ref_base.join(format!(".errno-cmp-{stamp}"));
    let tgt_root = mountpoint.join(format!(".errno-cmp-{stamp}"));
    fs::create_dir_all(&ref_root).map_err(|e| format!("mkdir {}: {e}", ref_root.display()))?;
    fs::create_dir_all(&tgt_root).map_err(|e| format!("mkdir {}: {e}", tgt_root.display()))?;

    let mut alt_created_parent = false;
    let (ref_alt, tgt_alt_scratch) = if let Some(tgt_alt) = &args.alt_root {
        let ra = ref_base.join(format!(".errno-cmp-alt-{stamp}"));
        fs::create_dir_all(&ra).map_err(|e| e.to_string())?;
        if !tgt_alt.exists() {
            alt_created_parent = true;
            fs::create_dir_all(tgt_alt).map_err(|e| e.to_string())?;
        }
        let ta_scratch = tgt_alt.join(format!(".errno-cmp-alt-{stamp}"));
        fs::create_dir_all(&ta_scratch).map_err(|e| e.to_string())?;
        (Some(ra), Some(ta_scratch))
    } else {
        (None, None)
    };

    let ref_gitws = if args.git_ws_root.is_some() {
        let rg = ref_base.join(format!(".errno-cmp-gitws-{stamp}"));
        fs::create_dir_all(rg.join(".git")).map_err(|e| e.to_string())?;
        mkfile(&rg.join(".git/config"), b"[core]\n");
        Some(rg)
    } else {
        None
    };

    let ref_ctx = SideCtx {
        alt_dir: ref_alt.as_deref(),
        git_ws_dir: ref_gitws.as_deref(),
    };
    let tgt_ctx = SideCtx {
        alt_dir: tgt_alt_scratch.as_deref(),
        git_ws_dir: args.git_ws_root.as_deref(),
    };

    let mut catalog = build_catalog(args.alt_root.is_some(), args.git_ws_root.is_some());
    if !args.families.is_empty() {
        catalog.retain(|c| args.families.iter().any(|f| f.eq_ignore_ascii_case(c.fam)));
    }

    info!(
        "errno-diff: {} cases across ref={} ({ref_fstype}) vs target={} ({tgt_fstype}, strict_posix={strict_posix})",
        catalog.len(),
        ref_base.display(),
        mountpoint.display()
    );

    let mut rows_json = Vec::new();
    let mut fam_groups: BTreeMap<String, Vec<(String, String, String, String, String)>> = BTreeMap::new();
    let (mut passes, mut fails, mut infos) = (0usize, 0usize, 0usize);
    let mut table_lines = vec![
        format!("reference root: {} ({ref_fstype})", ref_root.display()),
        format!("target root: {}{}", tgt_root.display(), args.suffix),
        String::new(),
        "| case | class | ref | target | verdict | note |".into(),
        "| --- | --- | --- | --- | --- | --- |".into(),
    ];

    for pc in &catalog {
        let ref_obs = run_side(&ref_root, &ref_ctx, pc);
        let tgt_obs = run_side(&tgt_root, &tgt_ctx, pc);
        let (verdict, detail) = evaluate(
            pc.id,
            pc.cls,
            &ref_obs,
            &tgt_obs,
            args.diverge_as_record,
            strict_posix,
            is_root,
        );
        match verdict {
            "FAIL" => fails += 1,
            "INFO" => infos += 1,
            _ => passes += 1,
        }
        let v_disp = if detail.is_empty() {
            verdict.to_string()
        } else {
            format!("{verdict} [{detail}]")
        };
        table_lines.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            pc.id,
            pc.cls.label(),
            ref_obs.fmt(),
            tgt_obs.fmt(),
            v_disp,
            pc.note
        ));
        fam_groups.entry(pc.fam.to_string()).or_default().push((
            pc.id.to_string(),
            verdict.to_string(),
            ref_obs.fmt(),
            tgt_obs.fmt(),
            detail.clone(),
        ));
        rows_json.push(json!({
            "id": pc.id,
            "family": pc.fam,
            "class": pc.cls.label(),
            "ref": ref_obs.fmt(),
            "target": tgt_obs.fmt(),
            "verdict": verdict,
            "detail": detail,
            "note": pc.note,
        }));
    }
    table_lines.push(String::new());
    table_lines.push(format!(
        "total={} pass={passes} fail={fails} info={infos}",
        catalog.len()
    ));
    let table_text = table_lines.join("\n");

    if let Some(tp) = &args.table_out
        && let Err(e) = fs::write(tp, format!("{table_text}\n"))
    {
        warn!("errno-diff: cannot write table {}: {e}", tp.display());
    }

    remove_dir_all_bounded(&ref_root, Duration::from_secs(10));
    remove_dir_all_bounded(&tgt_root, Duration::from_secs(20));
    if let Some(ra) = ref_alt {
        remove_dir_all_bounded(&ra, Duration::from_secs(5));
    }
    if let Some(tas) = tgt_alt_scratch {
        remove_dir_all_bounded(&tas, Duration::from_secs(10));
    }
    if alt_created_parent && let Some(ta) = &args.alt_root {
        let _ = fs::remove_dir(ta);
    }
    if let Some(rg) = ref_gitws {
        remove_dir_all_bounded(&rg, Duration::from_secs(5));
    }
    if let Some(tg) = &args.git_ws_root {
        let _ = fs::remove_file(tg.join(".git/hl-out"));
    }

    let mut cases = Vec::new();
    for (fam, frows) in &fam_groups {
        let fam_fails: Vec<String> = frows
            .iter()
            .filter(|(_, v, _, _, _)| v == "FAIL")
            .map(|(id, _, r, t, d)| format!("{id} ref={r} target={t} {d}"))
            .collect();
        let fam_passes = frows.iter().filter(|(_, v, _, _, _)| v == "PASS").count();
        let id = format!("family-{fam}{}", args.suffix);
        if !fam_fails.is_empty() {
            cases.push(CaseEntry {
                id,
                outcome: "fail".into(),
                detail: fam_fails.join("; "),
            });
        } else if fam_passes > 0 {
            cases.push(CaseEntry {
                id,
                outcome: "pass".into(),
                detail: String::new(),
            });
        } else {
            let recs: Vec<String> = frows
                .iter()
                .map(|(cid, _, r, t, _)| format!("{cid}: ref={r} target={t}"))
                .collect();
            cases.push(CaseEntry {
                id,
                outcome: "skip".into(),
                detail: format!("record-only: {}", recs.join(", ")),
            });
        }
    }
    cases.push(CaseEntry {
        id: format!("summary{}", args.suffix),
        outcome: if fails == 0 { "pass".into() } else { "fail".into() },
        detail: format!("total={} pass={passes} fail={fails} info={infos}", catalog.len()),
    });

    let duration_ms = t0.elapsed().as_millis() as u64;
    info!(
        "errno-diff: total={} pass={} fail={} info={} in {:.2}s",
        catalog.len(),
        passes,
        fails,
        infos,
        duration_ms as f64 / 1000.0
    );

    Ok(json!({
        "suite": "errno-diff",
        "host": host_name(),
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "durationMs": duration_ms,
        "params": {
            "ref_base": ref_base.display().to_string(),
            "ref_fstype": ref_fstype,
            "target_fstype": tgt_fstype,
            "require_ext4": args.require_ext4,
            "diverge_as_record": args.diverge_as_record,
            "strict_posix": strict_posix,
            "suffix": args.suffix,
            "alt_root": args.alt_root.as_ref().map(|p| p.display().to_string()),
            "git_ws_root": args.git_ws_root.as_ref().map(|p| p.display().to_string()),
        },
        "status": if fails == 0 { "ok" } else { "failed" },
        "summary": {
            "total": catalog.len(),
            "pass": passes,
            "fail": fails,
            "info": infos,
        },
        "rows": rows_json,
        "table": table_text,
        "cases": cases,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn errno_diff_passes_on_local_filesystem_and_preserves_existing_alt_root() {
        let base = std::env::temp_dir().join(format!("fstest-errnodiff-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let tgt = base.join("tgt");
        let alt = base.join("preexisting-alt");
        fs::create_dir_all(&tgt).unwrap();
        fs::create_dir_all(&alt).unwrap();

        let cli = TestCli::parse_from([
            "errno-diff",
            "--ref-dir",
            base.to_str().unwrap(),
            "--families",
            "A,B,C,H,M",
        ]);
        let rep = run(&tgt, &cli.args).expect("errno-diff");
        assert_eq!(rep["status"], json!("ok"), "rep: {rep}");
        assert_eq!(rep["summary"]["fail"], json!(0));

        // Verify a pre-existing `--alt-root` directory is never deleted by cleanup.
        let cli_alt = TestCli::parse_from([
            "errno-diff",
            "--ref-dir",
            base.to_str().unwrap(),
            "--alt-root",
            alt.to_str().unwrap(),
            "--families",
            "A",
        ]);
        let _ = run(&tgt, &cli_alt.args).unwrap();
        assert!(alt.is_dir(), "pre-existing alt_root must not be removed");

        let _ = fs::remove_dir_all(&base);
    }
}
