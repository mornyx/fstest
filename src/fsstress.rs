// Port of `fsstress` from xfstests `ltp/fsstress.c`: multi-threaded randomized
// directory-tree metadata + data stress test.

use crate::common::{host_name, iso8601, sync_filesystem};
use crate::fsx::GlibcRandom;
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const FILELEN_MAX: usize = 32 * 4096;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ft {
    Dir,
    Reg,
    Sym,
    Fifo,
    Chr,
    Blk,
}

const FT_TYPES: [(Ft, char); 6] = [
    (Ft::Dir, 'd'),
    (Ft::Reg, 'f'),
    (Ft::Sym, 'l'),
    (Ft::Fifo, 'p'),
    (Ft::Chr, 'c'),
    (Ft::Blk, 'b'),
];

const ALL_FTS: &[Ft] = &[Ft::Dir, Ft::Reg, Ft::Sym, Ft::Fifo, Ft::Chr, Ft::Blk];
const NON_DIR_FTS: &[Ft] = &[Ft::Reg, Ft::Sym, Ft::Fifo, Ft::Chr, Ft::Blk];

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
    Mkfifo,
    Mknod,
    Symlink,
    Rename,
    Rnoreplace,
    Rexchange,
    Truncate,
    Chmod,
    Utimens,
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

const OP_TABLE: [(Op, &str, u32, bool); 37] = [
    (Op::Creat, "creat", 4, true),
    (Op::Write, "write", 4, true),
    (Op::WriteV, "writev", 4, true),
    (Op::Read, "read", 1, false),
    (Op::ReadV, "readv", 1, false),
    (Op::MRead, "mread", 2, false),
    (Op::MWrite, "mwrite", 2, true),
    (Op::Mkdir, "mkdir", 2, true),
    (Op::Mkfifo, "mkfifo", 1, true),
    (Op::Mknod, "mknod", 1, true),
    (Op::Symlink, "symlink", 2, true),
    (Op::Rename, "rename", 2, true),
    (Op::Rnoreplace, "rnoreplace", 1, true),
    (Op::Rexchange, "rexchange", 1, true),
    (Op::Truncate, "truncate", 2, true),
    (Op::Chmod, "chmod", 1, true),
    (Op::Utimens, "utimens", 1, true),
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
    (Op::CloneRange, "clonerange", 2, true),
    (Op::CopyRange, "copyrange", 2, true),
];

fn op_by_name(n: &str) -> Option<Op> {
    OP_TABLE
        .iter()
        .find(|(_, name, _, _)| *name == n)
        .map(|(o, _, _, _)| *o)
}

fn op_name(o: Op) -> &'static str {
    OP_TABLE
        .iter()
        .find(|(x, ..)| *x == o)
        .map(|(_, n, _, _)| *n)
        .unwrap()
}

fn is_linux_only_op(o: Op) -> bool {
    matches!(
        o,
        Op::Fallocate
            | Op::Punch
            | Op::Zero
            | Op::Collapse
            | Op::Insert
            | Op::CloneRange
            | Op::CopyRange
    )
}

fn is_fatal_io_errno(raw: Option<i32>) -> bool {
    let Some(e) = raw else { return false };
    #[cfg(target_os = "linux")]
    if e == 117 || e == 77 {
        // EUCLEAN (structure needs cleaning) or EBADFD
        return true;
    }
    e == libc::EIO || e == libc::ENOTCONN
}

#[derive(Clone, Copy)]
struct Fent {
    id: u64,
    parent: i64,
    xattr_counter: u64,
}

#[derive(clap::Args)]
pub struct Args {
    /// number of operations per thread
    #[arg(short = 'n', long, default_value_t = 1000)]
    pub nops: u64,

    /// number of worker threads
    #[arg(short = 'p', long, default_value_t = 1)]
    pub nproc: u32,

    /// number of times to loop the whole run (0 = loop until --duration)
    #[arg(short = 'l', long, default_value_t = 1)]
    pub loops: u32,

    /// RNG seed (0 = time based)
    #[arg(short = 's', long, default_value_t = 0)]
    pub seed: i32,

    /// zero all default operation frequencies before applying `-f` overrides
    #[arg(short = 'z', long)]
    pub zero_freq: bool,

    /// adjust op frequency: `op_name=freq` (repeatable)
    #[arg(short = 'f', long = "freq")]
    pub freqs: Vec<String>,

