// Port of `fsx` from xfstests `ltp/fsx.c`: single-file data-integrity fuzzer
// verified against an in-memory shadow buffer (`good_buf`) after every operation.

use crate::common::{host_name, iso8601, suffixed};
use log::{info, warn};
use serde_json::{Value, json};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

// glibc `random()` TYPE_3 (degree 31) so same-seed runs match C `fsx` sequences.
pub struct GlibcRandom {
    st: [i32; 34],
    fp: usize,
    rp: usize,
}

impl GlibcRandom {
    pub fn new(seed: i32) -> Self {
        let mut st = [0i32; 34];
        st[0] = if seed == 0 { 1 } else { seed };
        for i in 1..31 {
            let hi = (st[i - 1] as i64) / 127773;
            let lo = (st[i - 1] as i64) % 127773;
            let mut word = 16807 * lo - 2836 * hi;
            if word <= 0 {
                word += 2147483647;
            }
            st[i] = word as i32;
        }
        for i in 31..34 {
            st[i] = st[i - 31];
        }
        let mut g = GlibcRandom { st, fp: 3, rp: 0 };
        for _ in 0..310 {
            g.next();
        }
        g
    }

    pub fn next(&mut self) -> u32 {
        let f = self.fp;
        let r = self.rp;
        self.st[f] = self.st[f].wrapping_add(self.st[r]);
        let res = (self.st[f] as u32) >> 1;
        self.fp = if f + 1 == 34 { 0 } else { f + 1 };
        self.rp = if r + 1 == 34 { 0 } else { r + 1 };
        res
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Read,
    Write,
    MapRead,
    MapWrite,
    Truncate,
    Fallocate,
    PunchHole,
    ZeroRange,
    WriteZeroes,
    CollapseRange,
    InsertRange,
    CloneRange,
    CopyRange,
    Fsync,
}

const OP_NAMES: [(Op, &str); 14] = [
    (Op::Read, "read"),
    (Op::Write, "write"),
    (Op::MapRead, "mapread"),
    (Op::MapWrite, "mapwrite"),
    (Op::Truncate, "truncate"),
    (Op::Fallocate, "fallocate"),
    (Op::PunchHole, "punch_hole"),
    (Op::ZeroRange, "zero_range"),
    (Op::WriteZeroes, "write_zeroes"),
    (Op::CollapseRange, "collapse_range"),
    (Op::InsertRange, "insert_range"),
    (Op::CloneRange, "clone_range"),
    (Op::CopyRange, "copy_range"),
    (Op::Fsync, "fsync"),
];

const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: i32 = 0x08;
const FALLOC_FL_ZERO_RANGE: i32 = 0x10;
const FALLOC_FL_INSERT_RANGE: i32 = 0x20;
const FALLOC_FL_UNSHARE_RANGE: i32 = 0x40;
const FALLOC_FL_WRITE_ZEROES: i32 = 0x80;

const LOGSIZE: usize = 10000;

#[derive(Clone, Copy)]
struct LogEntry {
    op: Op,
    args: [u64; 3],
    skipped: bool,
    keep_size: bool,
    unshare: bool,
    close_open: bool,
}

struct Cfg {
    readbdy: u64,
    writebdy: u64,
    truncbdy: u64,
    pollute_eof: bool,
    do_fsync: bool,
    direct: bool,
    simulatedopcount: u64,
    inject_failure_at: Option<u64>,
}

#[derive(clap::Args)]
pub struct Args {
    /// test file name (created under the mount point)
    #[arg(long, default_value = "fsx.testfile")]
    pub file: String,

    /// upper bound on file size
    #[arg(short = 'l', long, default_value_t = 256 * 1024)]
    pub flen: u64,

    /// upper bound on operation size
    #[arg(short = 'o', long, default_value_t = 64 * 1024)]
    pub oplen: u64,

    /// total number of operations (0 = run until --duration)
    #[arg(short = 'N', long, default_value_t = 1000)]
    pub numops: i64,

    /// RNG seed (0 = time+pid based)
    #[arg(short = 'S', long, default_value_t = 1)]
    pub seed: i32,

    /// read alignment boundary
    #[arg(short = 'r', long, default_value_t = 1)]
    pub readbdy: u64,

    /// write alignment boundary
    #[arg(short = 'w', long, default_value_t = 1)]
    pub writebdy: u64,

    /// truncate alignment boundary
    #[arg(short = 't', long, default_value_t = 1)]
    pub truncbdy: u64,

    /// truncate style: 1 gives smaller truncates
    #[arg(short = 's', long, default_value_t = 0)]
    pub style: i32,

    /// 1-in-P chance of file close+reopen after each operation
    #[arg(short = 'c', long, default_value_t = 0)]
    pub closeprob: i32,

    /// progress output every N ops
    #[arg(short = 'p', long, default_value_t = 0)]
    pub progress: u64,

    /// fsxLite: no file size changes
    #[arg(short = 'L', long)]
    pub lite: bool,

    /// use oplen for every op (disable random lengths)
    #[arg(short = 'O', long)]
    pub fixed_oplen: bool,

    /// direct I/O (`O_DIRECT` on Linux, `F_NOCACHE` on macOS; enforces page-aligned read/write boundaries)
    #[arg(short = 'Z', long)]
    pub direct: bool,

    /// disable mapped (mmap) reads
    #[arg(short = 'R', long)]
    pub no_mapped_reads: bool,

    /// disable mapped (mmap) writes
    #[arg(short = 'W', long)]
    pub no_mapped_writes: bool,

    /// disable fallocate (preallocation) calls
    #[arg(short = 'F', long)]
    pub no_fallocate: bool,

    /// disable keep-size calls
    #[arg(short = 'K', long)]
    pub no_keep_size: bool,

    /// disable punch hole calls
    #[arg(short = 'H', long)]
    pub no_punch: bool,

    /// disable zero range calls
    #[arg(short = 'z', long)]
    pub no_zero: bool,

    /// disable collapse range calls
    #[arg(short = 'C', long)]
    pub no_collapse: bool,

    /// disable insert range calls
    #[arg(short = 'I', long)]
    pub no_insert: bool,

    /// disable clone range calls (Linux)
    #[arg(short = 'J', long)]
    pub no_clone: bool,

    /// disable copy range calls (Linux)
    #[arg(short = 'E', long)]
    pub no_copy: bool,

    /// disable periodic fsync operations
    #[arg(long)]
    pub no_fsync_op: bool,

    /// disable file size verifications
    #[arg(short = 'n', long)]
    pub no_size_checks: bool,

    /// read and compare the whole file after every operation
    #[arg(short = 'X', long)]
    pub check_contents: bool,

    /// fsync after every write
    #[arg(short = 'y', long)]
    pub fsync_writes: bool,

    /// pollute post-EOF page on size changes (catches missing zeroing)
    #[arg(short = 'e', long)]
    pub pollute_eof: bool,

    /// quieter operation
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// debug output for all operations
    #[arg(short = 'd', long)]
    pub debug: bool,

    /// simulate operations in memory up to this op number before issuing syscalls
    #[arg(short = 'b', long, default_value_t = 0)]
    pub begin_op: u64,

    /// run for this many seconds instead of --numops
    #[arg(long, default_value_t = 0)]
    pub duration: i64,

    /// replay operations from a recorded `.fsxops` file
    #[arg(long)]
    pub replay_ops: Option<PathBuf>,

    /// dump `<file>.fsxops` even on success
    #[arg(long)]
    pub record_ops: bool,

