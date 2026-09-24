// Behavior-level port of fsstress from https://github.com/kdave/xfstests
// ltp/fsstress.c (SGI lineage, GPL-2.0): randomized directory-tree metadata+data
// stress. Upstream forks one child per process; here threads are used instead,
// each with its own file-entry lists and name sequence starting at 0 in the
// shared base directory, so cross-thread name collisions are tolerated by
// design, matching upstream where every child starts nameseq at 0. Ops that
// require Linux ioctls or XFS/btrfs specifics are disabled (freq 0).

use crate::fsx::GlibcRandom;
use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Instant, SystemTime};

const FILELEN_MAX: usize = 32 * 4096;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ft {
    Dir,
    Reg,
    Sym,
    Chr,
    Blk,
    Sock,
}

const FT_TYPES: [(Ft, char); 6] = [
    (Ft::Dir, 'd'),
    (Ft::Reg, 'f'),
    (Ft::Sym, 'l'),
    (Ft::Chr, 'c'),
    (Ft::Blk, 'r'),
    (Ft::Sock, 's'),
];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Op {
    Creat,
    Write,
    WriteV,
    Read,
    ReadV,
    MRead,
    MWrite,
    Mkdir,
    Mknod,
    Symlink,
    Rename,
    Truncate,
    SetFattr,
    Link,
    Unlink,
    RmDir,
    ReadLink,
    Stat,
    GetDents,
    GetFattr,
    RemoveFattr,
    GetAttr,
    Fsync,
    Fdatasync,
    Sync,
    Fallocate,
    Punch,
    Zero,
    Collapse,
    Insert,
    CloneRange,
    CopyRange,
}

// (op, name, upstream default frequency, is_write)
const OP_TABLE: [(Op, &str, u32, bool); 32] = [
    (Op::Creat, "creat", 4, true),
    (Op::Write, "write", 4, true),
    (Op::WriteV, "writev", 4, true),
    (Op::Read, "read", 1, false),
    (Op::ReadV, "readv", 1, false),
    (Op::MRead, "mread", 2, false),
    (Op::MWrite, "mwrite", 2, true),
    (Op::Mkdir, "mkdir", 2, true),
    (Op::Mknod, "mknod", 2, true),
    (Op::Symlink, "symlink", 2, true),
    (Op::Rename, "rename", 2, true),
    (Op::Truncate, "truncate", 2, true),
    (Op::SetFattr, "setfattr", 2, true),
    (Op::Link, "link", 1, true),
    (Op::Unlink, "unlink", 1, true),
    (Op::RmDir, "rmdir", 1, true),
    (Op::ReadLink, "readlink", 1, false),
    (Op::Stat, "stat", 1, false),
    (Op::GetDents, "getdents", 1, false),
    (Op::GetFattr, "getfattr", 1, false),
    (Op::RemoveFattr, "removefattr", 1, true),
    (Op::GetAttr, "getattr", 1, false),
    (Op::Fsync, "fsync", 1, true),
    (Op::Fdatasync, "fdatasync", 1, true),
    (Op::Sync, "sync", 1, true),
    (Op::Fallocate, "fallocate", 1, true),
    (Op::Punch, "punch", 1, true),
    (Op::Zero, "zero", 1, true),
    (Op::Collapse, "collapse", 1, true),
    (Op::Insert, "insert", 1, true),
    (Op::CloneRange, "clonerange", 4, true),
    (Op::CopyRange, "copyrange", 4, true),
];

fn op_by_name(n: &str) -> Option<Op> {
    OP_TABLE
        .iter()
        .find(|(_, name, _, _)| *name == n)
        .map(|(o, _, _, _)| *o)
}

fn op_name(o: Op) -> &'static str {
    OP_TABLE.iter().find(|(x, ..)| *x == o).map(|(_, n, _, _)| *n).unwrap()
}

#[derive(Clone, Copy)]
struct Fent {
    id: u64,
    parent: i64,
    xattr_counter: u64,
}

#[derive(clap::Args)]
pub struct Args {
    /// number of operations per process
    #[arg(short = 'n', long, default_value_t = 1000)]
    pub nops: u64,

    /// number of processes (threads)
    #[arg(short = 'p', long, default_value_t = 1)]
    pub nproc: u32,

    /// number of times to loop the whole run (0 = infinite until --duration)
    #[arg(short = 'l', long, default_value_t = 1)]
    pub loops: u32,

    /// RNG seed (0 = time based)
    #[arg(short = 's', long, default_value_t = 0)]
    pub seed: i32,