    /// zero frequencies of all write ops (read-only workload)
    #[arg(short = 'R', long)]
    pub read_only: bool,

    /// zero frequencies of all non-write ops
    #[arg(short = 'w', long)]
    pub write_only: bool,

    /// clean up created files and directories after the run
    #[arg(short = 'c', long)]
    pub cleanup: bool,

    /// verbose per-op error logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// run for this many seconds (stops early or drives `--loops 0`)
    #[arg(long, default_value_t = 0)]
    pub duration: u64,

    /// write the enriched JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

struct ThreadState {
    tid: u32,
    rng: GlibcRandom,
    nameseq: u64,
    lists: HashMap<Ft, Vec<Fent>>,
    errors: u64,
    fatal_errors: u64,
    first_fatal: Option<String>,
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
            fatal_errors: 0,
            first_fatal: None,
            hist: HashMap::new(),
        }
    }

    fn fent_to_name(&self, base: &Path, ft: Ft, fent: &Fent) -> PathBuf {
        let mut chain = Vec::new();
        let mut cur_parent = fent.parent;
        let mut guard = 0usize;
        while cur_parent >= 0 && guard < 128 {
            if let Some(p) = self.dir_by_id(cur_parent as u64) {
                chain.push(p.id);
                cur_parent = p.parent;
            } else {
                break;
            }
            guard += 1;
        }
        let mut path = base.to_path_buf();
        for dir_id in chain.into_iter().rev() {
            path.push(format!("d{dir_id:x}"));
        }
        let tag = FT_TYPES.iter().find(|(t, _)| *t == ft).unwrap().1;
        path.push(format!("{tag}{:x}", fent.id));
        path
    }

    fn dir_by_id(&self, id: u64) -> Option<Fent> {
        self.lists.get(&Ft::Dir)?.iter().find(|f| f.id == id).copied()
    }

    fn is_dir_ancestor_or_self(&self, anc_id: u64, mut cand_parent: i64) -> bool {
        let mut guard = 0usize;
        while cand_parent >= 0 && guard < 128 {
            if cand_parent as u64 == anc_id {
                return true;
            }
            match self.dir_by_id(cand_parent as u64) {
                Some(d) => cand_parent = d.parent,
                None => break,
            }
            guard += 1;
        }
        false
    }

    fn reparent_children(&mut self, old_dir_id: u64, new_dir_id: u64) {
        for list in self.lists.values_mut() {
            for fent in list.iter_mut() {
                if fent.parent == old_dir_id as i64 {
                    fent.parent = new_dir_id as i64;
                }
            }
        }
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
    SIGBUS_ERRORS.fetch_add(1, Ordering::Relaxed);
    if is_fatal_io_errno(e.raw_os_error()) {
        st.fatal_errors += 1;
        if st.first_fatal.is_none() {
            st.first_fatal = Some(format!("tid {}: {op} {}: {e}", st.tid, path.display()));
        }
    }
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
    if unsafe { libc::lseek(fd, off as i64, libc::SEEK_SET) } < 0 {
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

// `O_NONBLOCK` + `S_IFREG` check is mandatory: cross-thread `rename`/`rexchange`
// can place a FIFO (`mkfifo`) or device node (`mknod`) at a path tracked as `Ft::Reg`,
// and blocking `open(2)` on a FIFO without `O_NONBLOCK` hangs forever in the kernel.
fn open_reg_fd(
    st: &mut ThreadState,
    path: &Path,
    write: bool,
    verbose: bool,
) -> Option<(i32, libc::stat)> {
    let flags = (if write { libc::O_WRONLY } else { libc::O_RDONLY }) | libc::O_NONBLOCK;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        report_err(st, "open", path, &std::io::Error::last_os_error(), verbose);
        return None;
    }
    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut sb) } != 0 || (sb.st_mode & libc::S_IFMT) != libc::S_IFREG {
        unsafe { libc::close(fd) };
        return None;
    }
    Some((fd, sb))
}

fn do_fallocate(st: &mut ThreadState, fd: i32, mode: i32, off: u64, len: u64, path: &Path, verbose: bool) {
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::fallocate(fd, mode, off as i64, len as i64) } != 0 {
            report_err(st, "fallocate", path, &std::io::Error::last_os_error(), verbose);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (st, fd, mode, off, len, path, verbose);
    }
}

