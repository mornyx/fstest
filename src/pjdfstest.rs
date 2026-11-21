// pjdfstest POSIX conformance suite (https://github.com/pjd/pjdfstest, BSD-2-Clause).
// Upstream `.t` scripts and `misc.sh` are embedded at compile time; the privileged
// C syscall helper is implemented as `fstest __pjdfstest` inside the same binary.

use crate::common::{
     detect_fstype, exec_with_timeout, host_name, iso8601, posix_fallocate_fd,
    remove_dir_all_bounded, tail_utf8,
};
use log::{info, warn};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

include!(concat!(env!("OUT_DIR"), "/pjdfstest_files.rs"));

fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8))
        .unwrap_or(4)
}

#[derive(clap::Args)]
pub struct Args {
    /// only run test files whose relative path contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// override the detected filesystem type passed to `supported()` (e.g. FUSE.JUICEFS, APFS, EXT4)
    #[arg(long)]
    pub fs: Option<String>,

    /// override the detected OS name (Darwin, Linux, FreeBSD)
    #[arg(long)]
    pub os: Option<String>,

    /// parallel test-file worker count
    #[arg(short = 'j', long, default_value_t = default_jobs())]
    pub jobs: usize,

    /// per-test-file timeout in seconds
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// keep per-file scratch directories under <mount>/.fstest-pjdfstest/
    #[arg(long)]
    pub keep: bool,

