// pjdfstest functional suite (https://github.com/pjd/pjdfstest, BSD-2-Clause).
// The upstream shell tests (tests/*.t + misc.sh + conf) are embedded at compile
// time and piped to `sh -s` at runtime — nothing but the tests' own scratch
// files ever touches the filesystem under test. The privileged-operation helper
// that upstream ships as the setuid `pjdfstest` C binary is reimplemented as a
// hidden `fstest __pjdfstest` mode invoked through the current executable, so
// distribution stays a single binary.

use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

include!(concat!(env!("OUT_DIR"), "/pjdfstest_files.rs"));

#[derive(clap::Args)]
pub struct Args {
    /// only run test files whose relative path contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// override the detected filesystem type passed to the suite's `supported()`
    /// gating (e.g. FUSE.JUICEFS, APFS, EXT4)
    #[arg(long)]
    pub fs: Option<String>,

    /// override the detected OS name (Darwin, Linux, FreeBSD)
    #[arg(long)]
    pub os: Option<String>,

    /// per-test-file timeout in seconds
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// keep the per-file scratch directories (left under <mount>/.fstest-pjdfstest/)
    #[arg(long)]
    pub keep: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// helper protocol: `fstest __pjdfstest [-U umask] [-u uid] [-g gids] syscall args...`
// Multiple syscalls chain in one invocation separated by ":". On success the
// helper prints "0" (stat/pathconf variants print field values); on failure it
// prints the errno name and exits 1 — exactly what upstream's expect() matches.
// ---------------------------------------------------------------------------

pub fn helper_main(argv: &[String]) -> i32 {
    let mut i = 0;
    let mut umsk: Option<u32> = None;
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
                    "-u" => {
                        let uid = parse_num(val) as libc::uid_t;
                        eprintln!("changing uid to {uid}");
                        if unsafe { libc::setuid(uid) } < 0 {
                            eprintln!("cannot change uid: {}", std::io::Error::last_os_error());
                            return 1;
                        }
                    }
                    "-g" => {
                        eprintln!("changing groups to {val}");
                        if let Err(e) = set_gids(val) {
                            eprintln!("cannot change groups: {e}");
                            return 1;
                        }
                    }
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
        i += 1; // consume ':' or run past end
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

// strtol(..., 0) semantics: 0x hex, leading-0 octal, else decimal
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

// protocol special path values: "NULL" -> null pointer, "DEADCODE" -> 0xdeadc0de
fn cptr(p: &str) -> *const libc::c_char {
    match p {
        "NULL" => std::ptr::null(),
        "DEADCODE" => 0xdeadc0de as *const libc::c_char,
        // leak intentionally: the helper is one-shot, owned storage would be
        // dangling right after this expression
        _ => cstr(p).into_raw(),
    }
}

fn exec_syscall(name: &str, a: &[&str], fds: &mut Vec<RawFd>) -> Result<String, i32> {
    let arg = |i: usize| -> &str { a.get(i).copied().unwrap_or("") };
    let num = |i: usize| -> i64 { parse_num(arg(i)) };
    match name {
        "open" | "openat" => {
            let (dirfd, path, flags_str, mode_idx) = if name == "openat" {
                (fd_get(fds, num(0))?, arg(1), arg(2), 3)
            } else {
                (libc::AT_FDCWD, arg(0), arg(1), 2)
            };
            let flags = parse_open_flags(flags_str)?;
            #[cfg(target_os = "linux")]
            let flags = flags | libc::O_CLOEXEC; // keep descriptors out of leaked state
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
            let flag = if arg(2) == "AT_REMOVEDIR" {
                libc::AT_REMOVEDIR
            } else {
                parse_num(arg(2)) as i32
            };
            cvt(unsafe { libc::unlinkat(fd_get(fds, num(0))?, cptr(arg(1)), flag) })
        }
        "mkdir" => cvt(unsafe { libc::mkdir(cptr(arg(0)), num(1) as libc::mode_t) }),
        "mkdirat" => cvt(unsafe { libc::mkdirat(fd_get(fds, num(0))?, cptr(arg(1)), num(2) as libc::mode_t) }),
        "rmdir" => cvt(unsafe { libc::rmdir(cptr(arg(0))) }),
        "link" => cvt(unsafe { libc::link(cptr(arg(0)), cptr(arg(1))) }),
        "linkat" => {
            let flag = if arg(4) == "AT_SYMLINK_FOLLOW" {
                libc::AT_SYMLINK_FOLLOW
            } else {
                parse_num(arg(4)) as i32
            };
            cvt(unsafe {
                libc::linkat(
                    fd_get(fds, num(0))?,
                    cptr(arg(1)),
                    fd_get(fds, num(2))?,
                    cptr(arg(3)),
                    flag,
                )
            })
        }
        "symlink" => cvt(unsafe { libc::symlink(cptr(arg(0)), cptr(arg(1))) }),
        "symlinkat" => cvt(unsafe { libc::symlinkat(cptr(arg(0)), fd_get(fds, num(1))?, cptr(arg(2))) }),
        "rename" => cvt(unsafe { libc::rename(cptr(arg(0)), cptr(arg(1))) }),
        "renameat" => {
            cvt(unsafe { libc::renameat(fd_get(fds, num(0))?, cptr(arg(1)), fd_get(fds, num(2))?, cptr(arg(3))) })
        }
        "mkfifo" => cvt(unsafe { libc::mkfifo(cptr(arg(0)), num(1) as libc::mode_t) }),
        "mkfifoat" => cvt(unsafe { libc::mkfifoat(fd_get(fds, num(0))?, cptr(arg(1)), num(2) as libc::mode_t) }),
        "mknod" | "mknodat" => {
            let mode = num(2) as libc::mode_t
                | match arg(1) {
                    "b" => libc::S_IFBLK,
                    "c" | "u" => libc::S_IFCHR,
                    "f" => libc::S_IFIFO,
                    _ => libc::S_IFREG,
                };
            let dev = makedev(num(3), num(4));
            let r = if name == "mknodat" {
                unsafe { libc::mknodat(fd_get(fds, num(0))?, cptr(arg(1)), mode, dev) }
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
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), sa.sun_path.as_mut_ptr() as *mut u8, bytes.len());
                let len = 2 + bytes.len();
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
            }
            Ok("0".into())
        }
        "chmod" => cvt(unsafe { libc::chmod(cptr(arg(0)), num(1) as libc::mode_t) }),
        "fchmod" => cvt(unsafe { libc::fchmod(fd_get(fds, num(0))?, num(1) as libc::mode_t) }),
        "fchmodat" => {
            let flag = if arg(3) == "AT_SYMLINK_NOFOLLOW" {
                libc::AT_SYMLINK_NOFOLLOW
            } else {
                parse_num(arg(3)) as i32
            };
            cvt(unsafe { libc::fchmodat(fd_get(fds, num(0))?, cptr(arg(1)), num(2) as libc::mode_t, flag) })
        }
        "chown" => cvt(unsafe { libc::chown(cptr(arg(0)), num(1) as libc::uid_t, num(2) as libc::gid_t) }),
        "lchown" => cvt(unsafe { libc::lchown(cptr(arg(0)), num(1) as libc::uid_t, num(2) as libc::gid_t) }),
        "fchown" => cvt(unsafe { libc::fchown(fd_get(fds, num(0))?, num(1) as libc::uid_t, num(2) as libc::gid_t) }),
        "fchownat" => {
            let flag: i32 = if arg(4) == "AT_SYMLINK_NOFOLLOW" {
                libc::AT_SYMLINK_NOFOLLOW
            } else if arg(4) == "AT_EMPTY_PATH" && cfg!(target_os = "linux") {
                0x1000
            } else {
                parse_num(arg(4)) as i32
            };
            cvt(unsafe {
                libc::fchownat(
                    fd_get(fds, num(0))?,
                    cptr(arg(1)),
                    num(2) as libc::uid_t,
                    num(3) as libc::gid_t,
                    flag,
                )
            })
        }
        "truncate" => cvt(unsafe { libc::truncate(cptr(arg(0)), num(1) as libc::off_t) }),
        "ftruncate" => cvt(unsafe { libc::ftruncate(fd_get(fds, num(0))?, num(1) as libc::off_t) }),
        "posix_fallocate" => {
            crate::smallfile::preallocate(fd_get(fds, num(0))?, num(2))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            Ok("0".into())
        }
        "stat" | "lstat" | "fstat" | "fstatat" => {
            let mut sb: libc::stat = unsafe { std::mem::zeroed() };
            let r = match name {
                "stat" => unsafe { libc::stat(cptr(arg(0)), &mut sb) },
                "lstat" => unsafe { libc::lstat(cptr(arg(0)), &mut sb) },
                "fstat" => unsafe { libc::fstat(fd_get(fds, num(0))?, &mut sb) },
                _ => {
                    let flag = if arg(3) == "AT_SYMLINK_NOFOLLOW" {
                        libc::AT_SYMLINK_NOFOLLOW
                    } else {
                        parse_num(arg(3)) as i32
                    };
                    unsafe { libc::fstatat(fd_get(fds, num(0))?, cptr(arg(1)), &mut sb, flag) }
                }
            };
            if r < 0 {
                return Err(last_errno());
            }
            Ok(show_stats(
                &sb,
                match name {
                    "stat" | "lstat" => arg(1),
                    _ => arg(1),
                },
            ))
        }
        "pathconf" | "lpathconf" | "fpathconf" => {
            let pc = pathconf_id(arg(1));
            let r = match name {
                "fpathconf" => unsafe { libc::fpathconf(fd_get(fds, num(0))?, pc) },
                _ => unsafe { libc::pathconf(cptr(arg(0)), pc) },
            };
            if r < 0 {
                return Err(last_errno());
            }
            Ok(format!("{r}"))
        }
        "utimensat" => {
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = a;
                return Err(libc::ENOSYS);
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let times = [timespec_arg(num(2), arg(3)), timespec_arg(num(4), arg(5))];
                let flag = if arg(6) == "AT_SYMLINK_NOFOLLOW" {
                    libc::AT_SYMLINK_NOFOLLOW
                } else {
                    parse_num(arg(6)) as i32
                };
                let r = unsafe { libc::utimensat(fd_get(fds, num(0))?, cptr(arg(1)), times.as_ptr(), flag) };
                cvt(r)
            }
        }
        "write" => {
            let data = arg(1).as_bytes();
            let r = unsafe { libc::write(fd_get(fds, num(0))?, data.as_ptr() as *const libc::c_void, data.len()) };
            cvt(r as i32)
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
            cvt(r as i32)
        }
        "pread" => {
            let fd = fd_get(fds, num(0))?;
            let count = (num(1) as usize).min(1024);
            let off = num(2) as libc::off_t;
            let mut buf = vec![0u8; count];
            let mut got = 0usize;
            loop {
                let r = unsafe {
                    libc::pread(
                        fd,
                        buf[got..].as_mut_ptr() as *mut libc::c_void,
                        count - got,
                        off + got as libc::off_t,
                    )
                };
                if r <= 0 {
                    break;
                }
                got += r as usize;
            }
            return Ok(String::from_utf8_lossy(&buf[..got]).to_string());
        }
        "mksyscalls" => {
            // upstream prints the syscall list for CI docs; the runner never calls it
            println!("# syscall list not required by the fstest runner");
            Ok(String::new())
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

fn fd_get(fds: &[RawFd], pos: i64) -> Result<RawFd, i32> {
    // "0" also means stdin when no descriptor was opened yet (upstream keeps
    // stdin at slot 0 implicitly through the same table)
    fds.get(pos as usize).copied().ok_or(libc::EBADF)
}

fn makedev(major: i64, minor: i64) -> libc::dev_t {
    #[cfg(target_os = "linux")]
    {
        libc::makedev(major as libc::c_uint, minor as libc::c_uint)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // BSD/dev_t encoding used by Darwin
        (((major as u64 & 0xff_ffff) << 24) | (minor as u64 & 0xff_ffff)) as libc::dev_t
    }
}

fn timespec_arg(sec: i64, nsec_str: &str) -> libc::timespec {
    let nsec: i64 = if nsec_str == "UTIME_NOW" {
        utime_now()
    } else if nsec_str == "UTIME_OMIT" {
        utime_omit()
    } else {
        parse_num(nsec_str)
    };
    libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as libc::c_long,
    }
}

#[cfg(target_os = "linux")]
fn utime_now() -> i64 {
    libc::UTIME_NOW
}
#[cfg(target_os = "linux")]
fn utime_omit() -> i64 {
    libc::UTIME_OMIT
}
#[cfg(not(target_os = "linux"))]
fn utime_now() -> i64 {
    -2
}
#[cfg(not(target_os = "linux"))]
fn utime_omit() -> i64 {
    -1
}

fn parse_open_flags(s: &str) -> Result<i32, i32> {
    let mut flags = 0i32;
    for f in s.split(',') {
        if f.is_empty() {
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
            "O_SHLOCK" | "O_EXLOCK" | "O_EVTONLY" | "O_SYMLINK" => 0, // BSD-only, unused
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
            "atime_ns" => format!("{}", st_atime_nsec(sb)),
            "mtime" => format!("{}", sb.st_mtime),
            "mtime_ns" => format!("{}", st_mtime_nsec(sb)),
            "ctime" => format!("{}", sb.st_ctime),
            "ctime_ns" => format!("{}", st_ctime_nsec(sb)),
            "major" => {
                #[cfg(target_os = "linux")]
                {
                    format!("{}", libc::major(sb.st_rdev))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    format!("{}", (sb.st_rdev >> 24) & 0xff_ffff)
                }
            }
            "minor" => {
                #[cfg(target_os = "linux")]
                {
                    format!("{}", libc::minor(sb.st_rdev))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    format!("{}", sb.st_rdev & 0xff_ffff)
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

#[cfg(target_os = "macos")]
fn st_atime_nsec(sb: &libc::stat) -> i64 {
    sb.st_atime_nsec
}
#[cfg(target_os = "macos")]
fn st_mtime_nsec(sb: &libc::stat) -> i64 {
    sb.st_mtime_nsec
}
#[cfg(target_os = "macos")]
fn st_ctime_nsec(sb: &libc::stat) -> i64 {
    sb.st_ctime_nsec
}
#[cfg(not(target_os = "macos"))]
fn st_atime_nsec(sb: &libc::stat) -> i64 {
    sb.st_atime_nsec
}
#[cfg(not(target_os = "macos"))]
fn st_mtime_nsec(sb: &libc::stat) -> i64 {
    sb.st_mtime_nsec
}
#[cfg(not(target_os = "macos"))]
fn st_ctime_nsec(sb: &libc::stat) -> i64 {
    sb.st_ctime_nsec
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

// ---------------------------------------------------------------------------
// runner: execute each embedded .t through `sh -s` with an inline prelude
// ---------------------------------------------------------------------------

struct FileResult {
    name: String,
    tests: u64,
    passed: u64,
    failed: u64,
    todo: u64,
    failures: Vec<String>,
    bail: bool,
    stderr_tail: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own executable: {e}"))?;
    let misc = embedded("misc.sh").ok_or("misc.sh missing from embedded suite")?;
    let conf = embedded("conf").ok_or("conf missing from embedded suite")?;
    // misc.sh's prologue locates conf/pjdfstest on disk; we inline everything,
    // so keep the file body starting at requires_root()
    let misc_body = misc
        .splitn(2, "\nrequires_root()")
        .nth(1)
        .ok_or("unexpected misc.sh layout")?;
    let selected: Vec<&(&str, &str)> = TEST_FILES
        .iter()
        .filter(|(name, _)| name.ends_with(".t"))
        .filter(|(name, _)| args.filter.is_empty() || args.filter.iter().any(|f| name.contains(f)))
        .collect();
    info!(
        "pjdfstest: {} test files selected, top={}{}{}",
        selected.len(),
        mountpoint.display(),
        args.fs.as_ref().map(|f| format!(", fs={f}")).unwrap_or_default(),
        args.os.as_ref().map(|o| format!(", os={o}")).unwrap_or_default(),
    );
    if !nix_euid_is_root() {
        warn!("not running as root: chown/mknod/permission cases will fail (upstream behaves the same)");
    }
    let scratch_root = mountpoint.join(".fstest-pjdfstest");
    fs::create_dir_all(&scratch_root).map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    for (idx, (name, body)) in selected.iter().enumerate() {
        let scratch = scratch_root.join(format!("{idx:03}-{}", name.replace('/', "_")));
        results.push(run_one(&scratch, name, body, conf, misc_body, &exe, args));
        if !args.keep {
            let _ = fs::remove_dir_all(&scratch);
        }
    }
    if !args.keep {
        let _ = fs::remove_dir_all(&scratch_root);
    }
    let total_tests: u64 = results.iter().map(|r| r.tests).sum();
    let total_failed: u64 = results.iter().map(|r| r.failed).sum();
    let total_todo: u64 = results.iter().map(|r| r.todo).sum();
    let bailed = results.iter().any(|r| r.bail);
    let mut failures = Vec::new();
    for r in &results {
        for f in &r.failures {
            failures.push(format!("{}: {f}", r.name));
        }
    }
    info!(
        "pjdfstest: files={} tests={} failed={} todo-passed={}",
        results.len(),
        total_tests,
        total_failed,
        total_todo
    );
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "pjdfstest",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "params": {
            "files_selected": selected.len(),
            "filter": args.filter,
            "fs_override": args.fs,
            "os_override": args.os,
            "timeout_sec": args.timeout,
            "euid": unsafe { libc::geteuid() },
        },
        "status": if total_failed == 0 && !bailed { "ok" } else { "failed" },
        "summary": {
            "files": results.len(),
            "tests": total_tests,
            "passed": total_tests - total_failed,
            "failed": total_failed,
            "todoPassed": total_todo,
        },
        "failures": failures,
        "files": results.iter().map(|r| json!({
            "name": r.name,
            "tests": r.tests, "passed": r.passed, "failed": r.failed, "todo": r.todo,
            "bail": r.bail, "timedOut": r.timed_out, "exitCode": r.exit_code,
            "failures": r.failures,
            "stderrTail": r.stderr_tail,
        })).collect::<Vec<_>>(),
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}

fn run_one(scratch: &Path, name: &str, body: &str, conf: &str, misc_body: &str, exe: &Path, args: &Args) -> FileResult {
    let _ = fs::remove_dir_all(scratch);
    let mut r = FileResult {
        name: name.to_string(),
        tests: 0,
        passed: 0,
        failed: 0,
        todo: 0,
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
    // strip the on-disk sourcing header; misc.sh is already inlined
    let body = body
        .lines()
        .filter(|l| {
            let t = l.trim();
            t != "dir=`dirname $0`" && t != ". ${dir}/../misc.sh"
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut script = String::new();
    script.push_str("fstest='");
    script.push_str(&exe.display().to_string().replace('\'', "'\\''"));
    script.push_str(" __pjdfstest'\n");
    script.push_str(conf);
    if let Some(fs_name) = &args.fs {
        script.push_str(&format!("\nfs='{fs_name}'\n"));
    }
    if let Some(os_name) = &args.os {
        script.push_str(&format!("\nos='{os_name}'\n"));
    }
    script.push_str("\ntest -n \"${fstest}\" || { echo 'not ok - fstest helper not set'; exit 1; }\n");
    script.push_str("\nrequires_root()");
    script.push_str(misc_body);
    script.push('\n');
    script.push_str(&body);
    script.push('\n');
    // the script travels as a single argv element (max ~14KB, well under the
    // per-argument limit) — piping it via stdin lets any command that inherits
    // stdin eat the remaining script stream, which stalled the big matrix tests
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&script)
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            r.failures.push(format!("cannot spawn sh: {e}"));
            r.tests = 1;
            r.failed = 1;
            return r;
        }
    };
    // drain stdout/stderr concurrently: failing assertions print long lines and
    // a full 64KB pipe would deadlock the child against our unread buffers
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_reader = thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let stderr_reader = thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let deadline = Duration::from_secs(args.timeout.max(1));
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                r.exit_code = status.code();
                break;
            }
            Ok(None) => {
                if start.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    r.timed_out = true;
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    let _ = child.wait();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    r.stderr_tail = truncate(&stderr, 2000);
    let mut plan: Option<u64> = None;
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.starts_with("1..") {
            if let Ok(n) = line[3..].trim().parse() {
                plan = Some(n);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Bail out!") {
            r.bail = true;
            r.failures.push(line.to_string());
            let _ = rest;
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
            // TODO failures count as passed in TAP
            r.passed += 1;
        } else {
            r.failed += 1;
            r.failures.push(line.to_string());
        }
    }
    if let Some(n) = plan {
        if n > r.tests && r.failed == 0 && !r.timed_out {
            // script died before finishing the plan
            r.failed += n - r.tests;
            r.failures
                .push(format!("script stopped early: declared {n} tests, ran {}", r.tests));
        }
    }
    if r.timed_out {
        r.failures.push(format!("timed out after {}s", args.timeout));
    }
    r
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let start = s.len() - max;
        let mut off = 0;
        while !s.is_char_boundary(off) {
            off += 1;
        }
        format!("...{}", &s[start + off..])
    }
}