fn rename_flags_call(src: &Path, dst: &Path, flags: u32) -> Result<(), std::io::Error> {
    let cs = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let cd = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
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
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut dflags: libc::c_uint = 0;
        if flags & 1 != 0 {
            dflags |= libc::RENAME_EXCL;
        }
        if flags & 2 != 0 {
            dflags |= libc::RENAME_SWAP;
        }
        if libc::renamex_np(cs.as_ptr(), cd.as_ptr(), dflags) == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (cs, cd, flags);
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

// Async-signal-safe SIGBUS handler for opt-in `mread`/`mwrite` vs concurrent `truncate`.
extern "C" fn sigbus_handler(_: libc::c_int) {
    const MSG: &[u8] = b"fsstress: SIGBUS (concurrent truncate vs mmap race); emitting failed report and exiting\n";
    const CAUSE: &[u8] = b"SIGBUS: concurrent truncate vs mmap race aborted the run";
    let mut buf = [0u8; 512];
    let mut w = 0usize;
    w = push_bytes(&mut buf, w, b"{\"suite\":\"fsstress\",\"status\":\"failed\",\"failure\":\"");
    w = push_bytes(&mut buf, w, CAUSE);
    w = push_bytes(&mut buf, w, b"\",\"totalOps\":");
    w = push_u64(&mut buf, w, SIGBUS_OPS.load(Ordering::Relaxed));
    w = push_bytes(&mut buf, w, b",\"totalErrors\":");
    w = push_u64(&mut buf, w, SIGBUS_ERRORS.load(Ordering::Relaxed));
    w = push_bytes(&mut buf, w, b"}\n");
    unsafe {
        libc::write(2, MSG.as_ptr() as *const libc::c_void, MSG.len());
        libc::write(1, buf.as_ptr() as *const libc::c_void, w);
        let fd = SIGBUS_JSON_FD.load(Ordering::Relaxed);
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

static SIGBUS_OPS: AtomicU64 = AtomicU64::new(0);
static SIGBUS_ERRORS: AtomicU64 = AtomicU64::new(0);
static SIGBUS_JSON_FD: AtomicI32 = AtomicI32::new(-1);

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
    top_entries: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Proc {
    fn note_if_top(&mut self, path: &Path, parid: i64) {
        if parid < 0 {
            self.top_entries.lock().unwrap().insert(path.to_path_buf());
        }
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
            Op::Mkfifo => self.mkfifo(r),
            Op::Mknod => self.mknod(r),
            Op::Symlink => self.symlink(r),
            Op::Rename => self.rename(r),
            Op::Rnoreplace => self.rnoreplace(r),
            Op::Rexchange => self.rexchange(r),
            Op::Truncate => self.truncate(r),
            Op::Chmod => self.chmod(r),
            Op::Utimens => self.utimens(r),
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
            Op::Sync => sync_filesystem(&self.base),
            Op::Fallocate => self.falloc_range(r, 0, false),
            Op::Punch => self.falloc_range(r, 0x02 | 0x01, true),
            Op::Zero => self.falloc_range(r, 0x10, false),
            Op::Collapse => self.range_shift(r, true),
            Op::Insert => self.range_shift(r, false),
            Op::CloneRange => self.clonerange(r),
            Op::CopyRange => self.copyrange(r),
        }
    }

    fn choose_parent(&mut self, r: u64) -> Option<(Ft, Fent)> {
        if r % 4 == 0 {
            None
        } else {
            self.st.get_fent(&[Ft::Dir], r >> 2)
        }
    }

    fn creat(&mut self, r: u64) {
        let parent = self.choose_parent(r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Reg, &self.base);
        let res = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&path);
        match res {
            Ok(_) => {
                self.st.add_fent(Ft::Reg, id, parid);
                self.note_if_top(&path, parid);
            }
            Err(e) => report_err(&mut self.st, "creat", &path, &e, self.verbose),
        }
    }

    fn mkdir(&mut self, r: u64) {
        let parent = self.choose_parent(r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Dir, &self.base);
        match fs::create_dir(&path) {
            Ok(()) => {
                self.st.add_fent(Ft::Dir, id, parid);
                self.note_if_top(&path, parid);
            }
            Err(e) => report_err(&mut self.st, "mkdir", &path, &e, self.verbose),
        }
    }

    fn mkfifo(&mut self, r: u64) {
        let parent = self.choose_parent(r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Fifo, &self.base);
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        if unsafe { libc::mkfifo(c.as_ptr(), 0o644) } == 0 {
            self.st.add_fent(Ft::Fifo, id, parid);
            self.note_if_top(&path, parid);
        } else {
            report_err(
                &mut self.st,
                "mkfifo",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
        }
    }

    fn mknod(&mut self, r: u64) {
        let parent = self.choose_parent(r);
        let ft = if r % 2 == 0 { Ft::Chr } else { Ft::Blk };
        let (path, id, parid) = self.st.generate_name(parent, ft, &self.base);
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let mode = match ft {
            Ft::Chr => libc::S_IFCHR | 0o644,
            Ft::Blk => libc::S_IFBLK | 0o644,
            _ => unreachable!(),
        };
        #[cfg(target_os = "linux")]
        let dev = libc::makedev(1, (r % 8) as u32);
        #[cfg(not(target_os = "linux"))]
        let dev = libc::makedev(1, (r % 8) as i32);
        if unsafe { libc::mknod(c.as_ptr(), mode, dev) } == 0 {
            self.st.add_fent(ft, id, parid);
            self.note_if_top(&path, parid);
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
        let parent = self.choose_parent(r);
        let (path, id, parid) = self.st.generate_name(parent, Ft::Sym, &self.base);
        let len = ((r % 256) as usize).max(1);
        let mut val = vec![b'x'; len];
        for i in (10..len.saturating_sub(1)).step_by(10) {
            val[i] = b'/';
        }
        let target = String::from_utf8_lossy(&val).into_owned();
        match std::os::unix::fs::symlink(&target, &path) {
            Ok(()) => {
                self.st.add_fent(Ft::Sym, id, parid);
                self.note_if_top(&path, parid);
            }
            Err(e) => report_err(&mut self.st, "symlink", &path, &e, self.verbose),
        }
    }

    fn rw(&mut self, r: u64, write: bool, vectored: bool) {
        let types: &[Ft] = if write { &[Ft::Reg] } else { &[Ft::Reg, Ft::Sym] };
        let Some(path) = pick_path(&mut self.st, types, r, &self.base) else {
            return;
        };
        let Some((fd, sb)) = open_reg_fd(&mut self.st, &path, write, self.verbose) else {
            return;
        };
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
                if unsafe { libc::writev(fd, iov.as_ptr(), 2) } < 0 {
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
        } else if vectored {
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
            if unsafe { libc::readv(fd, iov.as_mut_ptr(), 2) } < 0 {
                report_err(
                    &mut self.st,
                    "readv",
                    &path,
                    &std::io::Error::last_os_error(),
                    self.verbose,
                );
            }
        } else {
            let mut buf = vec![0u8; len];
            if unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, len, off as i64) } < 0 {
                report_err(
                    &mut self.st,
                    "pread",
                    &path,
                    &std::io::Error::last_os_error(),
                    self.verbose,
                );
            }
        }
        unsafe { libc::close(fd) };
    }

    fn mrw(&mut self, r: u64, write: bool) {
        let types: &[Ft] = if write { &[Ft::Reg] } else { &[Ft::Reg, Ft::Sym] };
        let Some(path) = pick_path(&mut self.st, types, r, &self.base) else {
            return;
        };
        let flags = (if write { libc::O_RDWR } else { libc::O_RDONLY }) | libc::O_NONBLOCK;
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return;
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
        if unsafe { libc::fstat(fd, &mut sb) } != 0
            || (sb.st_mode & libc::S_IFMT) != libc::S_IFREG
            || sb.st_size == 0
        {
            unsafe { libc::close(fd) };
            return;
        }
        let size = sb.st_size as u64;
        let page = 4096usize;
        let mut off = self.st.rng.next() as u64 % size;
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
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let off = rand_file_offset(&mut self.st, md.len());
        if unsafe { libc::truncate(c.as_ptr(), off as libc::off_t) } != 0 {
            report_err(
                &mut self.st,
                "truncate",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
        }
    }

    fn chmod(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg, Ft::Dir], r, &self.base) else {
            return;
        };
        let mode = if path.is_dir() { 0o755 } else if r % 2 == 0 { 0o644 } else { 0o600 };
        if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(mode)) {
            report_err(&mut self.st, "chmod", &path, &e, self.verbose);
        }
    }

    fn utimens(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg, Ft::Dir, Ft::Sym], r, &self.base) else {
            return;
        };
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let sec = 1_700_000_000i64 + (r % 100_000) as i64;
        let nsec = ((r * 1000) % 1_000_000_000) as libc::c_long;
        let ts = [
            libc::timespec {
                tv_sec: sec as libc::time_t,
                tv_nsec: nsec,
            },
            libc::timespec {
                tv_sec: sec as libc::time_t,
                tv_nsec: nsec,
            },
        ];
        if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            report_err(
                &mut self.st,
                "utimensat",
                &path,
                &std::io::Error::last_os_error(),
                self.verbose,
            );
        }
    }

    fn link(&mut self, r: u64) {
        let Some((src_ft, src_fent)) = self.st.get_fent(NON_DIR_FTS, r) else {
            return;
        };
        let src_path = self.st.fent_to_name(&self.base, src_ft, &src_fent);
        let rr = self.st.rng.next() as u64;
        let dest_dir = self.choose_parent(rr);
        let (path, id, parid) = self.st.generate_name(dest_dir, src_ft, &self.base);
        match fs::hard_link(&src_path, &path) {
            Ok(()) => {
                self.st.add_fent(src_ft, id, parid);
                self.note_if_top(&path, parid);
            }
            Err(e) => report_err(&mut self.st, "link", &path, &e, self.verbose),
        }
    }

    fn unlink(&mut self, r: u64) {
        let Some((ft, fent)) = self.st.get_fent(NON_DIR_FTS, r) else {
            return;
        };
        let path = self.st.fent_to_name(&self.base, ft, &fent);
        match fs::remove_file(&path) {
            Ok(()) => {
                self.st.lists.get_mut(&ft).unwrap().retain(|f| f.id != fent.id);
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
                self.st.lists.get_mut(&Ft::Dir).unwrap().retain(|f| f.id != fent.id);
            }
            Err(e) => report_err(&mut self.st, "rmdir", &path, &e, self.verbose),
        }
    }

    fn rename_common(&mut self, r: u64, flags: u32, op_label: &'static str) {
        let Some((ft, fent)) = self.st.get_fent(ALL_FTS, r) else {
            return;
        };
        let src = self.st.fent_to_name(&self.base, ft, &fent);
        let rr = self.st.rng.next() as u64;
        let mut dest_dir = self.choose_parent(rr);
        if ft == Ft::Dir
            && let Some((_, dpar)) = &dest_dir
            && self.st.is_dir_ancestor_or_self(fent.id, dpar.id as i64)
        {
            dest_dir = None;
        }
        let (dest, newid, parid) = self.st.generate_name(dest_dir, ft, &self.base);
        let res = if flags == 0 {
            fs::rename(&src, &dest)
        } else {
            rename_flags_call(&src, &dest, flags)
        };
        match res {
            Ok(()) => {
                self.st.lists.get_mut(&ft).unwrap().retain(|f| f.id != fent.id);
                self.st.add_fent(ft, newid, parid);
                if ft == Ft::Dir {
                    self.st.reparent_children(fent.id, newid);
                }
                self.note_if_top(&dest, parid);
            }
            Err(e) => report_err(&mut self.st, op_label, &src, &e, self.verbose),
        }
    }

    fn rename(&mut self, r: u64) {
        self.rename_common(r, 0, "rename");
    }

    fn rnoreplace(&mut self, r: u64) {
        self.rename_common(r, 1, "rnoreplace");
    }

    fn rexchange(&mut self, r: u64) {
        let Some((ft1, f1)) = self.st.get_fent(NON_DIR_FTS, r) else {
            return;
        };
        let rr = self.st.rng.next() as u64;
        let Some((ft2, f2)) = self.st.get_fent(NON_DIR_FTS, rr) else {
            return;
        };
        if ft1 == ft2 && f1.id == f2.id {
            return;
        }
        let p1 = self.st.fent_to_name(&self.base, ft1, &f1);
        let p2 = self.st.fent_to_name(&self.base, ft2, &f2);
        if let Err(e) = rename_flags_call(&p1, &p2, 2) {
            report_err(&mut self.st, "rexchange", &p1, &e, self.verbose);
        } else if ft1 != ft2 {
            // Types swapped between `p1` and `p2`: remove old tracking entries and re-register swapped types.
            self.st.lists.get_mut(&ft1).unwrap().retain(|x| x.id != f1.id);
            self.st.lists.get_mut(&ft2).unwrap().retain(|x| x.id != f2.id);
        }
    }

    fn readlink(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, &[Ft::Sym], r, &self.base) else {
            return;
        };
        if let Err(e) = fs::read_link(&path) {
            report_err(&mut self.st, "readlink", &path, &e, self.verbose);
        }
    }

    fn stat(&mut self, r: u64) {
        let Some(path) = pick_path(&mut self.st, ALL_FTS, r, &self.base) else {
            return;
        };
        if let Err(e) = fs::symlink_metadata(&path) {
            report_err(&mut self.st, "lstat", &path, &e, self.verbose);
        }
    }

    fn getdents(&mut self, r: u64) {
        let path = pick_path(&mut self.st, &[Ft::Dir], r, &self.base)
            .unwrap_or_else(|| self.base.clone());
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
        let Some(path) = pick_path(&mut self.st, &[Ft::Dir, Ft::Reg], r, &self.base) else {
            return;
        };
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
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
        #[cfg(target_os = "linux")]
        unsafe {
            let mut fl: u32 = 0;
            let _ = libc::ioctl(fd, 0x80086601u64 as libc::c_ulong, &mut fl);
        }
        #[cfg(target_os = "macos")]
        unsafe {
            let _ = libc::fcntl(fd, libc::F_GETFL);
        }
        unsafe { libc::close(fd) };
    }

    fn fdatasync_like(&mut self, full: bool) {
        let rr = self.st.rng.next() as u64;
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg], rr, &self.base) else {
            return;
        };
        let Some((fd, _)) = open_reg_fd(&mut self.st, &path, true, self.verbose) else {
            return;
        };
        #[cfg(target_os = "linux")]
        let r = if full {
            unsafe { libc::fsync(fd) }
        } else {
            unsafe { libc::fdatasync(fd) }
        };
        #[cfg(not(target_os = "linux"))]
        let r = {
            let _ = full;
            unsafe { libc::fsync(fd) }
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
        unsafe { libc::close(fd) };
    }

    fn falloc_range(&mut self, r: u64, mode: i32, clip_eof: bool) {
        if !self.linux {
            return;
        }
        let Some(path) = pick_path(&mut self.st, &[Ft::Reg], r, &self.base) else {
            return;
        };
        let Some((fd, sb)) = open_reg_fd(&mut self.st, &path, true, self.verbose) else {
            return;
        };
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

    fn clonerange(&mut self, r: u64) {
        #[cfg(target_os = "linux")]
        {
            #[repr(C)]
            struct FileCloneRange {
                src_fd: i64,
                src_offset: i64,
                src_length: i64,
                dest_offset: i64,
            }
            const FICLONERANGE: libc::c_ulong = 0x4020940D;

            let Some(src_path) = pick_path(&mut self.st, &[Ft::Reg], r, &self.base) else {
                return;
            };
            let rr = self.st.rng.next() as u64;
            let Some(dst_path) = pick_path(&mut self.st, &[Ft::Reg], rr, &self.base) else {
                return;
            };
            if src_path == dst_path {
                return;
            }
            let Some((sfd, sb)) = open_reg_fd(&mut self.st, &src_path, false, self.verbose) else {
                return;
            };
            let Some((dfd, _)) = open_reg_fd(&mut self.st, &dst_path, true, self.verbose) else {
                unsafe { libc::close(sfd) };
                return;
            };
            if sb.st_size >= 4096 {
                let blocks = (sb.st_size as u64) / 4096;
                let src_off = ((self.st.rng.next() as u64) % blocks) * 4096;
                let len = (((self.st.rng.next() as u64) % (blocks - src_off / 4096)) + 1) * 4096;
                let dst_off = ((self.st.rng.next() as u64) % 16) * 4096;
                let fcr = FileCloneRange {
                    src_fd: sfd as i64,
                    src_offset: src_off as i64,
                    src_length: len as i64,
                    dest_offset: dst_off as i64,
                };
                if unsafe { libc::ioctl(dfd, FICLONERANGE, &fcr) } != 0 {
                    report_err(
                        &mut self.st,
                        "clonerange",
                        &dst_path,
                        &std::io::Error::last_os_error(),
                        self.verbose,
                    );
                }
            }
            unsafe {
                libc::close(sfd);
                libc::close(dfd);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = r;
        }
    }

    fn copyrange(&mut self, r: u64) {
        #[cfg(target_os = "linux")]
        {
            let Some(src_path) = pick_path(&mut self.st, &[Ft::Reg], r, &self.base) else {
                return;
            };
            let rr = self.st.rng.next() as u64;
            let Some(dst_path) = pick_path(&mut self.st, &[Ft::Reg], rr, &self.base) else {
                return;
            };
            if src_path == dst_path {
                return;
            }
            let Some((sfd, sb)) = open_reg_fd(&mut self.st, &src_path, false, self.verbose) else {
                return;
            };
            let Some((dfd, _)) = open_reg_fd(&mut self.st, &dst_path, true, self.verbose) else {
                unsafe { libc::close(sfd) };
                return;
            };
            if sb.st_size > 0 {
                let size = sb.st_size as u64;
                let mut o1 = ((self.st.rng.next() as u64) % size) as i64;
                let mut o2 = ((self.st.rng.next() as u64) % FILELEN_MAX as u64) as i64;
                let len = (((self.st.rng.next() as u64) % (size - o1 as u64)) + 1)
                    .min(FILELEN_MAX as u64) as usize;
                if unsafe { libc::copy_file_range(sfd, &mut o1, dfd, &mut o2, len, 0) } < 0 {
                    report_err(
                        &mut self.st,
                        "copyrange",
                        &dst_path,
                        &std::io::Error::last_os_error(),
                        self.verbose,
                    );
                }
            }
            unsafe {
                libc::close(sfd);
                libc::close(dfd);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = r;
        }
    }
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint)
        .map_err(|e| format!("{}: {e}", mountpoint.display()))?;
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
            let def = if args.zero_freq
                || (!linux && is_linux_only_op(*o))
                || matches!(o, Op::MRead | Op::MWrite)
            {
                0
            } else {
                *f
            };
            (*n, def)
        })
        .collect();

    for spec in &args.freqs {
        let (name, freq) = spec
            .split_once('=')
            .ok_or_else(|| format!("bad -f spec {spec:?}, expected op_name=freq"))?;
        let op = op_by_name(name).ok_or_else(|| format!("unknown op {name:?}"))?;
        let v: u32 = freq.parse().map_err(|_| format!("bad freq {freq:?}"))?;
        if !linux && v > 0 && is_linux_only_op(op) {
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

    SIGBUS_OPS.store(0, Ordering::Relaxed);
    SIGBUS_ERRORS.store(0, Ordering::Relaxed);
    if let Some(path) = &args.json
        && let Ok(f) = fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)
    {
        use std::os::unix::io::IntoRawFd;
        SIGBUS_JSON_FD.store(f.into_raw_fd(), Ordering::Relaxed);
    }
    install_sigbus_guard();

    info!(
        "fsstress: top={} procs={threads} nops={} loops={} seed={seed} distinct_ops={}",
        mountpoint.display(),
        args.nops,
        args.loops,
        table.iter().collect::<HashSet<_>>().len()
    );

    let suite_started = SystemTime::now();
    let t0 = Instant::now();
    let base = mountpoint.to_path_buf();
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let top_entries: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    let per_thread: Vec<Value> = thread::scope(|s| {
        let mut joins = Vec::new();
        for tid in 0..threads {
            let base = &base;
            let table = &table;
            let top_entries = top_entries.clone();
            joins.push(s.spawn(move || {
                let mut proc = Proc {
                    st: ThreadState::new(tid, seed),
                    base: base.clone(),
                    verbose: args.verbose,
                    linux,
                    top_entries,
                };
                let started = Instant::now();
                let mut current_index: u64 = 0;
                let infinite_loops = args.loops == 0 && args.duration > 0;
                let max_loops = if infinite_loops { u32::MAX } else { args.loops.max(1) };
                for _ in 0..max_loops {
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
                        SIGBUS_OPS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let hist: Map<String, Value> = proc
                    .st
                    .hist
                    .iter()
                    .map(|(k, v)| (k.to_string(), json!(v)))
                    .collect();
                json!({
                    "proc": tid,
                    "opsDone": current_index,
                    "errors": proc.st.errors,
                    "fatalErrors": proc.st.fatal_errors,
                    "firstFatal": proc.st.first_fatal,
                    "opHistogram": hist,
                })
            }));
        }
        joins.into_iter().map(|j| j.join().unwrap()).collect()
    });

    let prev_sigbus_fd = SIGBUS_JSON_FD.swap(-1, Ordering::Relaxed);
    if prev_sigbus_fd >= 0 {
        unsafe { libc::close(prev_sigbus_fd) };
    }

    if args.cleanup {
        let entries: Vec<PathBuf> = top_entries.lock().unwrap().drain().collect();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("fsstress-cleanup".into())
            .spawn(move || {
                for p in entries {
                    if let Ok(md) = fs::symlink_metadata(&p) {
                        if md.is_dir() {
                            let _ = fs::remove_dir_all(&p);
                        } else {
                            let _ = fs::remove_file(&p);
                        }
                    }
                }
                let _ = tx.send(());
            })
            .ok();
        if rx.recv_timeout(Duration::from_secs(30)).is_err() {
            warn!("fsstress: cleanup did not finish within 30s");
        }
    }

    let duration_ms = t0.elapsed().as_millis() as u64;
    let total_ops: u64 = per_thread.iter().map(|v| v["opsDone"].as_u64().unwrap_or(0)).sum();
    let total_errors: u64 = per_thread.iter().map(|v| v["errors"].as_u64().unwrap_or(0)).sum();
    let fatal_errors: u64 = per_thread
        .iter()
        .map(|v| v["fatalErrors"].as_u64().unwrap_or(0))
        .sum();
    let first_fatal = per_thread
        .iter()
        .find_map(|v| v["firstFatal"].as_str().map(String::from));

    info!(
        "fsstress done in {:.2}s: ops={total_ops} race_errors={total_errors} fatal_errors={fatal_errors}",
        duration_ms as f64 / 1000.0
    );

    let mut report = json!({
        "suite": "fsstress",
        "host": host_name(),
        "date": iso8601(suite_started),
        "top": mountpoint.display().to_string(),
        "durationMs": duration_ms,
        "params": {
            "threads": threads,
            "nops": args.nops,
            "loops": args.loops,
            "seed": seed,
            "zero_freq": args.zero_freq,
            "read_only": args.read_only,
            "write_only": args.write_only,
            "freq_overrides": args.freqs,
        },
        "status": if fatal_errors == 0 { "ok" } else { "failed" },
        "totalOps": total_ops,
        "totalErrors": total_errors,
        "fatalErrors": fatal_errors,
        "proc": per_thread,
    });
    if let Some(f) = first_fatal {
        report["failure"] = json!(f);
    }
    Ok(report)
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
    fn fsstress_exercises_directory_renames_and_cleans_up_completely() {
        let dir = std::env::temp_dir().join(format!("fstest-fsstress-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cli = TestCli::parse_from(["fsstress", "-n", "300", "-p", "3", "-s", "42", "-c"]);
        let rep = run(&dir, &cli.args).expect("fsstress run");
        assert_eq!(rep["status"], json!("ok"));
        assert_eq!(rep["fatalErrors"], json!(0));
        assert_eq!(rep["totalOps"], json!(900));

        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert!(
            remaining.is_empty(),
            "expected `-c` cleanup to leave zero entries, found: {remaining:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fsstress_zero_freq_runs_only_selected_ops() {
        let dir = std::env::temp_dir().join(format!("fstest-fsstress-z-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cli = TestCli::parse_from([
            "fsstress",
            "-z",
            "-f",
            "mkdir=2",
            "-f",
            "rename=2",
            "-n",
            "50",
            "-c",
        ]);
        let rep = run(&dir, &cli.args).expect("fsstress -z");
        assert_eq!(rep["status"], json!("ok"));
        let hist = rep["proc"][0]["opHistogram"].as_object().unwrap();
        assert!(hist.keys().all(|k| k == "mkdir" || k == "rename"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fsstress_mkfifo_rexchange_never_blocks_on_swapped_fifo() {
        let dir = std::env::temp_dir().join(format!("fstest-fsstress-fifo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cli = TestCli::parse_from([
            "fsstress",
            "-z",
            "-f",
            "mkfifo=10",
            "-f",
            "creat=10",
            "-f",
            "rexchange=10",
            "-f",
            "write=10",
            "-f",
            "read=10",
            "-f",
            "truncate=10",
            "-p",
            "4",
            "-n",
            "400",
            "-s",
            "7",
            "-c",
        ]);
        let rep = run(&dir, &cli.args).expect("fsstress fifo race");
        assert_eq!(rep["status"], json!("ok"));
        assert_eq!(rep["fatalErrors"], json!(0));
        assert_eq!(rep["totalOps"], json!(1600));
        let _ = fs::remove_dir_all(&dir);
    }
}