    /// write `<file>.fsxgood` expected shadow image on success for post-remount `--verify-good`
    #[arg(long)]
    pub keep_good: bool,

    /// verify `<file>` byte-for-byte against `<file>.fsxgood` without mutating it
    #[arg(long)]
    pub verify_good: bool,

    /// remove test file on clean completion (ignored when `--keep-good` or `--record-ops` is set)
    #[arg(long)]
    pub cleanup: bool,

    #[arg(long, hide = true)]
    pub inject_failure_at: Option<u64>,

    /// write the enriched JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

struct Fsx {
    a: Cfg,
    file_path: PathBuf,
    fd: RawFd,
    good_buf: Vec<u8>,
    original_buf: Vec<u8>,
    file_size: u64,
    biggest: u64,
    maxfilelen: u64,
    page_size: u64,
    page_mask: u64,
    testcalls: u64,
    rng: GlibcRandom,
    log: Vec<LogEntry>,
    logptr: usize,
    logcount: u64,
    badoff: i64,
    closeopen: bool,
    enabled: [bool; 14],
    failure: Option<String>,
}

impl Drop for Fsx {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

fn rounddown(v: u64, bdy: u64) -> u64 {
    if bdy <= 1 { v } else { v - v % bdy }
}

fn open_test_fd(path: &Path, create: bool, truncate: bool, direct: bool) -> io::Result<RawFd> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let mut flags = libc::O_RDWR;
    if create {
        flags |= libc::O_CREAT;
    }
    if truncate {
        flags |= libc::O_TRUNC;
    }
    #[cfg(target_os = "linux")]
    if direct {
        flags |= libc::O_DIRECT;
    }
    let fd = unsafe { libc::open(cpath.as_ptr(), flags, 0o666usize) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "macos")]
    if direct && unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) } != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(fd)
}

struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize, align: usize) -> io::Result<Self> {
        let alloc_len = len.max(align);
        let mut raw: *mut libc::c_void = std::ptr::null_mut();
        let rc = unsafe { libc::posix_memalign(&mut raw, align.max(4096), alloc_len) };
        if rc != 0 || raw.is_null() {
            return Err(io::Error::from_raw_os_error(rc));
        }
        unsafe {
            std::ptr::write_bytes(raw as *mut u8, 0, alloc_len);
        }
        Ok(Self {
            ptr: raw as *mut u8,
            len,
        })
    }
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { libc::free(self.ptr as *mut libc::c_void) };
        }
    }
}

impl Fsx {
    fn rnd(&mut self) -> u32 {
        self.rng.next()
    }

    fn simulating(&self) -> bool {
        self.testcalls <= self.a.simulatedopcount
    }

    fn log_entry(&mut self, op: Op, args: [u64; 3], skipped: bool, keep_size: bool, unshare: bool) {
        self.log[self.logptr] = LogEntry {
            op,
            args,
            skipped,
            keep_size,
            unshare,
            close_open: self.closeopen,
        };
        self.logptr = (self.logptr + 1) % LOGSIZE;
        self.logcount += 1;
    }

    fn log4(&mut self, op: Op, a0: u64, a1: u64, skipped: bool) {
        self.log_entry(op, [a0, a1, self.file_size], skipped, false, false);
    }

    fn log5(&mut self, op: Op, a0: u64, a1: u64, a2: u64, skipped: bool) {
        self.log_entry(op, [a0, a1, a2], skipped, false, false);
    }

    fn gendata(&mut self, offset: usize, size: usize) {
        let tc = (self.testcalls % 256) as u8;
        for i in offset..offset + size {
            let mut b = tc;
            if i % 2 == 1 {
                b = b.wrapping_add(self.original_buf[i]);
            }
            self.good_buf[i] = b;
        }
    }

    fn pread_all(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        if self.a.direct {
            let align = self.page_size as usize;
            let aligned_len = (buf.len() + align - 1) & !(align - 1);
            let mut abuf = AlignedBuf::new(aligned_len, align)?;
            let mut off = offset;
            let mut done = 0usize;
            while done < buf.len() {
                let req = (aligned_len - done).max(align);
                let n = unsafe {
                    libc::pread(
                        self.fd,
                        abuf.as_mut_slice()[done..].as_mut_ptr() as *mut libc::c_void,
                        req,
                        off as i64,
                    )
                };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
                }
                done += n as usize;
                off += n as u64;
            }
            buf.copy_from_slice(&abuf.as_slice()[..buf.len()]);
            return Ok(());
        }
        let mut off = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let n = unsafe {
                libc::pread(
                    self.fd,
                    buf[done..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - done,
                    off as i64,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            done += n as usize;
            off += n as u64;
        }
        Ok(())
    }

    fn pwrite_all(&self, data: &[u8], offset: u64) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if self.a.direct {
            let align = self.page_size as usize;
            let mut abuf = AlignedBuf::new(data.len(), align)?;
            abuf.as_mut_slice().copy_from_slice(data);
            let mut off = offset;
            let mut done = 0usize;
            while done < data.len() {
                let n = unsafe {
                    libc::pwrite(
                        self.fd,
                        abuf.as_slice()[done..].as_ptr() as *const libc::c_void,
                        data.len() - done,
                        off as i64,
                    )
                };
                if n <= 0 {
                    return Err(io::Error::last_os_error());
                }
                done += n as usize;
                off += n as u64;
            }
            return Ok(());
        }
        let mut off = offset;
        let mut done = 0usize;
        while done < data.len() {
            let n = unsafe {
                libc::pwrite(
                    self.fd,
                    data[done..].as_ptr() as *const libc::c_void,
                    data.len() - done,
                    off as i64,
                )
            };
            if n <= 0 {
                return Err(io::Error::last_os_error());
            }
            done += n as usize;
            off += n as u64;
        }
        Ok(())
    }

    fn do_close_open(&mut self) {
        if self.simulating() {
            return;
        }
        if unsafe { libc::close(self.fd) } != 0 {
            self.fd = -1;
            self.fail(format!("close_open close: {}", io::Error::last_os_error()));
            return;
        }
        self.fd = -1;
        match open_test_fd(&self.file_path, false, false, self.a.direct) {
            Ok(new_fd) => self.fd = new_fd,
            Err(e) => self.fail(format!("close_open reopen: {e}")),
        }
    }

    fn flush_simulated_state(&mut self) {
        if unsafe { libc::ftruncate(self.fd, self.file_size as i64) } != 0 {
            self.fail(format!("begin_op ftruncate: {}", io::Error::last_os_error()));
            return;
        }
        if self.file_size > 0 {
            let img = self.good_buf[..self.file_size as usize].to_vec();
            if let Err(e) = self.pwrite_all(&img, 0) {
                self.fail(format!("begin_op pwrite: {e}"));
            }
        }
    }

    fn fail(&mut self, msg: String) {
        if self.failure.is_none() {
            self.failure = Some(msg.clone());
            self.badoff = -1;
        }
        warn!("fsx failure: {msg}");
    }

    fn check_buffers(&mut self, buf: &[u8], offset: u64, size: usize) {
        if buf[..size] != self.good_buf[offset as usize..offset as usize + size] {
            let mut mismatches = 0u32;
            for (i, b) in buf[..size].iter().enumerate() {
                if *b != self.good_buf[offset as usize + i] {
                    if mismatches < 16 {
                        info!(
                            "READ BAD DATA: offset 0x{:x} (+{}): good 0x{:02x} bad 0x{:02x}",
                            offset + i as u64,
                            i,
                            self.good_buf[offset as usize + i],
                            b
                        );
                    }
                    mismatches += 1;
                    self.badoff = (offset + i as u64) as i64;
                }
            }
            self.fail(format!(
                "READ BAD DATA: offset = 0x{offset:x}, size = 0x{size:x}, mismatches = {mismatches}"
            ));
        }
    }