    /// adjust op frequency: op_name=freq (repeatable)
    #[arg(short = 'f', long = "freq")]
    pub freqs: Vec<String>,

    /// zero frequencies of all write ops (read-only workload)
    #[arg(short = 'R', long)]
    pub read_only: bool,

    /// zero frequencies of all non-write ops
    #[arg(short = 'w', long)]
    pub write_only: bool,

    /// clean up created files after the run
    #[arg(short = 'c', long)]
    pub cleanup: bool,

    /// verbose per-op logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// run for this many seconds instead of -n
    #[arg(long, default_value_t = 0)]
    pub duration: u64,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

struct ThreadState {
    tid: u32,
    rng: GlibcRandom,
    nameseq: u64,
    lists: HashMap<Ft, Vec<Fent>>,
    errors: u64,
    hist: HashMap<&'static str, u64>,
}

impl ThreadState {
    fn new(tid: u32, seed: i32) -> Self {
        ThreadState {
            tid,
            rng: GlibcRandom::new(seed.wrapping_add(tid as i32)),
            nameseq: 0,
            lists: FT_TYPES.iter().map(|(t, _)| (*t, Vec::new())).collect(),
            errors: 0,
            hist: HashMap::new(),
        }
    }

    fn fent_to_name(&self, base: &Path, ft: Ft, fent: &Fent) -> PathBuf {
        let mut path = base.to_path_buf();
        if fent.parent >= 0 {
            if let Some(p) = self.dir_by_id(fent.parent as u64) {
                path = self.fent_to_name(base, Ft::Dir, &p);
            }
        }
        let tag = FT_TYPES.iter().find(|(t, _)| *t == ft).unwrap().1;
        path.push(format!("{tag}{:x}", fent.id));
        path
    }

    fn dir_by_id(&self, id: u64) -> Option<Fent> {
        self.lists.get(&Ft::Dir)?.iter().find(|f| f.id == id).copied()
    }

    fn get_fent(&self, types: &[Ft], r: u64) -> Option<(Ft, Fent)> {
        let totalsum: usize = types
            .iter()
            .map(|t| self.lists.get(t).map(|l| l.len()).unwrap_or(0))
            .sum();
        if totalsum == 0 {
            return None;
        }
        let mut x = (r as usize) % totalsum;
        for t in types {
            let list = self.lists.get(t).unwrap();
            if x < list.len() {
                return Some((*t, list[x]));
            }
            x -= list.len();
        }
        None
    }

    fn generate_name(&mut self, parent: Option<(Ft, Fent)>, ft: Ft, base: &Path) -> (PathBuf, u64, i64) {
        let id = self.nameseq;
        self.nameseq += 1;
        let tag = FT_TYPES.iter().find(|(t, _)| *t == ft).unwrap().1;
        let name = format!("{tag}{id:x}");
        let parid = match &parent {
            Some((_, fep)) => fep.id as i64,
            None => -1,
        };
        let mut path = base.to_path_buf();
        if let Some((Ft::Dir, fep)) = &parent {
            path = self.fent_to_name(base, Ft::Dir, fep);
        }
        path.push(name);
        (path, id, parid)
    }

    fn add_fent(&mut self, ft: Ft, id: u64, parent: i64) {
        self.lists.entry(ft).or_default().push(Fent {
            id,
            parent,
            xattr_counter: 0,
        });
    }
}

fn report_err(st: &mut ThreadState, op: &str, path: &Path, e: &std::io::Error, verbose: bool) {
    st.errors += 1;
    SIGBUS_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if verbose {
        warn!("{}: {op} {}: {}", st.tid, path.display(), e);
    }
}

fn pick_path(st: &mut ThreadState, types: &[Ft], r: u64, base: &Path) -> Option<PathBuf> {
    let (ft, fent) = st.get_fent(types, r)?;
    Some(st.fent_to_name(base, ft, &fent))
}

fn rand_file_offset(st: &mut ThreadState, size: u64) -> u64 {
    let lr = ((st.rng.next() as u64) << 32) | st.rng.next() as u64;
    let lim = (size + 1024 * 1024).min(u32::MAX as u64);
    if lim == 0 { 0 } else { lr % lim }
}

fn write_pattern(st: &mut ThreadState, fd: i32, path: &Path, off: u64, len: usize, verbose: bool) {
    let fill = (st.nameseq & 0xff) as u8;
    let buf = vec![fill; len];
    let r = unsafe { libc::lseek(fd, off as i64, libc::SEEK_SET) };
    if r < 0 {
        report_err(st, "lseek", path, &std::io::Error::last_os_error(), verbose);
        return;
    }
    let mut done = 0usize;
    while done < len {
        let n = unsafe { libc::write(fd, buf[done..].as_ptr() as *const libc::c_void, len - done) };
        if n <= 0 {
            report_err(st, "write", path, &std::io::Error::last_os_error(), verbose);
            return;
        }
        done += n as usize;
    }
}

fn open_fd(st: &mut ThreadState, path: &Path, write: bool, verbose: bool) -> Option<i32> {
    let flags = if write { libc::O_WRONLY } else { libc::O_RDONLY };
    let c = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        report_err(st, "open", path, &std::io::Error::last_os_error(), verbose);
        None
    } else {
        Some(fd)
    }
}