    /// debug logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// write the enriched JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

pub fn helper_main(argv: &[String]) -> i32 {
    let mut i = 0;
    let mut umsk: Option<u32> = None;
    let mut want_uid: Option<libc::uid_t> = None;
    let mut want_gids: Option<&str> = None;
    while i < argv.len() && argv[i].starts_with('-') && argv[i].len() == 2 {
        if argv[i] == "--" {
            i += 1;
            break;
        }
        match argv[i].as_str() {
            "-U" | "-u" | "-g" => {
                let Some(val) = argv.get(i + 1) else {
                    eprintln!("option {} requires an argument", argv[i]);
                    return 1;
                };
                match argv[i].as_str() {
                    "-U" => umsk = Some(parse_num(val) as u32),
                    "-u" => want_uid = Some(parse_num(val) as libc::uid_t),
                    "-g" => want_gids = Some(val.as_str()),
                    _ => unreachable!(),
                }
                i += 2;
            }
            _ => {
                eprintln!("unknown option {}", argv[i]);
                return 1;
            }
        }
    }
    // Upstream `.t` scripts pass `-u <uid> -g <gid>`; apply group changes before
    // `setuid` drops root, otherwise `setgroups`/`setegid` fail with EPERM.
    if let Some(spec) = want_gids {
        if let Err(e) = set_gids(spec) {
            eprintln!("cannot change groups: {e}");
            return 1;
        }
    }
    if let Some(uid) = want_uid {
        if unsafe { libc::setuid(uid) } < 0 {
            eprintln!("cannot change uid: {}", std::io::Error::last_os_error());
            return 1;
        }
    }
    unsafe { libc::umask(umsk.unwrap_or(0) as libc::mode_t) };
    let mut fds: Vec<RawFd> = Vec::new();
    loop {
        let Some(name) = argv.get(i) else { break };
        i += 1;
        let mut sargs: Vec<&str> = Vec::new();
        while let Some(a) = argv.get(i) {
            if a == ":" {
                break;
            }
            sargs.push(a);
            i += 1;
        }
        i += 1;
        match exec_syscall(name, &sargs, &mut fds) {
            Ok(out) => println!("{out}"),
            Err(errno) => {
                println!("{}", errno_name(errno));
                return 1;
            }
        }
        if i > argv.len() {
            break;
        }
    }
    0
}

fn set_gids(spec: &str) -> Result<(), String> {
    let gids: Vec<libc::gid_t> = spec.split(',').map(|g| parse_num(g) as libc::gid_t).collect();
    if unsafe { libc::setgroups(gids.len() as _, gids.as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if unsafe { libc::setegid(gids[0]) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn parse_num(s: &str) -> i64 {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).unwrap_or(0)
    } else if s.len() > 1 && s.starts_with('0') {
        i64::from_str_radix(&s[1..], 8).unwrap_or(0)
    } else {
        s.parse().unwrap_or(0)
    }
}

fn set_errno_zero() {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
}

fn errno_name(e: i32) -> String {
    let n = match e {
        x if x == libc::EPERM => "EPERM",
        x if x == libc::ENOENT => "ENOENT",
        x if x == libc::ESRCH => "ESRCH",
        x if x == libc::EINTR => "EINTR",
        x if x == libc::EIO => "EIO",
        x if x == libc::ENXIO => "ENXIO",
        x if x == libc::E2BIG => "E2BIG",
        x if x == libc::ENOEXEC => "ENOEXEC",
        x if x == libc::EBADF => "EBADF",
        x if x == libc::ECHILD => "ECHILD",
        x if x == libc::EAGAIN => "EAGAIN",
        x if x == libc::ENOMEM => "ENOMEM",
        x if x == libc::EACCES => "EACCES",
        x if x == libc::EFAULT => "EFAULT",
        x if x == libc::EBUSY => "EBUSY",
        x if x == libc::EEXIST => "EEXIST",
        x if x == libc::EXDEV => "EXDEV",
        x if x == libc::ENODEV => "ENODEV",
        x if x == libc::ENOTDIR => "ENOTDIR",
        x if x == libc::EISDIR => "EISDIR",
        x if x == libc::EINVAL => "EINVAL",
        x if x == libc::ENFILE => "ENFILE",
        x if x == libc::EMFILE => "EMFILE",
        x if x == libc::ENOTTY => "ENOTTY",
        x if x == libc::ETXTBSY => "ETXTBSY",
        x if x == libc::EFBIG => "EFBIG",
        x if x == libc::ENOSPC => "ENOSPC",
        x if x == libc::ESPIPE => "ESPIPE",
        x if x == libc::EROFS => "EROFS",
        x if x == libc::EMLINK => "EMLINK",
        x if x == libc::EPIPE => "EPIPE",
        x if x == libc::EDOM => "EDOM",
        x if x == libc::ERANGE => "ERANGE",
        x if x == libc::EDEADLK => "EDEADLK",
        x if x == libc::ENAMETOOLONG => "ENAMETOOLONG",
        x if x == libc::ENOLCK => "ENOLCK",
        x if x == libc::ENOSYS => "ENOSYS",
        x if x == libc::ENOTEMPTY => "ENOTEMPTY",
        x if x == libc::ELOOP => "ELOOP",
        x if x == libc::EWOULDBLOCK => "EWOULDBLOCK",
        x if x == libc::EOPNOTSUPP => "EOPNOTSUPP",
        #[cfg(not(target_os = "macos"))]
        x if x == libc::ENOTSUP => "ENOTSUP",
        x if x == libc::EDQUOT => "EDQUOT",
        x if x == libc::EMULTIHOP => "EMULTIHOP",
        x if x == libc::ENOLINK => "ENOLINK",
        x if x == libc::ESTALE => "ESTALE",
        x if x == libc::ENOTSOCK => "ENOTSOCK",
        x if x == libc::EADDRINUSE => "EADDRINUSE",
        x if x == libc::ECONNREFUSED => "ECONNREFUSED",
        x if x == libc::ENOMSG => "ENOMSG",
        x if x == libc::EIDRM => "EIDRM",
        x if x == libc::EOVERFLOW => "EOVERFLOW",
        x if x == libc::EILSEQ => "EILSEQ",
        _ => return format!("{e}"),
    };
    n.to_string()
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

fn cstr(p: &str) -> std::ffi::CString {
    std::ffi::CString::new(p).unwrap_or_default()
}

// Protocol sentinel pointers: "NULL" -> null, "DEADCODE" -> 0xdeadc0de (EFAULT probe).
fn cptr(p: &str) -> *const libc::c_char {
    match p {
        "NULL" => std::ptr::null(),
        "DEADCODE" => 0xdeadc0de as *const libc::c_char,
        _ => cstr(p).into_raw(),
    }
}

fn parse_at_flag(s: &str) -> i32 {
    let mut out = 0i32;
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() || part == "0" {
            continue;
        }
        let v = match part {
            "AT_SYMLINK_NOFOLLOW" => libc::AT_SYMLINK_NOFOLLOW,
            "AT_SYMLINK_FOLLOW" => libc::AT_SYMLINK_FOLLOW,
            "AT_REMOVEDIR" => libc::AT_REMOVEDIR,
            #[cfg(target_os = "linux")]
            "AT_EMPTY_PATH" => 0x1000,
            _ => parse_num(part) as i32,
        };
        out |= v;
    }
    out
}

#[cfg(target_os = "macos")]
fn parse_chflags_mask(s: &str) -> Result<libc::c_uint, i32> {
    if s == "none" || s == "0" {
        return Ok(0);
    }
    let mut flags: libc::c_uint = 0;
    for tok in s.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let bit: libc::c_uint = match tok {
            "UF_NODUMP" => libc::UF_NODUMP,
            "UF_IMMUTABLE" => libc::UF_IMMUTABLE,
            "UF_APPEND" => libc::UF_APPEND,
            "UF_OPAQUE" => libc::UF_OPAQUE,
            "UF_HIDDEN" => libc::UF_HIDDEN,
            "SF_ARCHIVED" => libc::SF_ARCHIVED,
            "SF_IMMUTABLE" => libc::SF_IMMUTABLE,
            "SF_APPEND" => libc::SF_APPEND,
            "UF_NOUNLINK" => 0x0000_0010,
            "SF_NOUNLINK" => 0x0010_0000,
            _ => parse_num(tok) as libc::c_uint,
        };
        flags |= bit;
    }
    Ok(flags)
}

#[cfg(target_os = "macos")]
fn format_chflags(flags: u32) -> String {
    if flags == 0 {
        return "none".into();
    }
    let table: &[(u32, &str)] = &[
        (libc::UF_NODUMP, "UF_NODUMP"),
        (libc::UF_IMMUTABLE, "UF_IMMUTABLE"),
        (libc::UF_APPEND, "UF_APPEND"),
        (0x0000_0010, "UF_NOUNLINK"),
        (libc::UF_OPAQUE, "UF_OPAQUE"),
        (libc::UF_HIDDEN, "UF_HIDDEN"),
        (libc::SF_ARCHIVED, "SF_ARCHIVED"),
        (libc::SF_IMMUTABLE, "SF_IMMUTABLE"),
        (libc::SF_APPEND, "SF_APPEND"),
        (0x0010_0000, "SF_NOUNLINK"),
    ];
    let mut names = Vec::new();
    for &(bit, label) in table {
        if flags & bit != 0 {
            names.push(label);
        }
    }
    if names.is_empty() {
        format!("0x{flags:x}")
    } else {
        names.join(",")
    }
}

pub(crate) fn exec_syscall(name: &str, a: &[&str], fds: &mut Vec<RawFd>) -> Result<String, i32> {
    let arg = |i: usize| -> &str { a.get(i).copied().unwrap_or("") };
    let num = |i: usize| -> i64 { parse_num(arg(i)) };
    match name {
        "open" | "openat" => {
            let (dirfd, path, flags_str, mode_idx) = if name == "openat" {
                (dirfd_get(fds, arg(0))?, arg(1), arg(2), 3)
            } else {
                (libc::AT_FDCWD, arg(0), arg(1), 2)
            };
            let flags = parse_open_flags(flags_str)?;
            #[cfg(target_os = "linux")]
            let flags = flags | libc::O_CLOEXEC;
            let fd = if flags & libc::O_CREAT != 0 {
                unsafe { libc::openat(dirfd, cptr(path), flags, num(mode_idx) as libc::c_uint) }
            } else {
                unsafe { libc::openat(dirfd, cptr(path), flags) }
            };
            if fd < 0 {
                return Err(last_errno());
            }
            fds.push(fd);
            Ok("0".into())
        }
        "create" => {
            let fd = unsafe {
                libc::open(
                    cptr(arg(0)),
                    libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
                    num(1) as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(last_errno());
            }
            unsafe { libc::close(fd) };
            Ok("0".into())
        }
        "unlink" => cvt(unsafe { libc::unlink(cptr(arg(0))) }),
        "unlinkat" => {
            let flag = parse_at_flag(arg(2));
            cvt(unsafe { libc::unlinkat(dirfd_get(fds, arg(0))?, cptr(arg(1)), flag) })
        }
        "mkdir" => cvt(unsafe { libc::mkdir(cptr(arg(0)), num(1) as libc::mode_t) }),
        "mkdirat" => cvt(unsafe {
            libc::mkdirat(dirfd_get(fds, arg(0))?, cptr(arg(1)), num(2) as libc::mode_t)
        }),
        "rmdir" => cvt(unsafe { libc::rmdir(cptr(arg(0))) }),
        "link" => cvt(unsafe { libc::link(cptr(arg(0)), cptr(arg(1))) }),
        "linkat" => {
            let flag = parse_at_flag(arg(4));
            cvt(unsafe {
                libc::linkat(
                    dirfd_get(fds, arg(0))?,
                    cptr(arg(1)),
                    dirfd_get(fds, arg(2))?,
                    cptr(arg(3)),
                    flag,
                )
            })
        }
        "symlink" => cvt(unsafe { libc::symlink(cptr(arg(0)), cptr(arg(1))) }),
        "symlinkat" => cvt(unsafe {
            libc::symlinkat(cptr(arg(0)), dirfd_get(fds, arg(1))?, cptr(arg(2)))
        }),
        "readlink" | "readlinkat" => {
            let mut buf = vec![0u8; 4096];
            let n = if name == "readlinkat" {
                unsafe {
                    libc::readlinkat(
                        dirfd_get(fds, arg(0))?,
                        cptr(arg(1)),
                        buf.as_mut_ptr() as *mut libc::c_char,
                        buf.len(),
                    )
                }
            } else {
                unsafe {
                    libc::readlink(
                        cptr(arg(0)),
                        buf.as_mut_ptr() as *mut libc::c_char,
                        buf.len(),
                    )
                }
            };
            if n < 0 {
                return Err(last_errno());
            }
            Ok(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
        }
        "rename" => cvt(unsafe { libc::rename(cptr(arg(0)), cptr(arg(1))) }),
        "renameat" => cvt(unsafe {
            libc::renameat(
                dirfd_get(fds, arg(0))?,
                cptr(arg(1)),
                dirfd_get(fds, arg(2))?,
                cptr(arg(3)),
            )
        }),
        "renameat2" => {
            let mut rflags = 0u32;
            for tok in arg(4).split(',') {
                let tok = tok.trim();
                if tok.is_empty() || tok == "0" {
                    continue;
                }
                rflags |= match tok {
                    "RENAME_NOREPLACE" => 1,
                    "RENAME_EXCHANGE" => 2,
                    "RENAME_WHITEOUT" => 4,
                    _ => parse_num(tok) as u32,
                };
            }
            #[cfg(target_os = "linux")]
            {
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        dirfd_get(fds, arg(0))?,
                        cptr(arg(1)),
                        dirfd_get(fds, arg(2))?,
                        cptr(arg(3)),
                        rflags as libc::c_uint,
                    )
                };
                cvt_ssize(rc as isize)
            }
            #[cfg(target_os = "macos")]
            {
                let mut dflags: libc::c_uint = 0;
                if rflags & 1 != 0 {
                    dflags |= libc::RENAME_EXCL;
                }
                if rflags & 2 != 0 {
                    dflags |= libc::RENAME_SWAP;
                }
                if rflags & !3 != 0 {
                    return Err(libc::EINVAL);
                }
                cvt(unsafe {
                    libc::renameatx_np(
                        dirfd_get(fds, arg(0))?,
                        cptr(arg(1)),
                        dirfd_get(fds, arg(2))?,
                        cptr(arg(3)),
                        dflags,
                    )
                })
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = rflags;
                Err(libc::ENOSYS)
            }
        }
        "mkfifo" => cvt(unsafe { libc::mkfifo(cptr(arg(0)), num(1) as libc::mode_t) }),
        "mkfifoat" => cvt(unsafe {
            libc::mkfifoat(dirfd_get(fds, arg(0))?, cptr(arg(1)), num(2) as libc::mode_t)
        }),
        "mknod" | "mknodat" => {
            let base = if name == "mknodat" { 1 } else { 0 };
            let ftype = match arg(base + 1) {
                "b" => libc::S_IFBLK,
                "c" | "u" => libc::S_IFCHR,
                "f" => libc::S_IFIFO,
                "s" => libc::S_IFSOCK,
                _ => libc::S_IFREG,
            };
            let mode = (parse_num(arg(base + 2)) as libc::mode_t) | ftype;
            let dev = makedev(parse_num(arg(base + 3)), parse_num(arg(base + 4)));
            let r = if name == "mknodat" {
                unsafe { libc::mknodat(dirfd_get(fds, arg(0))?, cptr(arg(1)), mode, dev) }
            } else {
                unsafe { libc::mknod(cptr(arg(0)), mode, dev) }
            };
            cvt(r)
        }
        "bind" | "connect" => {
            let path = arg(0);
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(last_errno());
            }
            let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = path.as_bytes();
            if bytes.len() >= sa.sun_path.len() {
                unsafe { libc::close(fd) };
                return Err(libc::ENAMETOOLONG);
            }
            let len = 2 + bytes.len();
            #[cfg(target_os = "macos")]
            {
                sa.sun_len = len as u8;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    sa.sun_path.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
                let r = if name == "bind" {
                    libc::bind(fd, &sa as *const _ as *const libc::sockaddr, len as libc::socklen_t)
                } else {
                    libc::connect(fd, &sa as *const _ as *const libc::sockaddr, len as libc::socklen_t)
                };
                if r < 0 {
                    let e = last_errno();
                    libc::close(fd);
                    return Err(e);
                }
                libc::close(fd);
            }
            Ok("0".into())
        }
        "chmod" => cvt(unsafe { libc::chmod(cptr(arg(0)), num(1) as libc::mode_t) }),
        "lchmod" => {
            #[cfg(target_os = "macos")]
            {
                cvt(unsafe {
                    libc::fchmodat(
                        libc::AT_FDCWD,
                        cptr(arg(0)),
                        num(1) as libc::mode_t,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(libc::ENOSYS)
            }
        }
        "fchmod" => cvt(unsafe { libc::fchmod(fd_get(fds, num(0))?, num(1) as libc::mode_t) }),
        "fchmodat" => {
            let flag = parse_at_flag(arg(3));
            cvt(unsafe {
                libc::fchmodat(
                    dirfd_get(fds, arg(0))?,
                    cptr(arg(1)),
                    num(2) as libc::mode_t,
                    flag,
                )
            })
        }
        "chflags" | "lchflags" | "fchflags" => {
            #[cfg(target_os = "macos")]
            {
                unsafe extern "C" {
                    fn lchflags(path: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
                }
                let mask = parse_chflags_mask(arg(1))?;
                let rc = match name {
                    "chflags" => unsafe { libc::chflags(cptr(arg(0)), mask) },
                    "lchflags" => unsafe { lchflags(cptr(arg(0)), mask) },
                    _ => unsafe { libc::fchflags(fd_get(fds, num(0))?, mask) },
                };
                cvt(rc)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(libc::ENOSYS)
            }
        }
        "chown" => cvt(unsafe {
            libc::chown(cptr(arg(0)), num(1) as libc::uid_t, num(2) as libc::gid_t)
        }),
        "lchown" => cvt(unsafe {
            libc::lchown(cptr(arg(0)), num(1) as libc::uid_t, num(2) as libc::gid_t)
        }),
        "fchown" => cvt(unsafe {
            libc::fchown(
                fd_get(fds, num(0))?,
                num(1) as libc::uid_t,
                num(2) as libc::gid_t,
            )
        }),
        "fchownat" => {
            let flag = parse_at_flag(arg(4));
            cvt(unsafe {
                libc::fchownat(
                    dirfd_get(fds, arg(0))?,
                    cptr(arg(1)),
                    num(2) as libc::uid_t,
                    num(3) as libc::gid_t,
                    flag,
                )
            })
        }
        "access" | "faccessat" => {
            let parse_amode = |s: &str| -> i32 {
                let mut m = 0;
                for tok in s.split(',') {
                    m |= match tok.trim() {
                        "F_OK" => libc::F_OK,
                        "R_OK" => libc::R_OK,
                        "W_OK" => libc::W_OK,
                        "X_OK" => libc::X_OK,
                        other => parse_num(other) as i32,
                    };
                }
                m
            };
            if name == "faccessat" {
                cvt(unsafe {
                    libc::faccessat(
                        dirfd_get(fds, arg(0))?,
                        cptr(arg(1)),
                        parse_amode(arg(2)),
                        parse_at_flag(arg(3)),
                    )
                })
            } else {
                cvt(unsafe { libc::access(cptr(arg(0)), parse_amode(arg(1))) })
            }
        }
        "truncate" => cvt(unsafe { libc::truncate(cptr(arg(0)), num(1) as libc::off_t) }),
        "ftruncate" => cvt(unsafe {
            libc::ftruncate(fd_get(fds, num(0))?, num(1) as libc::off_t)
        }),
        "posix_fallocate" => {
            posix_fallocate_fd(fd_get(fds, num(0))?, num(1), num(2))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            Ok("0".into())
        }
        "stat" | "lstat" | "fstat" | "fstatat" => {
            let mut sb: libc::stat = unsafe { std::mem::zeroed() };
            let (r, what) = match name {
                "stat" => (unsafe { libc::stat(cptr(arg(0)), &mut sb) }, arg(1)),
                "lstat" => (unsafe { libc::lstat(cptr(arg(0)), &mut sb) }, arg(1)),
                "fstat" => (unsafe { libc::fstat(fd_get(fds, num(0))?, &mut sb) }, arg(1)),
                _ => {
                    let flag = parse_at_flag(arg(3));
                    (
                        unsafe {
                            libc::fstatat(dirfd_get(fds, arg(0))?, cptr(arg(1)), &mut sb, flag)
                        },
                        arg(2),
                    )
                }
            };
            if r < 0 {
                return Err(last_errno());
            }
            Ok(show_stats(&sb, what))
        }
        "pathconf" | "fpathconf" => {
            let pc = pathconf_id(arg(1));
            set_errno_zero();
            let r = if name == "fpathconf" {
                unsafe { libc::fpathconf(fd_get(fds, num(0))?, pc) }
            } else {
                unsafe { libc::pathconf(cptr(arg(0)), pc) }
            };
            if r < 0 {
                let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if e != 0 {
                    return Err(e);
                }
            }
            Ok(format!("{r}"))
        }
        "lpathconf" => Err(libc::ENOSYS),
        "utimensat" => {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let times = [timespec_arg(num(2), arg(3)), timespec_arg(num(4), arg(5))];
                let flag = parse_at_flag(arg(6));
                let r = unsafe {
                    libc::utimensat(dirfd_get(fds, arg(0))?, cptr(arg(1)), times.as_ptr(), flag)
                };
                cvt(r)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                Err(libc::ENOSYS)
            }
        }
        "futimens" => {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let times = [timespec_arg(num(1), arg(2)), timespec_arg(num(3), arg(4))];
                cvt(unsafe { libc::futimens(fd_get(fds, num(0))?, times.as_ptr()) })
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                Err(libc::ENOSYS)
            }
        }
        "write" => {
            let data = arg(1).as_bytes();
            let r = unsafe {
                libc::write(
                    fd_get(fds, num(0))?,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                )
            };
            cvt_ssize(r)
        }
        "pwrite" => {
            let data = arg(1).as_bytes();
            let r = unsafe {
                libc::pwrite(
                    fd_get(fds, num(0))?,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    num(2) as libc::off_t,
                )
            };
            cvt_ssize(r)
        }
        "pread" => {
            let fd = fd_get(fds, num(0))?;
            let count = (num(1) as usize).min(65536);
            let off = num(2) as libc::off_t;
            let mut buf = vec![0u8; count];
            let mut got = 0usize;
            while got < count {
                let r = unsafe {
                    libc::pread(
                        fd,
                        buf[got..].as_mut_ptr() as *mut libc::c_void,
                        count - got,
                        off + got as libc::off_t,
                    )
                };
                if r < 0 {
                    return Err(last_errno());
                }
                if r == 0 {
                    break;
                }
                got += r as usize;
            }
            Ok(String::from_utf8_lossy(&buf[..got]).to_string())
        }
        "fsync" => cvt(unsafe { libc::fsync(fd_get(fds, num(0))?) }),
        "close" => cvt(unsafe { libc::close(fd_get(fds, num(0))?) }),
        "mksyscalls" => Ok(String::new()),
        "randhex" => {
            let nchars = a.first().map(|s| parse_num(s) as usize).unwrap_or(32).max(1);
            let nbytes = nchars.div_ceil(2);
            let mut buf = vec![0u8; nbytes];
            if let Ok(mut f) = fs::File::open("/dev/urandom") {
                use std::io::Read;
                let _ = f.read_exact(&mut buf);
            } else {
                let mut rng = crate::common::SplitMix64::new(
                    (std::process::id() as u64)
                        ^ SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0x9e37_79b9_7f4a_7c15),
                );
                rng.fill_bytes(&mut buf);
            }
            let mut hex_str = hex::encode(&buf);
            hex_str.truncate(nchars);
            Ok(hex_str)
        }
        _ => {
            eprintln!("syscall '{name}' not supported");
            Err(libc::ENOSYS)
        }
    }
}

fn cvt(r: i32) -> Result<String, i32> {
    if r < 0 { Err(last_errno()) } else { Ok("0".into()) }
}

fn cvt_ssize(r: isize) -> Result<String, i32> {
    if r < 0 { Err(last_errno()) } else { Ok("0".into()) }
}

fn fd_get(fds: &[RawFd], pos: i64) -> Result<RawFd, i32> {
    if pos < 0 {
        return Err(libc::EBADF);
    }
    fds.get(pos as usize).copied().ok_or(libc::EBADF)
}

fn dirfd_get(fds: &[RawFd], s: &str) -> Result<RawFd, i32> {
    if s == "AT_FDCWD" {
        Ok(libc::AT_FDCWD)
    } else {
        fd_get(fds, parse_num(s))
    }
}

fn makedev(major: i64, minor: i64) -> libc::dev_t {
    #[cfg(target_os = "linux")]
    {
        libc::makedev(major as libc::c_uint, minor as libc::c_uint)
    }
    #[cfg(not(target_os = "linux"))]
    {
        ((((major as u32) & 0xff) << 24) | ((minor as u32) & 0x00ff_ffff)) as libc::dev_t
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn timespec_arg(sec: i64, nsec_str: &str) -> libc::timespec {
    let nsec: libc::c_long = if nsec_str == "UTIME_NOW" {
        libc::UTIME_NOW as libc::c_long
    } else if nsec_str == "UTIME_OMIT" {
        libc::UTIME_OMIT as libc::c_long
    } else {
        parse_num(nsec_str) as libc::c_long
    };
    libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec,
    }
}

fn parse_open_flags(s: &str) -> Result<i32, i32> {
    let mut flags = 0i32;
    for f in s.split(',') {
        let f = f.trim();
        if f.is_empty() || f == "0" {
            continue;
        }
        let v = match f {
            "O_RDONLY" => libc::O_RDONLY,
            "O_WRONLY" => libc::O_WRONLY,
            "O_RDWR" => libc::O_RDWR,
            "O_NONBLOCK" => libc::O_NONBLOCK,
            "O_APPEND" => libc::O_APPEND,
            "O_CREAT" => libc::O_CREAT,
            "O_TRUNC" => libc::O_TRUNC,
            "O_EXCL" => libc::O_EXCL,
            "O_NOFOLLOW" => libc::O_NOFOLLOW,
            "O_SYNC" => libc::O_SYNC,
            "O_DSYNC" => libc::O_DSYNC,
            "O_DIRECTORY" => libc::O_DIRECTORY,
            "O_NOCTTY" => libc::O_NOCTTY,
            "O_CLOEXEC" => libc::O_CLOEXEC,
            #[cfg(target_os = "macos")]
            "O_SHLOCK" => libc::O_SHLOCK,
            #[cfg(target_os = "macos")]
            "O_EXLOCK" => libc::O_EXLOCK,
            #[cfg(target_os = "macos")]
            "O_EVTONLY" => libc::O_EVTONLY,
            #[cfg(target_os = "macos")]
            "O_SYMLINK" => libc::O_SYMLINK,
            #[cfg(not(target_os = "macos"))]
            "O_SHLOCK" | "O_EXLOCK" | "O_EVTONLY" | "O_SYMLINK" => 0,
            #[cfg(target_os = "linux")]
            "O_DIRECT" => libc::O_DIRECT,
            #[cfg(target_os = "linux")]
            "O_PATH" => libc::O_PATH,
            _ => {
                eprintln!("unknown open flag {f}");
                return Err(libc::EINVAL);
            }
        };
        flags |= v;
    }
    Ok(flags)
}

fn show_stats(sb: &libc::stat, what: &str) -> String {
    let mut out = Vec::new();
    for w in what.split(',') {
        let v = match w {
            "mode" => format!("0{:o}", sb.st_mode & 0o7777),
            "inode" => format!("{}", sb.st_ino),
            "nlink" => format!("{}", sb.st_nlink),
            "uid" => format!("{}", sb.st_uid),
            "gid" => format!("{}", sb.st_gid),
            "size" => format!("{}", sb.st_size),
            "blocks" => format!("{}", sb.st_blocks),
            "atime" => format!("{}", sb.st_atime),
            "atime_ns" => format!("{}", sb.st_atime_nsec),
            "mtime" => format!("{}", sb.st_mtime),
            "mtime_ns" => format!("{}", sb.st_mtime_nsec),
            "ctime" => format!("{}", sb.st_ctime),
            "ctime_ns" => format!("{}", sb.st_ctime_nsec),
            "birthtime" => {
                #[cfg(target_os = "macos")]
                {
                    format!("{}", sb.st_birthtime)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    "unknown".into()
                }
            }
            "birthtime_ns" => {
                #[cfg(target_os = "macos")]
                {
                    format!("{}", sb.st_birthtime_nsec)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    "unknown".into()
                }
            }
            "flags" => {
                #[cfg(target_os = "macos")]
                {
                    format_chflags(sb.st_flags)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    "none".into()
                }
            }
            "major" => {
                #[cfg(target_os = "linux")]
                {
                    format!("{}", libc::major(sb.st_rdev))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    format!("{}", ((sb.st_rdev as u32) >> 24) & 0xff)
                }
            }
            "minor" => {
                #[cfg(target_os = "linux")]
                {
                    format!("{}", libc::minor(sb.st_rdev))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    format!("{}", (sb.st_rdev as u32) & 0x00ff_ffff)
                }
            }
            "type" => match sb.st_mode & libc::S_IFMT {
                libc::S_IFIFO => "fifo".into(),
                libc::S_IFCHR => "char".into(),
                libc::S_IFDIR => "dir".into(),
                libc::S_IFBLK => "block".into(),
                libc::S_IFREG => "regular".into(),
                libc::S_IFLNK => "symlink".into(),
                libc::S_IFSOCK => "socket".into(),
                _ => "unknown".into(),
            },
            _ => "unknown".into(),
        };
        out.push(v);
    }
    out.join(",")
}

fn pathconf_id(name: &str) -> i32 {
    match name {
        "_PC_LINK_MAX" => libc::_PC_LINK_MAX,
        "_PC_NAME_MAX" => libc::_PC_NAME_MAX,
        "_PC_PATH_MAX" => libc::_PC_PATH_MAX,
        "_PC_SYMLINK_MAX" => libc::_PC_SYMLINK_MAX,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        "_PC_NO_TRUNC" => libc::_PC_NO_TRUNC,
        _ => libc::_PC_NAME_MAX,
    }
}

struct FileResult {
    name: String,
    tests: u64,
    passed: u64,
    failed: u64,
    todo: u64,
    skipped: bool,
    skip_reason: String,
    failures: Vec<String>,
    bail: bool,
    stderr_tail: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

fn detect_os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "freebsd") {
        "FreeBSD"
    } else {
        "Linux"
    }
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint)
        .map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own executable: {e}"))?;
    let misc = embedded("misc.sh").ok_or("misc.sh missing from embedded suite")?;
    let misc_body = misc
        .splitn(2, "\nrequires_root()")
        .nth(1)
        .ok_or("unexpected misc.sh layout")?;

    let resolved_os = args
        .os
        .clone()
        .unwrap_or_else(|| detect_os_name().to_string());
    let resolved_fs = args
        .fs
        .clone()
        .unwrap_or_else(|| detect_fstype(mountpoint).to_ascii_uppercase());

    let selected: Vec<&(&str, &str)> = TEST_FILES
        .iter()
        .filter(|(name, _)| name.ends_with(".t"))
        .filter(|(name, _)| args.filter.is_empty() || args.filter.iter().any(|f| name.contains(f)))
        .collect();

    let jobs = args.jobs.max(1).min(selected.len().max(1));
    info!(
        "pjdfstest: {} test files selected, jobs={jobs}, top={}, os={resolved_os}, fs={resolved_fs}",
        selected.len(),
        mountpoint.display(),
    );
    if !nix_euid_is_root() {
        warn!("not running as root: chown/mknod/permission cases will fail (upstream behaves the same)");
    }

    let started = SystemTime::now();
    let t0 = Instant::now();
    let scratch_root = mountpoint.join(".fstest-pjdfstest");
    fs::create_dir_all(&scratch_root).map_err(|e| e.to_string())?;

    let cursor = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<FileResult>>> = (0..selected.len()).map(|_| Mutex::new(None)).collect();

    thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    if idx >= selected.len() {
                        break;
                    }
                    let (name, body) = selected[idx];
                    let scratch = scratch_root.join(format!("{idx:03}-{}", name.replace('/', "_")));
                    let res = run_one(
                        &scratch,
                        name,
                        body,
                        &resolved_os,
                        &resolved_fs,
                        misc_body,
                        &exe,
                        args,
                    );
                    if !args.keep {
                        remove_dir_all_bounded(&scratch, Duration::from_secs(10));
                    }
                    *slots[idx].lock().unwrap() = Some(res);
                }
            });
        }
    });

    if !args.keep {
        remove_dir_all_bounded(&scratch_root, Duration::from_secs(30));
    }

    let results: Vec<FileResult> = slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect();

    let total_tests: u64 = results.iter().map(|r| r.tests).sum();
    let total_passed: u64 = results.iter().map(|r| r.passed).sum();
    let total_failed: u64 = results.iter().map(|r| r.failed).sum();
    let total_todo: u64 = results.iter().map(|r| r.todo).sum();
    let skipped_files = results.iter().filter(|r| r.skipped).count();
    let bailed = results.iter().any(|r| r.bail);
    let timed_out_any = results.iter().any(|r| r.timed_out);
    let mut failures = Vec::new();
    for r in &results {
        for f in &r.failures {
            failures.push(format!("{}: {f}", r.name));
        }
    }
    let duration_ms = t0.elapsed().as_millis() as u64;
    info!(
        "pjdfstest: files={} skipped={} tests={} failed={} todo={} in {:.2}s",
        results.len(),
        skipped_files,
        total_tests,
        total_failed,
        total_todo,
        duration_ms as f64 / 1000.0,
    );

    Ok(json!({
        "suite": "pjdfstest",
        "host": host_name(),
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "durationMs": duration_ms,
        "params": {
            "files_selected": selected.len(),
            "jobs": jobs,
            "filter": args.filter,
            "os": resolved_os,
            "fs": resolved_fs,
            "timeout_sec": args.timeout,
            "euid": unsafe { libc::geteuid() },
        },
        "status": if total_failed == 0 && !bailed && !timed_out_any { "ok" } else { "failed" },
        "summary": {
            "files": results.len(),
            "skippedFiles": skipped_files,
            "tests": total_tests,
            "passed": total_passed,
            "failed": total_failed,
            "todoPassed": total_todo,
        },
        "failures": failures,
        "files": results.iter().map(|r| json!({
            "name": r.name,
            "tests": r.tests,
            "passed": r.passed,
            "failed": r.failed,
            "todo": r.todo,
            "skipped": r.skipped,
            "skipReason": r.skip_reason,
            "bail": r.bail,
            "timedOut": r.timed_out,
            "exitCode": r.exit_code,
            "failures": r.failures,
            "stderrTail": r.stderr_tail,
        })).collect::<Vec<_>>(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn run_one(
    scratch: &Path,
    name: &str,
    body: &str,
    os_name: &str,
    fs_name: &str,
    misc_body: &str,
    exe: &Path,
    args: &Args,
) -> FileResult {
    let _ = fs::remove_dir_all(scratch);
    let mut r = FileResult {
        name: name.to_string(),
        tests: 0,
        passed: 0,
        failed: 0,
        todo: 0,
        skipped: false,
        skip_reason: String::new(),
        failures: Vec::new(),
        bail: false,
        stderr_tail: String::new(),
        exit_code: None,
        timed_out: false,
    };
    if let Err(e) = fs::create_dir_all(scratch) {
        r.failures.push(format!("cannot create scratch dir: {e}"));
        r.failed = 1;
        r.tests = 1;
        return r;
    }

    let body = body
        .lines()
        .filter(|l| {
            let t = l.trim();
            t != "dir=`dirname $0`" && t != ". ${dir}/../misc.sh"
        })
        .collect::<Vec<_>>()
        .join("\n");

    let esc_exe = exe.display().to_string().replace('\'', "'\\''");
    let esc_os = os_name.replace('\'', "");
    let esc_fs = fs_name.replace('\'', "");

    // Define `fstest` as a shell function and set `fstest=fstest` so both unquoted
    // `${fstest} $*` (in `misc.sh`) and quoted `"$fstest" ...` (in `utimensat/05.t`) work.
    let mut script = String::with_capacity(misc_body.len() + body.len() + 512);
    script.push_str(&format!(
        "ntest=1\nGREP=grep\nos='{esc_os}'\nfs='{esc_fs}'\n\
         fstest() {{ '{esc_exe}' __pjdfstest \"$@\"; }}\nfstest=fstest\n\
         requires_root(){misc_body}\n\
         namegen() {{ printf 'pjdfstest_%s\\n' \"$(${{fstest}} randhex 32)\"; }}\n\
         namegen_len() {{ printf '%s' \"$(${{fstest}} randhex \"$1\")\"; }}\n\
         {body}\n"
    ));

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&script).current_dir(scratch);
    let exec = exec_with_timeout(cmd, Duration::from_secs(args.timeout.max(1)));
    if let Some(err) = exec.spawn_err {
        r.failures.push(format!("cannot spawn sh: {err}"));
        r.tests = 1;
        r.failed = 1;
        return r;
    }
    r.exit_code = exec.code;
    r.timed_out = exec.timed_out;
    r.stderr_tail = tail_utf8(&exec.stderr, 2000);

    parse_tap_output(&exec.stdout, &exec.stderr, args.timeout, &mut r);
    r
}

fn parse_tap_output(stdout: &str, stderr: &str, timeout_sec: u64, r: &mut FileResult) {
    let mut plan: Option<u64> = None;
    for line in stdout.lines() {
        let line = line.trim_end();
        if let Some(after) = line.strip_prefix("1..") {
            let (num_part, comment_part) = match after.split_once('#') {
                Some((n, c)) => (n.trim(), Some(c.trim())),
                None => (after.trim(), None),
            };
            if let Ok(n) = num_part.parse::<u64>() {
                plan = Some(n);
                if n == 0 {
                    r.skipped = true;
                    r.skip_reason = comment_part.unwrap_or("1..0").to_string();
                }
            }
            continue;
        }
        if line.starts_with("Bail out!") {
            r.bail = true;
            r.failures.push(line.to_string());
            continue;
        }
        let (ok, rest) = if let Some(rest) = line.strip_prefix("ok ") {
            (true, rest)
        } else if let Some(rest) = line.strip_prefix("not ok ") {
            (false, rest)
        } else if line == "ok" {
            (true, "")
        } else if line == "not ok" {
            (false, "")
        } else {
            continue;
        };
        let is_todo = rest.contains("# TODO") || rest.contains("# SKIP");
        r.tests += 1;
        if ok {
            r.passed += 1;
            if is_todo {
                r.todo += 1;
            }
        } else if is_todo {
            r.passed += 1;
        } else {
            r.failed += 1;
            r.failures.push(line.to_string());
        }
    }

    if r.timed_out {
        r.failed = r.failed.max(1);
        r.failures.push(format!("timed out after {timeout_sec}s"));
        return;
    }
    if r.skipped {
        return;
    }
    if let Some(n) = plan {
        if n > r.tests {
            let missing = n - r.tests;
            r.failed += missing;
            r.failures.push(format!(
                "script stopped early: declared {n} tests, ran {}",
                r.tests
            ));
        }
    } else if r.tests == 0 {
        r.failed += 1;
        let detail = if !stderr.trim().is_empty() {
            tail_utf8(stderr.trim(), 240)
        } else {
            format!("exit_code={:?} with zero TAP assertions", r.exit_code)
        };
        r.failures.push(format!("no TAP output produced ({detail})"));
    } else if r.exit_code.unwrap_or(1) != 0 && r.failed == 0 {
        r.failed += 1;
        r.failures.push(format!(
            "script exited with {:?} despite passing TAP lines",
            r.exit_code
        ));
    }
}

fn embedded(name: &str) -> Option<&'static str> {
    TEST_FILES
        .iter()
        .find(|(n, _)| n.as_bytes() == name.as_bytes())
        .map(|(_, c)| *c)
}

fn nix_euid_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fstatat_and_at_fdcwd_and_pread_error_work_correctly() {
        let dir = std::env::temp_dir().join(format!("fstest-pjd-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("probe.txt");
        fs::write(&file, b"hello-world").unwrap();

        let mut fds = Vec::new();
        let d_str = dir.to_str().unwrap();
        assert_eq!(exec_syscall("open", &[d_str, "O_RDONLY,O_DIRECTORY"], &mut fds).unwrap(), "0");
        assert_eq!(
            exec_syscall("fstatat", &["0", "probe.txt", "type,size", "0"], &mut fds).unwrap(),
            "regular,11"
        );
        assert_eq!(
            exec_syscall("fstatat", &["AT_FDCWD", file.to_str().unwrap(), "type,size", "0"], &mut fds).unwrap(),
            "regular,11"
        );
        // Reading from directory fd (`0`) or bad fd (`99`) must fail rather than returning `Ok("")`.
        assert_eq!(exec_syscall("pread", &["99", "8", "0"], &mut fds), Err(libc::EBADF));

        let f_str = file.to_str().unwrap();
        assert_eq!(exec_syscall("open", &[f_str, "O_RDWR"], &mut fds).unwrap(), "0");
        assert_eq!(exec_syscall("posix_fallocate", &["1", "1000", "234"], &mut fds).unwrap(), "0");
        assert_eq!(exec_syscall("fstat", &["1", "size"], &mut fds).unwrap(), "1234");

        for fd in fds {
            unsafe { libc::close(fd) };
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tap_parser_catches_skip_and_silent_crash() {
        let mut skip_res = FileResult {
            name: "open/24.t".into(),
            tests: 0,
            passed: 0,
            failed: 0,
            todo: 0,
            skipped: false,
            skip_reason: String::new(),
            failures: Vec::new(),
            bail: false,
            stderr_tail: String::new(),
            exit_code: Some(0),
            timed_out: false,
        };
        parse_tap_output("1..0 # SKIP: unsupported OS\n", "", 30, &mut skip_res);
        assert!(skip_res.skipped);
        assert_eq!(skip_res.failed, 0);

        let mut crash_res = FileResult {
            name: "broken/00.t".into(),
            tests: 0,
            passed: 0,
            failed: 0,
            todo: 0,
            skipped: false,
            skip_reason: String::new(),
            failures: Vec::new(),
            bail: false,
            stderr_tail: String::new(),
            exit_code: Some(1),
            timed_out: false,
        };
        parse_tap_output("", "conf failed", 30, &mut crash_res);
        assert!(!crash_res.skipped);
        assert_eq!(crash_res.failed, 1);
    }
}