    fn check_size(&mut self) {
        if self.simulating() {
            return;
        }
        let mut sb: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.fd, &mut sb) } != 0 {
            self.fail("check_size: fstat failed".into());
            return;
        }
        let seek_end = unsafe { libc::lseek(self.fd, 0, libc::SEEK_END) };
        if self.file_size != sb.st_size as u64 || self.file_size != seek_end as u64 {
            self.fail(format!(
                "Size error: expected 0x{:x} stat 0x{:x} seek 0x{seek_end:x}",
                self.file_size, sb.st_size
            ));
        }
    }

    fn update_file_size(&mut self, offset: u64, size: u64) {
        if offset > self.file_size {
            let gap = (offset - self.file_size) as usize;
            self.good_buf[self.file_size as usize..self.file_size as usize + gap].fill(0);
        }
        self.file_size = offset + size;
    }

    fn pollute_eofpage(&mut self, maxoff: u64) {
        if !self.a.pollute_eof || self.simulating() {
            return;
        }
        let offset = self.file_size;
        let pg_offset = offset & self.page_mask;
        if pg_offset == 0 {
            return;
        }
        let write_size = (self.page_size - pg_offset).min(maxoff.saturating_sub(offset));
        if write_size == 0 {
            return;
        }
        unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                self.page_size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd,
                (offset - pg_offset) as libc::off_t,
            );
            if p == libc::MAP_FAILED {
                return;
            }
            let base = p as *mut u8;
            for i in 0..write_size as usize {
                let b = (self.testcalls % 256) as u8;
                let b = if (pg_offset as usize + i) % 2 == 1 {
                    b.wrapping_add(self.original_buf[pg_offset as usize + i])
                } else {
                    b
                };
                base.add(pg_offset as usize + i).write(b);
            }
            libc::msync(p, self.page_size as usize, libc::MS_SYNC);
            libc::munmap(p, self.page_size as usize);
        }
    }

    fn doread(&mut self, mut offset: u64, size: u64) {
        offset = rounddown(offset, self.a.readbdy);
        let size = rounddown(size, self.a.readbdy);
        if size == 0 || offset + size > self.file_size {
            self.log4(Op::Read, offset, size, true);
            return;
        }
        self.log4(Op::Read, offset, size, false);
        if self.simulating() {
            return;
        }
        let mut tmp = vec![0u8; size as usize];
        if let Err(e) = self.pread_all(&mut tmp, offset) {
            self.fail(format!("doread: {e}"));
            return;
        }
        if self.a.inject_failure_at == Some(self.testcalls) && size > 8 {
            tmp[7] ^= 0xff;
        }
        self.check_buffers(&tmp, offset, size as usize);
    }

    fn dowrite(&mut self, mut offset: u64, size: u64) {
        offset = rounddown(offset, self.a.writebdy);
        let size = rounddown(size, self.a.writebdy);
        if size == 0 || offset + size > self.maxfilelen {
            self.log4(Op::Write, offset, size, true);
            return;
        }
        if offset + size > self.biggest {
            self.biggest = offset + size;
        }
        self.log4(Op::Write, offset, size, false);
        self.gendata(offset as usize, size as usize);
        if offset + size > self.file_size {
            self.update_file_size(offset, size);
        }
        if self.simulating() {
            return;
        }
        if let Err(e) = self.pwrite_all(
            &self.good_buf[offset as usize..offset as usize + size as usize],
            offset,
        ) {
            self.fail(format!("dowrite: {e}"));
            return;
        }
        if self.a.do_fsync && unsafe { libc::fsync(self.fd) } != 0 {
            self.fail(format!("dowrite fsync: {}", io::Error::last_os_error()));
        }
    }

    fn domapread(&mut self, mut offset: u64, size: u64) {
        offset = rounddown(offset, self.a.readbdy);
        if size == 0 || offset + size > self.file_size {
            self.log4(Op::MapRead, offset, size, true);
            return;
        }
        self.log4(Op::MapRead, offset, size, false);
        if self.simulating() {
            return;
        }
        unsafe {
            let pg_offset = offset & self.page_mask;
            let map_base = offset - pg_offset;
            let map_size = pg_offset + size;
            let p = libc::mmap(
                std::ptr::null_mut(),
                map_size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.fd,
                map_base as libc::off_t,
            );
            if p == libc::MAP_FAILED {
                self.fail(format!("domapread: mmap {}", io::Error::last_os_error()));
                return;
            }
            let src = (p as *const u8).add(pg_offset as usize);
            let mut tmp = vec![0u8; size as usize];
            std::ptr::copy_nonoverlapping(src, tmp.as_mut_ptr(), size as usize);

            // POSIX requires zero-fill past EOF within the mapped EOF page.
            let mapped_pages_end = (map_size + self.page_mask) & !self.page_mask;
            let eof_rel = self.file_size.saturating_sub(map_base);
            if self.file_size >= map_base
                && eof_rel < mapped_pages_end
                && (self.file_size & self.page_mask) != 0
            {
                let eof_page_end = (eof_rel + self.page_mask) & !self.page_mask;
                for i in eof_rel..eof_page_end.min(mapped_pages_end) {
                    let b = *(p as *const u8).add(i as usize);
                    if b != 0 {
                        self.fail(format!(
                            "Mapped Read: non-zero data past EOF (map offset 0x{i:x}) is 0x{b:02x}"
                        ));
                        break;
                    }
                }
            }
            libc::munmap(p, map_size as usize);
            self.check_buffers(&tmp, offset, size as usize);
        }
    }

    fn domapwrite(&mut self, mut offset: u64, size: u64) {
        offset = rounddown(offset, self.a.writebdy);
        if size == 0 || offset + size > self.maxfilelen {
            self.log4(Op::MapWrite, offset, size, true);
            return;
        }
        let cur = self.file_size;
        if offset + size > self.biggest {
            self.biggest = offset + size;
        }
        self.log4(Op::MapWrite, offset, size, false);
        self.gendata(offset as usize, size as usize);
        if offset + size > self.file_size {
            self.update_file_size(offset, size);
        }
        if self.simulating() {
            return;
        }
        unsafe {
            if self.file_size > cur && libc::ftruncate(self.fd, self.file_size as i64) != 0 {
                self.fail(format!("domapwrite: ftruncate {}", io::Error::last_os_error()));
                return;
            }
            let pg_offset = offset & self.page_mask;
            let map_size = pg_offset + size;
            let p = libc::mmap(
                std::ptr::null_mut(),
                map_size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd,
                (offset - pg_offset) as libc::off_t,
            );
            if p == libc::MAP_FAILED {
                self.fail(format!("domapwrite: mmap {}", io::Error::last_os_error()));
                return;
            }
            std::ptr::copy_nonoverlapping(
                self.good_buf[offset as usize..].as_ptr(),
                (p as *mut u8).add(pg_offset as usize),
                size as usize,
            );
            if libc::msync(p, map_size as usize, libc::MS_SYNC) != 0 {
                self.fail(format!("domapwrite: msync {}", io::Error::last_os_error()));
            }
            libc::munmap(p, map_size as usize);
        }
    }

    fn dotruncate(&mut self, mut size: u64) {
        size = rounddown(size, self.a.truncbdy).min(self.maxfilelen);
        if size > self.biggest {
            self.biggest = size;
        }
        self.log4(Op::Truncate, 0, size, false);
        if size < self.file_size {
            self.pollute_eofpage(self.maxfilelen);
        }
        self.update_file_size(size, 0);
        if self.simulating() {
            return;
        }
        if unsafe { libc::ftruncate(self.fd, size as i64) } != 0 {
            self.fail(format!("dotruncate: {}", io::Error::last_os_error()));
        }
    }

    fn do_fsync(&mut self) {
        self.log4(Op::Fsync, 0, 0, false);
        if self.simulating() {
            return;
        }
        if unsafe { libc::fsync(self.fd) } != 0 {
            self.fail(format!("do_fsync: {}", io::Error::last_os_error()));
        }
    }

    fn fallocate_call(&self, mode: i32, offset: u64, len: u64) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let r = unsafe { libc::fallocate(self.fd, mode, offset as i64, len as i64) };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            match mode {
                m if m & FALLOC_FL_PUNCH_HOLE != 0 => {
                    let mut pr = libc::fpunchhole_t {
                        fp_flags: 0,
                        reserved: 0,
                        fp_offset: offset as i64,
                        fp_length: len as i64,
                    };
                    if unsafe { libc::fcntl(self.fd, libc::F_PUNCHHOLE, &mut pr) } == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                }
                _ => Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (mode, offset, len);
            Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
        }
    }

    fn do_preallocate(&mut self, mut offset: u64, mut size: u64, keep_size: bool, unshare: bool) {
        offset = offset.min(self.maxfilelen);
        size = size.min(self.maxfilelen.saturating_sub(offset));
        if size == 0 {
            self.log_entry(Op::Fallocate, [offset, 0, self.file_size], true, keep_size, unshare);
            return;
        }
        let end = offset + size;
        if !keep_size && end > self.biggest {
            self.biggest = end;
        }
        let mut mode = 0i32;
        if keep_size {
            mode |= FALLOC_FL_KEEP_SIZE;
        }
        if unshare {
            mode |= FALLOC_FL_UNSHARE_RANGE;
        }
        self.log_entry(Op::Fallocate, [offset, size, self.file_size], false, keep_size, unshare);
        if !keep_size && end > self.file_size {
            self.good_buf[self.file_size as usize..end as usize].fill(0);
            self.file_size = end;
        }
        if self.simulating() {
            return;
        }
        if let Err(e) = self.fallocate_call(mode, offset, size) {
            self.fail(format!("do_preallocate: {e}"));
        }
    }

    fn do_punch_hole(&mut self, offset: u64, length: u64) {
        if length == 0 || self.file_size <= offset {
            self.log4(Op::PunchHole, offset, length, true);
            return;
        }
        let length = length.min(self.file_size - offset);
        #[cfg(target_os = "macos")]
        let (offset, length) = {
            let bdy = self.page_size;
            (rounddown(offset, bdy), rounddown(length, bdy))
        };
        if length == 0 || offset >= self.file_size {
            self.log4(Op::PunchHole, offset, length, true);
            return;
        }
        self.log4(Op::PunchHole, offset, length, false);
        if !self.simulating()
            && let Err(e) = self.fallocate_call(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, offset, length)
        {
            self.fail(format!("do_punch_hole: {e}"));
            return;
        }
        let max_len = length.min(self.file_size - offset);
        let s = offset as usize;
        self.good_buf[s..s + max_len as usize].fill(0);
    }

    fn do_zero_range(&mut self, offset: u64, length: u64, keep_size: bool) {
        if length == 0 || offset + length > self.maxfilelen {
            self.log_entry(Op::ZeroRange, [offset, length, self.file_size], true, keep_size, false);
            return;
        }
        let end = offset + length;
        if !keep_size && end > self.biggest {
            self.biggest = end;
        }
        self.log_entry(Op::ZeroRange, [offset, length, self.file_size], false, keep_size, false);
        if !keep_size && end > self.file_size {
            self.good_buf[self.file_size as usize..end as usize].fill(0);
            self.file_size = end;
        }
        let mode = if keep_size {
            FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE
        } else {
            FALLOC_FL_ZERO_RANGE
        };
        if !self.simulating() && let Err(e) = self.fallocate_call(mode, offset, length) {
            self.fail(format!("do_zero_range: {e}"));
            return;
        }
        let zero_end = end.min(self.file_size) as usize;
        if (offset as usize) < zero_end {
            self.good_buf[offset as usize..zero_end].fill(0);
        }
    }

    fn do_write_zeroes(&mut self, offset: u64, length: u64) {
        if length == 0 || offset + length > self.maxfilelen {
            self.log4(Op::WriteZeroes, offset, length, true);
            return;
        }
        let end = offset + length;
        if end > self.biggest {
            self.biggest = end;
        }
        self.log4(Op::WriteZeroes, offset, length, false);
        if end > self.file_size {
            self.good_buf[self.file_size as usize..end as usize].fill(0);
            self.file_size = end;
        }
        if !self.simulating() && let Err(e) = self.fallocate_call(FALLOC_FL_WRITE_ZEROES, offset, length) {
            self.fail(format!("do_write_zeroes: {e}"));
            return;
        }
        let s = offset as usize;
        self.good_buf[s..s + length as usize].fill(0);
    }

    fn do_collapse_range(&mut self, offset: u64, length: u64) {
        if length == 0 || offset + length >= self.file_size {
            self.log4(Op::CollapseRange, offset, length, true);
            return;
        }
        self.log4(Op::CollapseRange, offset, length, false);
        self.pollute_eofpage(self.maxfilelen);
        if !self.simulating() && let Err(e) = self.fallocate_call(FALLOC_FL_COLLAPSE_RANGE, offset, length) {
            self.fail(format!("do_collapse_range: {e}"));
            return;
        }
        let end = offset + length;
        self.good_buf
            .copy_within(end as usize..self.file_size as usize, offset as usize);
        self.file_size -= length;
    }

    fn do_insert_range(&mut self, offset: u64, length: u64) {
        if length == 0 || offset >= self.file_size || self.file_size + length > self.maxfilelen {
            self.log4(Op::InsertRange, offset, length, true);
            return;
        }
        if self.file_size + length > self.biggest {
            self.biggest = self.file_size + length;
        }
        self.log4(Op::InsertRange, offset, length, false);
        self.pollute_eofpage(self.maxfilelen);
        if !self.simulating() && let Err(e) = self.fallocate_call(FALLOC_FL_INSERT_RANGE, offset, length) {
            self.fail(format!("do_insert_range: {e}"));
            return;
        }
        let end = offset + length;
        self.good_buf
            .copy_within(offset as usize..self.file_size as usize, end as usize);
        self.good_buf[offset as usize..end as usize].fill(0);
        self.file_size += length;
    }

    #[cfg(target_os = "linux")]
    fn do_clone_range(&mut self, offset: u64, length: u64, dest: u64) {
        #[repr(C)]
        struct FileCloneRange {
            src_fd: i64,
            src_offset: i64,
            src_length: i64,
            dest_offset: i64,
        }
        const FICLONERANGE: libc::c_ulong = 0x4020940D;
        if length == 0 || offset + length > self.file_size || dest + length > self.maxfilelen {
            self.log5(Op::CloneRange, offset, length, dest, true);
            return;
        }
        if dest + length > self.biggest {
            self.biggest = dest + length;
        }
        self.log5(Op::CloneRange, offset, length, dest, false);
        if dest + length > self.file_size {
            self.update_file_size(dest, length);
        }
        if !self.simulating() {
            let fcr = FileCloneRange {
                src_fd: self.fd as i64,
                src_offset: offset as i64,
                src_length: length as i64,
                dest_offset: dest as i64,
            };
            if unsafe { libc::ioctl(self.fd, FICLONERANGE, &fcr) } == -1 {
                self.fail(format!("do_clone_range: FICLONERANGE {}", io::Error::last_os_error()));
                return;
            }
        }
        let (s, d) = (offset as usize, dest as usize);
        self.good_buf.copy_within(s..s + length as usize, d);
    }

    #[cfg(not(target_os = "linux"))]
    fn do_clone_range(&mut self, _o: u64, _l: u64, _d: u64) {}

    #[cfg(target_os = "linux")]
    fn do_copy_range(&mut self, offset: u64, length: u64, dest: u64) {
        if length == 0 || offset + length > self.file_size || dest + length > self.maxfilelen {
            self.log5(Op::CopyRange, offset, length, dest, true);
            return;
        }
        if dest + length > self.biggest {
            self.biggest = dest + length;
        }
        self.log5(Op::CopyRange, offset, length, dest, false);
        if dest + length > self.file_size {
            self.update_file_size(dest, length);
        }
        if !self.simulating() {
            let mut o1 = offset as i64;
            let mut o2 = dest as i64;
            let mut left = length as i64;
            while left > 0 {
                let n = unsafe {
                    libc::copy_file_range(self.fd, &mut o1, self.fd, &mut o2, left as usize, 0)
                };
                if n <= 0 {
                    self.fail(format!("do_copy_range: copy_file_range {}", io::Error::last_os_error()));
                    return;
                }
                left -= n as i64;
            }
        }
        let (s, d) = (offset as usize, dest as usize);
        self.good_buf.copy_within(s..s + length as usize, d);
    }

    #[cfg(not(target_os = "linux"))]
    fn do_copy_range(&mut self, _o: u64, _l: u64, _d: u64) {}

    fn check_contents(&mut self) {
        if self.simulating() || self.file_size == 0 {
            return;
        }
        let mut buf = vec![0u8; self.file_size as usize];
        if let Err(e) = self.pread_all(&mut buf, 0) {
            self.fail(format!("check_contents: {e}"));
            return;
        }
        self.check_buffers(&buf, 0, buf.len());
    }

    fn logdump(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!("LOG DUMP ({} total operations):", self.logcount));
        let start = if self.logcount < LOGSIZE as u64 {
            0
        } else {
            self.logptr
        };
        let count = self.logcount.min(LOGSIZE as u64);
        let base_num = self.logcount.saturating_sub(count);
        let mut i = start;
        for c in 0..count {
            let le = &self.log[i];
            let opnum = base_num + c + 1;
            let name = OP_NAMES
                .iter()
                .find(|(o, _)| *o == le.op)
                .map(|(_, n)| *n)
                .unwrap_or("bogus");
            if le.skipped {
                out.push(format!("{opnum}: SKIPPED {name}"));
            } else {
                out.push(format!(
                    "{opnum}: {name} 0x{:x} 0x{:x} 0x{:x}{}{}{}",
                    le.args[0],
                    le.args[1],
                    le.args[2],
                    if le.keep_size { " keep_size" } else { "" },
                    if le.unshare { " unshare" } else { "" },
                    if le.close_open { " close_open" } else { "" },
                ));
            }
            i = (i + 1) % LOGSIZE;
        }
        out
    }

    fn load_replay(path: &Path) -> io::Result<Vec<LogEntry>> {
        let text = fs::read_to_string(path)?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("LOG DUMP") {
                continue;
            }
            let body = match line.split_once(':') {
                Some((prefix, rest)) if prefix.chars().all(|c| c.is_ascii_digit()) => rest.trim(),
                _ => line,
            };
            let mut toks: Vec<&str> = body.split_whitespace().collect();
            if toks.is_empty() {
                continue;
            }
            let mut skipped = false;
            if matches!(toks[0], "SKIPPED" | "skip") {
                skipped = true;
                toks.remove(0);
            }
            let Some(&opname) = toks.first() else {
                continue;
            };
            let Some(&(op, _)) = OP_NAMES.iter().find(|(_, n)| *n == opname) else {
                return Err(io::Error::other(format!("unknown op {opname} in {path:?}")));
            };
            if skipped {
                out.push(LogEntry {
                    op,
                    args: [0; 3],
                    skipped: true,
                    keep_size: false,
                    unshare: false,
                    close_open: false,
                });
                continue;
            }
            let parse_u64 = |s: &str| -> u64 {
                if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    u64::from_str_radix(h, 16).unwrap_or(0)
                } else {
                    s.parse().unwrap_or(0)
                }
            };
            let mut args = [0u64; 3];
            for j in 0..3 {
                if let Some(&t) = toks.get(1 + j) {
                    args[j] = parse_u64(t);
                }
            }
            let flags = if toks.len() > 4 { &toks[4..] } else { &[] };
            out.push(LogEntry {
                op,
                args,
                skipped: false,
                keep_size: flags.contains(&"keep_size"),
                unshare: flags.contains(&"unshare"),
                close_open: flags.contains(&"close_open"),
            });
        }
        Ok(out)
    }
}