fn do_fallocate(st: &mut ThreadState, fd: i32, mode: i32, off: u64, len: u64, path: &Path, verbose: bool) {
    #[cfg(target_os = "linux")]
    {
        let r = unsafe { libc::fallocate(fd, mode, off as i64, len as i64) };
        if r != 0 {
            report_err(st, "fallocate", path, &std::io::Error::last_os_error(), verbose);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (st, fd, mode, off, len, path, verbose);
    }
}

extern "C" fn sigbus_handler(_: libc::c_int) {
    // async-signal-safe: no allocation, no locks — fixed buffers, raw write(2)
    const MSG: &[u8] = b"fsstress: SIGBUS (concurrent truncate vs mmap race); emitting failed report and exiting\n";
    const CAUSE: &[u8] = b"SIGBUS: concurrent truncate vs mmap race aborted the run (upstream fsstress recovers per forked child; the threaded port cannot)";
    let mut buf = [0u8; 512];
    let mut w = 0usize;
    w = push_bytes(&mut buf, w, b"{\"suite\":\"fsstress\",\"status\":\"failed\",\"failure\":\"");
    w = push_bytes(&mut buf, w, CAUSE);
    w = push_bytes(&mut buf, w, b"\",\"totalOps\":");
    w = push_u64(&mut buf, w, SIGBUS_OPS.load(std::sync::atomic::Ordering::Relaxed));
    w = push_bytes(&mut buf, w, b",\"totalErrors\":");
    w = push_u64(&mut buf, w, SIGBUS_ERRORS.load(std::sync::atomic::Ordering::Relaxed));
    w = push_bytes(&mut buf, w, b"}\n");
    unsafe {
        libc::write(2, MSG.as_ptr() as *const libc::c_void, MSG.len());
        libc::write(1, buf.as_ptr() as *const libc::c_void, w);
        let fd = SIGBUS_JSON_FD.load(std::sync::atomic::Ordering::Relaxed);
        if fd >= 0 {
            libc::lseek(fd, 0, 0);
            libc::write(fd, buf.as_ptr() as *const libc::c_void, w);
        }
        libc::_exit(1);
    }
}

fn push_bytes(buf: &mut [u8], w: usize, b: &[u8]) -> usize {
    let n = (buf.len() - w).min(b.len());
    buf[w..w + n].copy_from_slice(&b[..n]);
    w + n
}

fn push_u64(buf: &mut [u8], w: usize, mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let n = (buf.len() - w).min(tmp.len() - i);
    buf[w..w + n].copy_from_slice(&tmp[i..i + n]);
    w + n
}

// progress counters the SIGBUS handler reads after a fault
static SIGBUS_OPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SIGBUS_ERRORS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SIGBUS_JSON_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn install_sigbus_guard() {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction = sigbus_handler as *const () as usize;
    act.sa_flags = libc::SA_SIGINFO;
    unsafe {
        libc::sigaction(libc::SIGBUS, &act, std::ptr::null_mut());
    }
}

struct Proc {
    st: ThreadState,
    base: PathBuf,
    verbose: bool,
    linux: bool,
    created: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
}

impl Proc {
    fn note_created(&mut self, path: &Path) {
        self.created.lock().unwrap().insert(path.to_path_buf());
    }

    fn run_op(&mut self, op: Op, r: u64) {
        self.st.hist.entry(op_name(op)).and_modify(|c| *c += 1).or_insert(1);
        match op {
            Op::Creat => self.creat(r),
            Op::Write => self.rw(r, true, false),
            Op::WriteV => self.rw(r, true, true),
            Op::Read => self.rw(r, false, false),
            Op::ReadV => self.rw(r, false, true),
            Op::MRead => self.mrw(r, false),
            Op::MWrite => self.mrw(r, true),
            Op::Mkdir => self.mkdir(r),
            Op::Mknod => self.mknod(r),
            Op::Symlink => self.symlink(r),
            Op::Rename => self.rename(r),
            Op::Truncate => self.truncate(r),
            Op::SetFattr => self.setfattr(r),
            Op::Link => self.link(r),
            Op::Unlink => self.unlink(r),
            Op::RmDir => self.rmdir(r),
            Op::ReadLink => self.readlink(r),
            Op::Stat => self.stat(r),
            Op::GetDents => self.getdents(r),
            Op::GetFattr => self.getfattr(r),
            Op::RemoveFattr => self.removefattr(r),
            Op::GetAttr => self.getattr(r),
            Op::Fsync => self.fdatasync_like(true),
            Op::Fdatasync => self.fdatasync_like(false),
            Op::Sync => unsafe { libc::sync() },
            Op::Fallocate => self.falloc_range(r, 0, false),
            Op::Punch => self.falloc_range(r, 0x02 | 0x01, true),
            Op::Zero => self.falloc_range(r, 0x10, false),
            Op::Collapse => self.range_shift(r, true),
            Op::Insert => self.range_shift(r, false),
            Op::CloneRange => self.clonerange(r),
            Op::CopyRange => self.copyrange(r),
        }
    }

    fn creat(&mut self, r: u64) {
        let parent = self.st.get_fent(&[Ft::Dir], r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Reg, &self.base);
        match fs::OpenOptions::new().write(true).create(true).mode(0o666).open(&path) {
            Ok(_) => {
                self.st.add_fent(Ft::Reg, id, parid);
                self.note_created(&path);
            }
            Err(e) => report_err(&mut self.st, "creat", &path, &e, self.verbose),
        }
    }

    fn mkdir(&mut self, r: u64) {
        let parent = self.st.get_fent(&[Ft::Dir], r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Dir, &self.base);
        match fs::create_dir(&path) {
            Ok(()) => {
                self.st.add_fent(Ft::Dir, id, parid);
                self.note_created(&path);
            }
            Err(e) => report_err(&mut self.st, "mkdir", &path, &e, self.verbose),
        }
    }

    fn mknod(&mut self, r: u64) {
        let parent = self.st.get_fent(&[Ft::Dir], r);
        let ft = if r % 2 == 0 { Ft::Chr } else { Ft::Blk };
        let (path, id, parid) = self.st.generate_name(parent, ft, &self.base);
        let c = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mode = libc::S_IFCHR | 0o644;
        #[cfg(target_os = "linux")]
        let dev = libc::makedev(1, (r % 8) as u32);
        #[cfg(not(target_os = "linux"))]
        let dev = libc::makedev(1, (r % 8) as i32);
        if unsafe { libc::mknod(c.as_ptr(), mode, dev) } == 0 {
            self.st.add_fent(ft, id, parid);
            self.note_created(&path);
        } else {
            report_err(
                &mut self.st,
                "mknod",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
        }
    }

    fn symlink(&mut self, r: u64) {
        let parent = self.st.get_fent(&[Ft::Dir], r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Sym, &self.base);
        let len = (r % 1024) as usize;
        let mut val = vec![b'x'; len];
        for i in (10..len.saturating_sub(1)).step_by(10) {
            val[i] = b'/';
        }
        let target = String::from_utf8_lossy(&val).into_owned();
        match std::os::unix::fs::symlink(&target, &path) {
            Ok(()) => {
                self.st.add_fent(Ft::Sym, id, parid);
                self.note_created(&path);
            }
            Err(e) => report_err(&mut self.st, "symlink", &path, &e, self.verbose),
        }
    }

    fn rw(&mut self, r: u64, write: bool, vectored: bool) {
        let types = if write { vec![Ft::Reg] } else { vec![Ft::Reg, Ft::Sym] };
        let Some(path) = pick_path(&mut self.st, &types, r, &self.base) else {
            return;
        };
        let Some(fd) = open_fd(&mut self.st, &path, write, self.verbose) else {
            return;
        };
        let mut sb: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut sb) } != 0 {
            unsafe { libc::close(fd) };
            return;
        }
        let size = sb.st_size as u64;
        if !write && size == 0 {
            unsafe { libc::close(fd) };
            return;
        }
        let off = rand_file_offset(&mut self.st, size);
        let len = (self.st.rng.next() as usize % FILELEN_MAX) + 1;
        if write {
            unsafe { libc::lseek(fd, off as i64, libc::SEEK_SET) };
            let buf = vec![(self.st.nameseq & 0xff) as u8; len];
            if vectored {
                let half = len / 2;
                let iov = [
                    libc::iovec {
                        iov_base: buf.as_ptr() as *mut libc::c_void,
                        iov_len: half,
                    },
                    libc::iovec {
                        iov_base: buf[half..].as_ptr() as *mut libc::c_void,
                        iov_len: len - half,
                    },
                ];
                let n = unsafe { libc::writev(fd, iov.as_ptr(), 2) };
                if n < 0 {
                    report_err(
                        &mut self.st,
                        "writev",
                        &path,
                        &std::io::Error::last_os_error(),
                        self.verbose,
                    );
                }
            } else {
                write_pattern(&mut self.st, fd, &path, off, len, self.verbose);
            }
        } else {
            let mut buf = vec![0u8; len];
            if vectored {
                let half = len / 2;
                let mut b1 = vec![0u8; half];
                let mut b2 = vec![0u8; len - half];
                let mut iov = [
                    libc::iovec {
                        iov_base: b1.as_mut_ptr() as *mut libc::c_void,
                        iov_len: half,
                    },
                    libc::iovec {
                        iov_base: b2.as_mut_ptr() as *mut libc::c_void,
                        iov_len: len - half,
                    },
                ];
                let _ = unsafe { libc::readv(fd, iov.as_mut_ptr(), 2) };
            } else {
                let _ = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, len, off as i64) };
            }
        }
        unsafe { libc::close(fd) };
    }

    fn mrw(&mut self, r: u64, write: bool) {
        let types = if write { vec![Ft::Reg] } else { vec![Ft::Reg, Ft::Sym] };
        let Some(path) = pick_path(&mut self.st, &types, r, &self.base) else {
            return;
        };
        let flags = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let c = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd < 0 {
            report_err(
                &mut self.st,
                "open",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
            return;
        }
        let mut sb: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut sb) } != 0 || sb.st_size == 0 {
            unsafe { libc::close(fd) };
            return;
        }
        let size = sb.st_size as u64;
        // upstream: offset page-rounded into [0,size), length clipped to EOF,
        // MAP_SHARED/MAP_PRIVATE chosen at random
        let mut off = self.st.rng.next() as u64 % size;
        let page = 4096usize;
        off -= off % page as u64;
        let mut len = ((self.st.rng.next() as usize) % ((size - off) as usize).min(FILELEN_MAX)) + 1;
        if (off as usize) + len > size as usize {
            len = size as usize - off as usize;
        }
        let pg = (off as usize) & (page - 1);
        let map_len = pg + len;
        let map_flags = if self.st.rng.next() % 2 == 0 {
            libc::MAP_SHARED
        } else {
            libc::MAP_PRIVATE
        };
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                if write {
                    libc::PROT_READ | libc::PROT_WRITE
                } else {
                    libc::PROT_READ
                },
                map_flags,
                fd,
                (off as usize - pg) as libc::off_t,
            )
        };
        if p == libc::MAP_FAILED {
            report_err(
                &mut self.st,
                "mmap",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
            unsafe { libc::close(fd) };
            return;
        }
        if write {
            unsafe {
                std::ptr::write_bytes(p as *mut u8, (self.st.nameseq & 0xff) as u8, len);
                libc::msync(p, map_len, libc::MS_SYNC);
            }
        }
        unsafe { libc::munmap(p, map_len) };
        unsafe { libc::close(fd) };
    }

    fn truncate(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg, Ft::Sym], r, &self.base) else {
            return;
        };
        let md = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                report_err(&mut self.st, "stat", &path, &e, self.verbose);
                return;
            }
        };
        let off = rand_file_offset(&mut self.st, md.len());
        if let Err(e) = fs::File::options().write(true).open(&path).and_then(|f| f.set_len(off)) {
            report_err(&mut self.st, "truncate", &path, &e, self.verbose);
        }
    }

    fn link(&mut self, r: u64) {
        let types = vec![Ft::Reg, Ft::Sym, Ft::Chr, Ft::Blk, Ft::Sock];
        let Some(src_path) = pick_path(&mut self.st, &types, r, &self.base) else {
            return;
        };
        let Some((src_ft, _)) = self.st.get_fent(&types, r) else {
            return;
        };
        let rr = self.st.rng.next() as u64;
        let dest_dir = self.st.get_fent(&[Ft::Dir], rr);
        let (path, id, parid) = self.st.generate_name(dest_dir, src_ft, &self.base);
        match std::fs::hard_link(&src_path, &path) {
            Ok(()) => {
                self.st.add_fent(src_ft, id, parid);
                self.note_created(&path);
            }
            Err(e) => report_err(&mut self.st, "link", &path, &e, self.verbose),
        }
    }

    fn unlink(&mut self, r: u64) {
        let types = vec![Ft::Reg, Ft::Sym, Ft::Chr, Ft::Blk, Ft::Sock];
        let Some((ft, fent)) = self.st.get_fent(&types, r) else {
            return;
        };
        let path = self.st.fent_to_name(&self.base, ft, &fent);
        match fs::remove_file(&path) {
            Ok(()) => {
                let list = self.st.lists.get_mut(&ft).unwrap();
                list.retain(|f| f.id != fent.id);
            }
            Err(e) => report_err(&mut self.st, "unlink", &path, &e, self.verbose),
        }
    }

    fn rmdir(&mut self, r: u64) {
        let Some((_, fent)) = self.st.get_fent(&[Ft::Dir], r) else {
            return;
        };
        let path = self.st.fent_to_name(&self.base, Ft::Dir, &fent);
        match fs::remove_dir(&path) {
            Ok(()) => {
                let list = self.st.lists.get_mut(&Ft::Dir).unwrap();
                list.retain(|f| f.id != fent.id);
            }
            Err(e) => report_err(&mut self.st, "rmdir", &path, &e, self.verbose),
        }
    }

    fn rename(&mut self, r: u64) {
        let types = vec![Ft::Reg, Ft::Sym, Ft::Chr, Ft::Blk, Ft::Sock];
        let Some((ft, fent)) = self.st.get_fent(&types, r) else {
            return;
        };
        let src = self.st.fent_to_name(&self.base, ft, &fent);
        let rr = self.st.rng.next() as u64;
        let dest_dir = self.st.get_fent(&[Ft::Dir], rr);
        let (dest, newid, parid) = self.st.generate_name(dest_dir, ft, &self.base);
        match fs::rename(&src, &dest) {
            Ok(()) => {
                {
                    let list = self.st.lists.get_mut(&ft).unwrap();
                    list.retain(|f| f.id != fent.id);
                }
                self.st.add_fent(ft, newid, parid);
                self.note_created(&dest);
            }
            Err(e) => report_err(&mut self.st, "rename", &src, &e, self.verbose),
        }
    }

    fn readlink(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, &[Ft::Sym], r, &self.base) else {
            return;
        };
        match fs::read_link(&path) {
            Ok(_) => {}
            Err(e) => report_err(&mut self.st, "readlink", &path, &e, self.verbose),
        }
    }

    fn stat(&mut self, r: u64) {
        let Some(path) = pick_path(
            &mut self.st,
            &[Ft::Dir, Ft::Reg, Ft::Sym, Ft::Chr, Ft::Blk, Ft::Sock],
            r,
            &self.base,
        ) else {
            return;
        };
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) => report_err(&mut self.st, "lstat", &path, &e, self.verbose),
        }
    }

    fn getdents(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, &[Ft::Dir], r, &self.base) else {
            return;
        };
        match fs::read_dir(&path) {
            Ok(entries) => {
                for e in entries.flatten() {
                    let _ = e.file_name();
                }
            }
            Err(e) => report_err(&mut self.st, "getdents", &path, &e, self.verbose),
        }
    }

    fn xattr_target(&mut self, r: u64) -> Option<(PathBuf, u64)> {
        let (ft, fent) = self.st.get_fent(&[Ft::Reg, Ft::Dir], r)?;
        let path = self.st.fent_to_name(&self.base, ft, &fent);
        Some((path, fent.id))
    }

    fn setfattr(&mut self, r: u64) {
        let Some((path, id)) = self.xattr_target(r) else {
            return;
        };
        let n = (r % 8) as u32;
        let name = format!("user.x{n}");
        let val_len = (r % 101) as usize;
        let val: Vec<u8> = (0..val_len).map(|i| (r as u8).wrapping_add(i as u8)).collect();
        match xattr::set(&path, &name, &val) {
            Ok(()) => {
                for ft in [Ft::Dir, Ft::Reg] {
                    if let Some(f) = self.st.lists.get_mut(&ft).unwrap().iter_mut().find(|f| f.id == id) {
                        f.xattr_counter = f.xattr_counter.max(n as u64 + 1);
                        break;
                    }
                }
            }
            Err(e) => report_err(&mut self.st, "setfattr", &path, &e, self.verbose),
        }
    }

    fn getfattr(&mut self, r: u64) {
        let Some((path, _)) = self.xattr_target(r) else {
            return;
        };
        let n = (r % 8) as u32;
        if let Err(e) = xattr::get(&path, format!("user.x{n}")) {
            report_err(&mut self.st, "getfattr", &path, &e, self.verbose);
        }
    }

    fn removefattr(&mut self, r: u64) {
        let Some((path, _)) = self.xattr_target(r) else {
            return;
        };
        let n = (r % 8) as u32;
        if let Err(e) = xattr::remove(&path, format!("user.x{n}")) {
            report_err(&mut self.st, "removefattr", &path, &e, self.verbose);
        }
    }

    fn getattr(&mut self, r: u64) {
        let Some(path) = pick_path(
            &mut self.st,
            &[Ft::Dir, Ft::Reg, Ft::Sym, Ft::Chr, Ft::Blk, Ft::Sock],
            r,
            &self.base,
        ) else {
            return;
        };
        let c = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            report_err(
                &mut self.st,
                "open",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
            return;
        }
        // FS_IOC_GETFLAGS (Linux) / F_GETFLAGS (Darwin) — errors tolerated
        #[cfg(target_os = "linux")]
        unsafe {
            let mut fl: u32 = 0;
            let _ = libc::ioctl(fd, 0x80086601u64 as libc::c_ulong, &mut fl);
        }
        #[cfg(target_os = "macos")]
        unsafe {
            let mut fl: i32 = 0;
            let _ = libc::fcntl(fd, 46, &mut fl); // F_GETFLAGS on Darwin
        }
        unsafe { libc::close(fd) };
    }

    fn fdatasync_like(&mut self, full: bool) {
        let rr = self.st.rng.next() as u64;
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg], rr, &self.base) else {
            return;
        };
        let Some(fd) = open_fd(&mut self.st, &path, true, self.verbose) else {
            return;
        };
        #[cfg(target_os = "linux")]
        {
            let r = if full {
                unsafe { libc::fsync(fd) }
            } else {
                unsafe { libc::fdatasync(fd) }
            };
            if r != 0 {
                report_err(
                    &mut self.st,
                    "fsync",
                    &path,
                    &std::io::Error::last_os_error(),
                    self.verbose,
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = full;
            let r = unsafe { libc::fsync(fd) };
            if r != 0 {
                report_err(
                    &mut self.st,
                    "fsync",
                    &path,
                    &std::io::Error::last_os_error(),
                    self.verbose,
                );
            }
        }
        unsafe { libc::close(fd) };
    }

    fn falloc_range(&mut self, r: u64, mode: i32, clip_eof: bool) {
        if !self.linux {
            return;
        }
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg], r, &self.base) else {
            return;
        };
        let Some(fd) = open_fd(&mut self.st, &path, true, self.verbose) else {
            return;
        };
        let mut sb: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut sb) } != 0 {
            unsafe { libc::close(fd) };
            return;
        }
        let off = rand_file_offset(&mut self.st, sb.st_size as u64);
        let len = ((self.st.rng.next() as usize) % FILELEN_MAX + 1) as u64;
        let len = if clip_eof {
            len.min((sb.st_size as u64).saturating_sub(off)).max(1)
        } else {
            len
        };
        do_fallocate(&mut self.st, fd, mode, off, len, &path, self.verbose);
        unsafe { libc::close(fd) };
    }

    fn range_shift(&mut self, r: u64, collapse: bool) {
        if !self.linux {
            return;
        }
        let mode = if collapse { 0x08 } else { 0x20 };
        self.falloc_range(r, mode, collapse);
    }

    fn clonerange(&mut self, _r: u64) {
        // FICLONERANGE ioctl: Linux/reflink filesystems only
    }

    fn copyrange(&mut self, _r: u64) {
        // copy_file_range(2): Linux only in upstream's fsstress usage
    }
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let threads = args.nproc.max(1);
    let seed = if args.seed == 0 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i32
            ^ std::process::id() as i32
    } else {
        args.seed
    };
    let linux = cfg!(target_os = "linux");
    let mut freqs: HashMap<&'static str, u32> = OP_TABLE
        .iter()
        .map(|(o, n, f, _)| {
            (
                *n,
                if (linux
                    || !matches!(
                        o,
                        Op::Fallocate
                            | Op::Punch
                            | Op::Zero
                            | Op::Collapse
                            | Op::Insert
                            | Op::CloneRange
                            | Op::CopyRange
                    ))
                    // mmap ops race with concurrent truncate: upstream survives
                    // the resulting SIGBUS per forked child, the threaded port
                    // would abort the whole run, so they are opt-in via
                    // `-f mread=N` / `-f mwrite=N`
                    && !matches!(o, Op::MRead | Op::MWrite)
                {
                    *f
                } else {
                    0
                },
            )
        })
        .collect();
    for spec in &args.freqs {
        let (name, freq) = spec
            .split_once('=')
            .ok_or_else(|| format!("bad -f spec {spec:?}, expected op_name=freq"))?;
        let op = op_by_name(name).ok_or_else(|| format!("unknown op {name:?}"))?;
        let v: u32 = freq.parse().map_err(|_| format!("bad freq {freq:?}"))?;
        if !linux
            && v > 0
            && matches!(
                op,
                Op::Fallocate | Op::Punch | Op::Zero | Op::Collapse | Op::Insert | Op::CloneRange | Op::CopyRange
            )
        {
            return Err(format!("op {name} requires Linux"));
        }
        freqs.insert(op_name(op), v);
    }
    if args.read_only {
        for (_, n, _, is_w) in OP_TABLE.iter() {
            if *is_w {
                freqs.insert(n, 0);
            }
        }
    }
    if args.write_only {
        for (_, n, _, is_w) in OP_TABLE.iter() {
            if !*is_w {
                freqs.insert(n, 0);
            }
        }
    }
    let table: Vec<Op> = OP_TABLE
        .iter()
        .flat_map(|(o, n, _, _)| std::iter::repeat(*o).take(freqs[n] as usize))
        .collect();
    if table.is_empty() {
        return Err("all op frequencies are zero".into());
    }
    // pre-open the --json file so the async-signal-safe SIGBUS handler can
    // still honor the "always emit a JSON report" contract
    if let Some(path) = &args.json {
        if let Ok(f) = fs::OpenOptions::new().create(true).write(true).truncate(true).open(path) {
            use std::os::unix::io::IntoRawFd;
            SIGBUS_JSON_FD.store(f.into_raw_fd(), std::sync::atomic::Ordering::Relaxed);
        }
    }
    install_sigbus_guard();
    info!(
        "fsstress: top={} procs={threads} nops={} loops={} seed={seed} distinct_ops={}",
        mountpoint.display(),
        args.nops,
        args.loops,
        table.iter().collect::<std::collections::HashSet<_>>().len()
    );
    let base = mountpoint.to_path_buf();
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let created: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let per_thread: Vec<Value> = thread::scope(|s| {
        let mut joins = Vec::new();
        for tid in 0..threads {
            let base = &base;
            let table = &table;
            let created = created.clone();
            joins.push(s.spawn(move || {
                let mut proc = Proc {
                    st: ThreadState::new(tid, seed),
                    base: base.clone(),
                    verbose: args.verbose,
                    linux,
                    created,
                };
                let started = Instant::now();
                let mut current_index: u64 = 0;
                for _loop in 0..args.loops.max(1) {
                    if args.duration > 0 && started.elapsed().as_secs() >= args.duration {
                        break;
                    }
                    for _ in 0..args.nops {
                        if args.duration > 0 && started.elapsed().as_secs() >= args.duration {
                            break;
                        }
                        let op = table[(proc.st.rng.next() as usize) % table.len()];
                        let r = proc.st.rng.next() as u64;
                        proc.run_op(op, r);
                        current_index += 1;
                        SIGBUS_OPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let hist: Map<String, Value> = proc.st.hist.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
                json!({
                    "proc": tid,
                    "opsDone": current_index,
                    "errors": proc.st.errors,
                    "opHistogram": hist,
                })
            }));
        }
        joins.into_iter().map(|j| j.join().unwrap()).collect()
    });
    if args.cleanup {
        let mut paths: Vec<PathBuf> = created.lock().unwrap().iter().cloned().collect();
        paths.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
        for p in &paths {
            let _ = fs::remove_file(p).or_else(|_| fs::remove_dir(p));
        }
        for _ in 0..4 {
            let mut any = false;
            for p in paths.iter().rev() {
                if fs::remove_dir(p).is_ok() {
                    any = true;
                }
            }
            if !any {
                break;
            }
        }
    }
    let total_ops: u64 = per_thread.iter().map(|v| v["opsDone"].as_u64().unwrap_or(0)).sum();
    let total_errors: u64 = per_thread.iter().map(|v| v["errors"].as_u64().unwrap_or(0)).sum();
    info!(
        "fsstress done: ops={} errors={} (errors are tolerated like upstream; ENOENT/ENOTDIR from races are normal)",
        total_ops, total_errors
    );
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "fsstress",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "params": {
            "threads": threads, "nops": args.nops, "loops": args.loops,
            "seed": seed, "read_only": args.read_only, "write_only": args.write_only,
            "freq_overrides": args.freqs,
        },
        "status": "ok",
        "totalOps": total_ops,
        "totalErrors": total_errors,
        "proc": per_thread,
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}