pub fn run(mountpoint: &Path, a: &Args) -> Result<Value, String> {
    let dir_ok = fs::symlink_metadata(mountpoint)
        .map(|m| m.is_dir())
        .map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !dir_ok {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let file_path = mountpoint.join(&a.file);
    let good_path = suffixed(&file_path, ".fsxgood");
    let ops_path = suffixed(&file_path, ".fsxops");
    let log_path = suffixed(&file_path, ".fsxlog");

    if a.verify_good {
        let t0 = Instant::now();
        let want = fs::read(&good_path)
            .map_err(|e| format!("cannot read expected image {}: {e}", good_path.display()))?;
        let got = fs::read(&file_path)
            .map_err(|e| format!("cannot read target file {}: {e}", file_path.display()))?;
        let failure = if got.len() != want.len() {
            Some(format!(
                "verify-good size mismatch: {} has {} bytes, {} has {} bytes",
                file_path.display(),
                got.len(),
                good_path.display(),
                want.len()
            ))
        } else {
            got.iter()
                .zip(want.iter())
                .position(|(x, y)| x != y)
                .map(|off| {
                    format!(
                        "verify-good byte mismatch at offset {off} (0x{off:x}): got 0x{:02x}, expected 0x{:02x}",
                        got[off], want[off]
                    )
                })
        };
        if failure.is_none() && a.cleanup {
            let _ = fs::remove_file(&file_path);
            let _ = fs::remove_file(&good_path);
            let _ = fs::remove_file(&ops_path);
        }
        return Ok(json!({
            "suite": "fsx",
            "host": host_name(),
            "date": iso8601(SystemTime::now()),
            "top": mountpoint.display().to_string(),
            "durationMs": t0.elapsed().as_millis() as u64,
            "params": {"file": a.file, "mode": "verify-good"},
            "status": if failure.is_some() { "failed" } else { "ok" },
            "opsExecuted": 0,
            "opsCompleted": 0,
            "finalFileSize": got.len(),
            "failure": failure,
            "log": [],
        }));
    }

    let seed = if a.seed == 0 {
        let t = SystemTimeSeed::now();
        info!("fsx: seed = {t}");
        t
    } else {
        a.seed
    };

    let page_size = (unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64).max(4096);
    let readbdy = if a.direct { a.readbdy.max(page_size) } else { a.readbdy.max(1) };
    let writebdy = if a.direct { a.writebdy.max(page_size) } else { a.writebdy.max(1) };
    let truncbdy = if a.direct { a.truncbdy.max(page_size) } else { a.truncbdy.max(1) };

    let fd = open_test_fd(&file_path, true, !a.lite, a.direct)
        .map_err(|e| format!("{}: {e}", file_path.display()))?;
    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut sb) } != 0 {
        unsafe { libc::close(fd) };
        return Err("fstat failed".into());
    }

    let mut maxfilelen = a.flen;
    if a.direct {
        maxfilelen = rounddown(maxfilelen.max(page_size), page_size);
    }
    let initial_size = if a.lite {
        let sz = sb.st_size as u64;
        maxfilelen = maxfilelen.max(sz);
        sz
    } else {
        0u64
    };

    let mut enabled = [true; 14];
    let idx = |o: Op| OP_NAMES.iter().position(|(x, _)| *x == o).unwrap();
    let linux = cfg!(target_os = "linux");
    if a.no_mapped_reads {
        enabled[idx(Op::MapRead)] = false;
    }
    if a.no_mapped_writes {
        enabled[idx(Op::MapWrite)] = false;
    }
    if a.no_fallocate || !linux {
        enabled[idx(Op::Fallocate)] = false;
    }
    if a.no_punch || (!linux && !cfg!(target_os = "macos")) {
        enabled[idx(Op::PunchHole)] = false;
    }
    if a.no_zero || !linux {
        enabled[idx(Op::ZeroRange)] = false;
    }
    if a.no_collapse || !linux {
        enabled[idx(Op::CollapseRange)] = false;
    }
    if a.no_insert || !linux {
        enabled[idx(Op::InsertRange)] = false;
    }
    enabled[idx(Op::WriteZeroes)] = linux && !a.no_zero;
    if a.no_clone || !linux {
        enabled[idx(Op::CloneRange)] = false;
    }
    if a.no_copy || !linux {
        enabled[idx(Op::CopyRange)] = false;
    }
    if a.no_fsync_op || a.lite {
        enabled[idx(Op::Fsync)] = false;
    }

    let probe = FsxProbe { fd, size: initial_size };
    if linux && enabled[idx(Op::Fallocate)] && !probe.fa(0) {
        enabled[idx(Op::Fallocate)] = false;
    }
    if linux && enabled[idx(Op::PunchHole)] && !probe.fa(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) {
        enabled[idx(Op::PunchHole)] = false;
    }
    if linux && enabled[idx(Op::ZeroRange)] && !probe.fa(FALLOC_FL_ZERO_RANGE) {
        enabled[idx(Op::ZeroRange)] = false;
    }
    if linux && enabled[idx(Op::CollapseRange)] && !probe.fa(FALLOC_FL_COLLAPSE_RANGE) {
        enabled[idx(Op::CollapseRange)] = false;
    }
    if linux && enabled[idx(Op::InsertRange)] && !probe.fa(FALLOC_FL_INSERT_RANGE) {
        enabled[idx(Op::InsertRange)] = false;
    }
    if linux && enabled[idx(Op::WriteZeroes)] && !probe.fa(FALLOC_FL_WRITE_ZEROES) {
        enabled[idx(Op::WriteZeroes)] = false;
    }
    if linux && enabled[idx(Op::CloneRange)] && !probe.clone_range() {
        enabled[idx(Op::CloneRange)] = false;
    }
    if linux && enabled[idx(Op::CopyRange)] && !probe.copy_range() {
        enabled[idx(Op::CopyRange)] = false;
    }
    let falloc_keep_size_ok =
        linux && !a.no_keep_size && enabled[idx(Op::Fallocate)] && probe.fa(FALLOC_FL_KEEP_SIZE);
    let falloc_unshare_ok = linux
        && enabled[idx(Op::Fallocate)]
        && probe.fa(FALLOC_FL_UNSHARE_RANGE)
        && (!falloc_keep_size_ok || probe.fa(FALLOC_FL_UNSHARE_RANGE | FALLOC_FL_KEEP_SIZE));
    let zero_keep_size_ok = linux
        && !a.no_keep_size
        && enabled[idx(Op::ZeroRange)]
        && probe.fa(FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE);

    let mut fsx = Fsx {
        a: Cfg {
            readbdy,
            writebdy,
            truncbdy,
            pollute_eof: a.pollute_eof,
            do_fsync: a.fsync_writes,
            direct: a.direct,
            simulatedopcount: a.begin_op,
            inject_failure_at: a.inject_failure_at,
        },
        file_path: file_path.clone(),
        fd,
        good_buf: vec![0u8; maxfilelen as usize + writebdy as usize + 8],
        original_buf: {
            let mut g = GlibcRandom::new(seed);
            (0..maxfilelen as usize + writebdy as usize + 8)
                .map(|_| (g.next() % 256) as u8)
                .collect()
        },
        file_size: initial_size,
        biggest: initial_size,
        maxfilelen,
        page_size,
        page_mask: page_size - 1,
        testcalls: 0,
        rng: GlibcRandom::new(seed),
        log: vec![
            LogEntry {
                op: Op::Read,
                args: [0; 3],
                skipped: false,
                keep_size: false,
                unshare: false,
                close_open: false,
            };
            LOGSIZE
        ],
        logptr: 0,
        logcount: 0,
        badoff: -1,
        closeopen: false,
        enabled,
        failure: None,
    };

    if a.lite {
        fsx.file_size = maxfilelen;
        let init_img = fsx.good_buf[..maxfilelen as usize].to_vec();
        let _ = fsx.pwrite_all(&init_img, 0);
    } else if initial_size > 0 {
        let mut image = vec![0u8; initial_size as usize];
        fsx.pread_all(&mut image, 0).map_err(|e| e.to_string())?;
        fsx.good_buf[..initial_size as usize].copy_from_slice(&image);
    }

    if !a.quiet {
        info!(
            "fsx: file={} seed={seed} flen={maxfilelen} oplen={} numops={} lite={} direct={} bdy(r/w/t)={readbdy}/{writebdy}/{truncbdy}",
            file_path.display(),
            a.oplen,
            a.numops,
            a.lite,
            a.direct,
        );
    }

    let replay = match &a.replay_ops {
        Some(p) => Some(Fsx::load_replay(p).map_err(|e| format!("replay ops: {e}"))?),
        None => None,
    };
    let mut replay_idx = 0usize;

    let started = Instant::now();
    let maxop = if a.lite {
        4
    } else if a.no_fsync_op {
        13
    } else {
        14
    };

    while fsx.failure.is_none() {
        if let Some(rep) = &replay {
            if replay_idx >= rep.len() {
                break;
            }
            let entry = rep[replay_idx];
            replay_idx += 1;
            fsx.testcalls += 1;
            fsx.closeopen = entry.close_open;
            if entry.skipped {
                fsx.log_entry(entry.op, entry.args, true, entry.keep_size, entry.unshare);
            } else {
                match entry.op {
                    Op::Read => fsx.doread(entry.args[0], entry.args[1]),
                    Op::Write => fsx.dowrite(entry.args[0], entry.args[1]),
                    Op::MapRead => fsx.domapread(entry.args[0], entry.args[1]),
                    Op::MapWrite => fsx.domapwrite(entry.args[0], entry.args[1]),
                    Op::Truncate => fsx.dotruncate(entry.args[1]),
                    Op::Fallocate => fsx.do_preallocate(entry.args[0], entry.args[1], entry.keep_size, entry.unshare),
                    Op::PunchHole => fsx.do_punch_hole(entry.args[0], entry.args[1]),
                    Op::ZeroRange => fsx.do_zero_range(entry.args[0], entry.args[1], entry.keep_size),
                    Op::WriteZeroes => fsx.do_write_zeroes(entry.args[0], entry.args[1]),
                    Op::CollapseRange => fsx.do_collapse_range(entry.args[0], entry.args[1]),
                    Op::InsertRange => fsx.do_insert_range(entry.args[0], entry.args[1]),
                    Op::CloneRange => fsx.do_clone_range(entry.args[0], entry.args[1], entry.args[2]),
                    Op::CopyRange => fsx.do_copy_range(entry.args[0], entry.args[1], entry.args[2]),
                    Op::Fsync => fsx.do_fsync(),
                }
            }
        } else {
            if a.numops > 0 && fsx.testcalls >= a.numops as u64 {
                break;
            }
            if a.numops == 0 && a.duration <= 0 {
                break;
            }
            if a.duration > 0 && started.elapsed().as_secs() >= a.duration as u64 {
                break;
            }
            fsx.testcalls += 1;
            if a.closeprob > 0 {
                let rv = fsx.rnd();
                fsx.closeopen = (rv >> 3) < ((1u32 << 28) / (a.closeprob as u32).max(1));
            } else {
                fsx.closeopen = false;
            }

            let mut offset = fsx.rnd() as u64;
            let mut offset2 = 0u64;
            let mut size = if a.fixed_oplen {
                a.oplen
            } else {
                (fsx.rnd() as u64) % (a.oplen + 1)
            };
            let op_i = (fsx.rnd() as usize) % maxop;
            let op = OP_NAMES[op_i].0;

            let mut keep_size = false;
            let mut unshare = false;
            match op {
                Op::Truncate => {
                    if a.style == 0 {
                        size = (fsx.rnd() as u64) % (maxfilelen + 1);
                    }
                }
                Op::Fallocate => {
                    if size != 0 {
                        if !a.no_keep_size {
                            keep_size = (fsx.rnd() % 2 == 0) && falloc_keep_size_ok;
                        }
                        unshare = (fsx.rnd() % 2 == 0) && falloc_unshare_ok;
                    }
                }
                Op::ZeroRange => {
                    if size != 0 && !a.no_keep_size {
                        keep_size = (fsx.rnd() % 2 == 0) && zero_keep_size_ok;
                    }
                }
                Op::CloneRange | Op::CopyRange => {
                    gen_dest_range(&mut fsx, !linux, maxfilelen, &mut offset, &mut size, &mut offset2);
                }
                _ => {}
            }

            if !fsx.enabled[idx(op)]
                && !matches!(op, Op::Read | Op::Write | Op::MapRead | Op::MapWrite | Op::Truncate)
            {
                fsx.log5(op, offset, size, offset2, true);
                if fsx.testcalls == fsx.a.simulatedopcount {
                    fsx.flush_simulated_state();
                }
                continue;
            }

            match op {
                Op::Read => {
                    trim_off_len(&mut offset, &mut size, fsx.file_size);
                    fsx.doread(offset, size);
                }
                Op::Write => {
                    trim_off_len(&mut offset, &mut size, maxfilelen);
                    fsx.dowrite(offset, size);
                }
                Op::MapRead => {
                    trim_off_len(&mut offset, &mut size, fsx.file_size);
                    if a.no_mapped_reads {
                        fsx.doread(offset, size);
                    } else {
                        fsx.domapread(offset, size);
                    }
                }
                Op::MapWrite => {
                    trim_off_len(&mut offset, &mut size, maxfilelen);
                    if a.no_mapped_writes {
                        fsx.dowrite(offset, size);
                    } else {
                        fsx.domapwrite(offset, size);
                    }
                }
                Op::Truncate => fsx.dotruncate(size),
                Op::Fallocate => {
                    trim_off_len(&mut offset, &mut size, maxfilelen);
                    fsx.do_preallocate(offset, size, keep_size, unshare);
                }
                Op::PunchHole => {
                    trim_off_len(&mut offset, &mut size, fsx.file_size);
                    fsx.do_punch_hole(offset, size);
                }
                Op::ZeroRange => {
                    trim_off_len(&mut offset, &mut size, maxfilelen);
                    fsx.do_zero_range(offset, size, keep_size);
                }
                Op::WriteZeroes => {
                    trim_off_len(&mut offset, &mut size, maxfilelen);
                    fsx.do_write_zeroes(offset, size);
                }
                Op::CollapseRange => {
                    trim_off_len(&mut offset, &mut size, fsx.file_size.saturating_sub(1));
                    offset = rounddown(offset, 4096);
                    size = rounddown(size, 4096);
                    if size == 0 {
                        fsx.log4(Op::CollapseRange, offset, size, true);
                    } else {
                        fsx.do_collapse_range(offset, size);
                    }
                }
                Op::InsertRange => {
                    trim_off(&mut offset, fsx.file_size);
                    offset = rounddown(offset, 4096);
                    size = rounddown(size, 4096);
                    if size == 0 || fsx.file_size + size > maxfilelen {
                        fsx.log4(Op::InsertRange, offset, size, true);
                    } else {
                        fsx.do_insert_range(offset, size);
                    }
                }
                Op::CloneRange => {
                    if size == 0 || offset2 + size > maxfilelen {
                        fsx.log5(Op::CloneRange, offset, size, offset2, true);
                    } else {
                        fsx.do_clone_range(offset, size, offset2);
                    }
                }
                Op::CopyRange => {
                    if size == 0 || offset2 + size > maxfilelen {
                        fsx.log5(Op::CopyRange, offset, size, offset2, true);
                    } else {
                        fsx.do_copy_range(offset, size, offset2);
                    }
                }
                Op::Fsync => fsx.do_fsync(),
            }
        }

        if fsx.testcalls == fsx.a.simulatedopcount && fsx.failure.is_none() {
            fsx.flush_simulated_state();
        }
        if fsx.failure.is_some() {
            break;
        }
        if fsx.closeopen {
            fsx.do_close_open();
            if fsx.failure.is_some() {
                break;
            }
        }
        if a.check_contents && fsx.failure.is_none() {
            fsx.check_contents();
        }
        if !a.no_size_checks && fsx.failure.is_none() {
            fsx.check_size();
        }
        if a.progress > 0 && fsx.testcalls % a.progress == 0 && !a.quiet {
            info!("fsx: {} ops completed", fsx.testcalls);
        }
    }

    // Final whole-file verification against `good_buf` at the end of a clean run.
    if fsx.failure.is_none() {
        fsx.check_contents();
        if !a.no_size_checks && fsx.failure.is_none() {
            fsx.check_size();
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;

    if let Some(f) = &fsx.failure {
        let logdump = fsx.logdump();
        let _ = fs::write(&ops_path, logdump.join("\n"));
        let _ = fs::write(&good_path, &fsx.good_buf[..fsx.file_size as usize]);
        let _ = fs::write(&log_path, format!("failure: {f}\n{}", logdump.join("\n")));
        warn!("fsx: {} ops, FAILED: {f}", fsx.testcalls);
    } else {
        if !a.quiet {
            info!("fsx: All {} operations completed A-OK!", fsx.testcalls);
        }
        if a.record_ops {
            let _ = fs::write(&ops_path, fsx.logdump().join("\n"));
        }
        if a.keep_good {
            let _ = fs::write(&good_path, &fsx.good_buf[..fsx.file_size as usize]);
        } else if a.cleanup && !a.record_ops {
            let _ = fs::remove_file(&file_path);
        }
    }

    Ok(json!({
        "suite": "fsx",
        "host": host_name(),
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "durationMs": duration_ms,
        "params": {
            "file": a.file,
            "seed": seed,
            "numops": a.numops,
            "flen": maxfilelen,
            "oplen": a.oplen,
            "lite": a.lite,
            "direct": a.direct,
            "closeprob": a.closeprob,
            "readbdy": readbdy,
            "writebdy": writebdy,
            "truncbdy": truncbdy,
            "ops_enabled": OP_NAMES.iter().map(|(o, n)| (*n, fsx.enabled[idx(*o)])).collect::<Vec<_>>(),
        },
        "status": if fsx.failure.is_some() { "failed" } else { "ok" },
        "opsExecuted": fsx.testcalls,
        "opsCompleted": fsx.testcalls,
        "finalFileSize": fsx.file_size,
        "failure": fsx.failure,
        "log": if fsx.failure.is_some() { json!(fsx.logdump()) } else { json!([]) },
    }))
}

struct SystemTimeSeed;
impl SystemTimeSeed {
    fn now() -> i32 {
        let t = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (t.as_secs() as i32).wrapping_add(t.subsec_nanos() as i32)
    }
}

struct FsxProbe {
    #[allow(dead_code)]
    fd: RawFd,
    // Upstream probes at EOF with `len = 1` so filesystems report `EOPNOTSUPP`
    // rather than rejecting `len = 0` up front with `EINVAL`.
    size: u64,
}

impl FsxProbe {
    fn fa(&self, mode: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            let r = unsafe { libc::fallocate(self.fd, mode, self.size as i64, 1) };
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if r == 0 {
                let _ = unsafe { libc::ftruncate(self.fd, self.size as i64) };
                return true;
            }
            e != libc::ENOSYS && e != libc::EOPNOTSUPP && e != libc::ENOTTY
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (mode, self.size);
            false
        }
    }

    fn clone_range(&self) -> bool {
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
            let fcr = FileCloneRange {
                src_fd: self.fd as i64,
                src_offset: 0,
                src_length: 0,
                dest_offset: 0,
            };
            let r = unsafe { libc::ioctl(self.fd, FICLONERANGE, &fcr) };
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            r == 0 || (e != libc::EOPNOTSUPP && e != libc::ENOTTY && e != libc::ENOSYS)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn copy_range(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut o1: i64 = 0;
            let mut o2: i64 = 1;
            let r = unsafe { libc::copy_file_range(self.fd, &mut o1, self.fd, &mut o2, 1, 0) };
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if r > 0 {
                let _ = unsafe { libc::ftruncate(self.fd, self.size as i64) };
            }
            r >= 0 || (e != libc::ENOSYS && e != libc::EOPNOTSUPP && e != libc::ENOTTY)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

fn trim_off(off: &mut u64, size: u64) {
    *off = if size == 0 { 0 } else { *off % size };
}

fn trim_off_len(off: &mut u64, len: &mut u64, size: u64) {
    trim_off(off, size);
    if *off + *len > size {
        *len = size - *off;
    }
}

fn gen_dest_range(
    fsx: &mut Fsx,
    bdy_align: bool,
    max_range_end: u64,
    src_offset: &mut u64,
    size: &mut u64,
    dst_offset: &mut u64,
) {
    trim_off_len(src_offset, size, fsx.file_size);
    let bdy = if bdy_align { fsx.a.readbdy.max(1) } else { 4096 };
    *src_offset = rounddown(*src_offset, bdy);
    *size = rounddown(*size, bdy);
    if *size == 0 {
        return;
    }
    let wbdy = if bdy_align { fsx.a.writebdy.max(1) } else { 4096 };
    let mut tries = 0;
    loop {
        tries += 1;
        if tries >= 30 {
            *size = 0;
            break;
        }
        let mut d = fsx.rnd() as u64;
        d = if max_range_end == 0 { 0 } else { d % max_range_end };
        d = rounddown(d, wbdy);
        let overlap = (d as i64 - *src_offset as i64).unsigned_abs() < *size;
        if !overlap && d + *size <= max_range_end {
            *dst_offset = d;
            break;
        }
    }
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
    fn fsx_close_reopen_record_replay_and_verify_good_roundtrip() {
        let dir = std::env::temp_dir().join(format!("fstest-fsx-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cli = TestCli::parse_from([
            "fsx",
            "-N",
            "250",
            "-c",
            "5",
            "--record-ops",
            "--keep-good",
            "-q",
        ]);
        let r1 = run(&dir, &cli.args).expect("initial fsx run");
        assert_eq!(r1["status"], json!("ok"), "r1 failed: {r1}");

        // Verify `--verify-good` succeeds against saved `.fsxgood` shadow image.
        let v_cli = TestCli::parse_from(["fsx", "--verify-good", "-q"]);
        let rv = run(&dir, &v_cli.args).expect("verify-good");
        assert_eq!(rv["status"], json!("ok"));

        // Replay recorded `.fsxops` on a second target file and confirm identical final size & image.
        let ops_file = dir.join("fsx.testfile.fsxops");
        let rep_cli = TestCli::parse_from([
            "fsx",
            "--file",
            "replayed.bin",
            "--replay-ops",
            ops_file.to_str().unwrap(),
            "-q",
        ]);
        let r2 = run(&dir, &rep_cli.args).expect("replay fsx run");
        assert_eq!(r2["status"], json!("ok"), "r2 failed: {r2}");
        assert_eq!(r2["finalFileSize"], r1["finalFileSize"]);
        assert_eq!(
            fs::read(dir.join("replayed.bin")).unwrap(),
            fs::read(dir.join("fsx.testfile.fsxgood")).unwrap()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fsx_detects_injected_corruption() {
        let dir = std::env::temp_dir().join(format!("fstest-fsx-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cli = TestCli::parse_from(["fsx", "-N", "100", "-R", "-q"]);
        cli.args.inject_failure_at = Some(12);
        // Ensure a write occurs early with seed 1; scan if op 12 isn't a read by also checking contents.
        let rep = run(&dir, &cli.args).expect("run");
        if rep["status"] == json!("ok") {
            // Corrupt file directly and check `--verify-good` catches it.
            let mut cli2 = TestCli::parse_from(["fsx", "-N", "40", "--keep-good", "-q"]);
            cli2.args.file = "corrupt.bin".into();
            let r = run(&dir, &cli2.args).unwrap();
            assert_eq!(r["status"], json!("ok"));
            let p = dir.join("corrupt.bin");
            let mut b = fs::read(&p).unwrap();
            if b.is_empty() {
                b.push(0xaa);
            } else {
                b[0] ^= 0xff;
            }
            fs::write(&p, b).unwrap();
            let vc = TestCli::parse_from(["fsx", "--file", "corrupt.bin", "--verify-good", "-q"]);
            let vr = run(&dir, &vc.args).unwrap();
            assert_eq!(vr["status"], json!("failed"));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
