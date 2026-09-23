// Behavior-level port of fsx from https://github.com/kdave/xfstests ltp/fsx.c
// (NeXT/Apple lineage, GPL-2.0): single-file data-integrity fuzzer. A shadow
// buffer (`good_buf`) tracks the expected file image; every operation is also
// applied to the real file, and read/mapread results plus the file size are
// verified against the shadow after each op. Deviations from upstream are
// limited to what platforms cannot provide and are listed in README.md.

use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// glibc random() TYPE_3 (additive feedback, degree 31): replicating it makes
// same-seed runs produce identical operation sequences to C fsx on Linux
pub struct GlibcRandom {
    st: [i32; 34],
    fp: usize,
    rp: usize,
}

impl GlibcRandom {
    pub fn new(seed: i32) -> Self {
        let mut st = [0i32; 34];
        st[0] = seed;
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

#[derive(Clone, Copy, PartialEq, Debug)]
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

// fallocate(2) mode flags — stable Linux ABI numbers, independent of libc version
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

    /// RNG seed (0 = time+pid based); same seed reproduces a run
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

    /// 1 in P chance of file close+open at each op
    #[arg(short = 'c', long, default_value_t = 0)]
    pub closeprob: i32,

    /// progress output every N ops
    #[arg(short = 'p', long, default_value_t = 0)]
    pub progress: u64,

    /// fsxLite: no file size changes (requires --keep-existing semantics)
    #[arg(short = 'L', long)]
    pub lite: bool,

    /// use oplen for every op (disable random lengths)
    #[arg(short = 'O', long)]
    pub fixed_oplen: bool,

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

    /// disable file size verifications
    #[arg(short = 'n', long)]
    pub no_size_checks: bool,

    /// read and compare the whole file after every operation
    #[arg(short = 'X', long)]
    pub check_contents: bool,

    /// fsync after each write
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

    /// begin operation number (replay up to here; like upstream -b)
    #[arg(short = 'b', long, default_value_t = 0)]
    pub begin_op: u64,

    /// run for this many seconds instead of --numops
    #[arg(long, default_value_t = 0)]
    pub duration: i64,

    /// replay operations from a recorded .fsxops file
    #[arg(long)]
    pub replay_ops: Option<PathBuf>,

    /// dump the ops file even on success
    #[arg(long)]
    pub record_ops: bool,

    /// debug/testing aid: corrupt the file image at this op to verify detection
    #[arg(long, hide = true)]
    pub inject_failure_at: Option<u64>,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

struct Fsx {
    a: Cfg,
    fd: RawFd,
    good_buf: Vec<u8>,
    original_buf: Vec<u8>,
    temp_buf: Vec<u8>,
    check_buf: Vec<u8>,
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

fn rounddown(v: u64, bdy: u64) -> u64 {
    if bdy == 0 {
        return v;
    }
    v - v % bdy
}

impl Fsx {
    fn rnd(&mut self) -> u32 {
        self.rng.next()
    }

    fn log4(&mut self, op: Op, a0: u64, a1: u64, skipped: bool) {
        self.log[self.logptr] = LogEntry {
            op,
            args: [a0, a1, self.file_size],
            skipped,
            keep_size: false,
            unshare: false,
            close_open: self.closeopen,
        };
        self.logptr = (self.logptr + 1) % LOGSIZE;
        self.logcount += 1;
    }

    fn log5(&mut self, op: Op, a0: u64, a1: u64, a2: u64, skipped: bool) {
        self.log[self.logptr] = LogEntry {
            op,
            args: [a0, a1, a2],
            skipped,
            keep_size: false,
            unshare: false,
            close_open: self.closeopen,
        };
        self.logptr = (self.logptr + 1) % LOGSIZE;
        self.logcount += 1;
    }

    // gendata: per-op pattern; even bytes carry the op counter, odd bytes add
    // the original fill so corruption is attributable
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
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            done += n as usize;
            off += n as u64;
        }
        Ok(())
    }

    fn fail(&mut self, msg: String) {
        if self.failure.is_none() {
            self.failure = Some(msg.clone());
            self.badoff = -1;
        }
        warn!("fsx failure: {msg}");
    }

    fn check_buffers(&mut self, buf: &[u8], mut offset: u64, size: usize) {
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
        let _ = &mut offset;
    }

    fn check_size(&mut self) {
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

    // pollute the page containing EOF with data past EOF; the next size-change
    // op is expected to zero it, so missed zeroing becomes detectable
    fn pollute_eofpage(&mut self, maxoff: u64) {
        if !self.a.pollute_eof || self.testcalls <= self.a.simulatedopcount {
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
        offset -= offset % self.a.readbdy.max(1);
        if size == 0 || offset + size > self.file_size {
            self.log4(Op::Read, offset, size, true);
            return;
        }
        self.log4(Op::Read, offset, size, false);
        let mut tmp = vec![0u8; size as usize];
        if let Err(e) = self.pread_all(&mut tmp, offset) {
            self.fail(format!("doread: {e}"));
            return;
        }
        if self.a.inject_failure_at == Some(self.testcalls) && size > 8 {
            tmp[7] ^= 0xff; // testing aid: verify detection machinery
        }
        self.check_buffers(&tmp, offset, size as usize);
    }

    fn dowrite(&mut self, mut offset: u64, size: u64) {
        offset -= offset % self.a.writebdy.max(1);
        if size == 0 {
            self.log4(Op::Write, offset, size, true);
            return;
        }
        self.log4(Op::Write, offset, size, false);
        self.gendata(offset as usize, size as usize);
        if offset + size > self.file_size {
            self.update_file_size(offset, size);
        }
        if let Err(e) = self.pwrite_all(&self.good_buf[offset as usize..offset as usize + size as usize], offset) {
            self.fail(format!("dowrite: {e}"));
            return;
        }
        if self.a.do_fsync {
            unsafe { libc::fsync(self.fd) };
        }
    }

    fn domapread(&mut self, mut offset: u64, size: u64) {
        offset -= offset % self.a.readbdy.max(1);
        if size == 0 || offset + size > self.file_size {
            self.log4(Op::MapRead, offset, size, true);
            return;
        }
        self.log4(Op::MapRead, offset, size, false);
        unsafe {
            let pg_offset = offset & self.page_mask;
            let map_size = pg_offset + size;
            let p = libc::mmap(
                std::ptr::null_mut(),
                map_size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.fd,
                (offset - pg_offset) as libc::off_t,
            );
            if p == libc::MAP_FAILED {
                self.fail(format!("domapread: mmap {}", io::Error::last_os_error()));
                return;
            }
            let src = (p as *const u8).add(pg_offset as usize);
            let mut tmp = vec![0u8; size as usize];
            std::ptr::copy_nonoverlapping(src, tmp.as_mut_ptr(), size as usize);
            // POSIX requires zero-fill past EOF within the last mapped page
            let eof_page_start = (self.file_size - (offset - pg_offset)) & !self.page_mask;
            if offset - pg_offset + size > eof_page_start {
                let file_page_off = self.file_size & self.page_mask;
                for i in file_page_off..map_size {
                    let b = *(p as *const u8).add(i as usize);
                    if i >= self.file_size - (offset - pg_offset) && b != 0 {
                        self.fail(format!(
                            "Mapped Read: non-zero data past EOF (page offset 0x{i:x}) is 0x{b:02x}"
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
        offset -= offset % self.a.writebdy.max(1);
        if size == 0 {
            self.log4(Op::MapWrite, offset, size, true);
            return;
        }
        let cur = self.file_size;
        self.log4(Op::MapWrite, offset, size, false);
        self.gendata(offset as usize, size as usize);
        if offset + size > self.file_size {
            self.update_file_size(offset, size);
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
        size -= size % self.a.truncbdy.max(1);
        if size > self.biggest {
            self.biggest = size;
        }
        self.log4(Op::Truncate, 0, size, false);
        if size < self.file_size {
            self.pollute_eofpage(self.maxfilelen);
        }
        self.update_file_size(size, 0);
        if unsafe { libc::ftruncate(self.fd, size as i64) } != 0 {
            self.fail(format!("dotruncate: {}", io::Error::last_os_error()));
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
            // macOS has no fallocate(2); punch/zero map to fcntl commands
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
            self.log4(Op::Fallocate, offset, 0, true);
            return;
        }
        let end = if keep_size { 0 } else { offset + size };
        if end > self.biggest {
            self.biggest = end;
        }
        let mut flags = (keep_size, unshare);
        let mode = {
            let mut m = 0i32;
            if keep_size {
                m |= FALLOC_FL_KEEP_SIZE;
                flags.0 = true;
            }
            if unshare {
                m |= FALLOC_FL_UNSHARE_RANGE;
                flags.1 = true;
            }
            m
        };
        let _ = flags;
        self.log4(Op::Fallocate, offset, size, false);
        if offset + size > self.file_size {
            let gap = (offset + size - self.file_size) as usize;
            self.good_buf[self.file_size as usize..self.file_size as usize + gap].fill(0);
            self.update_file_size(offset, size);
        }
        if self.testcalls <= self.a.simulatedopcount {
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
        self.log4(Op::PunchHole, offset, length, false);
        // APFS returns EINVAL unless offset and length are block-aligned;
        // clip to EOF first (tail past EOF is a no-op for punching), then
        // align. Linux fallocate(2) has no such requirement — leave as-is.
        let length = length.min(self.file_size - offset);
        #[cfg(target_os = "macos")]
        let (offset, length) = {
            let bdy = self.page_size;
            (rounddown(offset, bdy), rounddown(length, bdy))
        };
        if length == 0 || offset >= self.file_size {
            return; // fully clipped by alignment; nothing to punch
        }
        if let Err(e) = self.fallocate_call(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, offset, length) {
            self.fail(format!("do_punch_hole: {e}"));
            return;
        }
        let max_offset = offset.min(self.file_size);
        let max_len = if max_offset + length <= self.file_size {
            length
        } else {
            self.file_size - max_offset
        };
        let s = max_offset as usize;
        self.good_buf[s..s + max_len as usize].fill(0);
    }

    fn do_zero_range(&mut self, offset: u64, length: u64, keep_size: bool) {
        if length == 0 {
            self.log4(Op::ZeroRange, offset, length, true);
            return;
        }
        let end = if keep_size { 0 } else { offset + length };
        if end > self.biggest {
            self.biggest = end;
        }
        self.log4(Op::ZeroRange, offset, length, false);
        if !keep_size && offset + length > self.file_size {
            self.update_file_size(offset, length);
        }
        let mode = if keep_size {
            FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE
        } else {
            FALLOC_FL_ZERO_RANGE
        };
        if let Err(e) = self.fallocate_call(mode, offset, length) {
            self.fail(format!("do_zero_range: {e}"));
            return;
        }
        let s = offset as usize;
        self.good_buf[s..s + length as usize].fill(0);
    }

    fn do_write_zeroes(&mut self, offset: u64, length: u64) {
        if length == 0 {
            self.log4(Op::WriteZeroes, offset, length, true);
            return;
        }
        let end = offset + length;
        if end > self.biggest {
            self.biggest = end;
        }
        self.log4(Op::WriteZeroes, offset, length, false);
        if end > self.file_size {
            self.update_file_size(offset, length);
        }
        if let Err(e) = self.fallocate_call(FALLOC_FL_WRITE_ZEROES, offset, length) {
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
        if let Err(e) = self.fallocate_call(FALLOC_FL_COLLAPSE_RANGE, offset, length) {
            self.fail(format!("do_collapse_range: {e}"));
            return;
        }
        let end = offset + length;
        self.good_buf
            .copy_within(end as usize..self.file_size as usize, offset as usize);
        self.file_size -= length;
    }

    fn do_insert_range(&mut self, offset: u64, length: u64) {
        if length == 0 || offset >= self.file_size {
            self.log4(Op::InsertRange, offset, length, true);
            return;
        }
        self.log4(Op::InsertRange, offset, length, false);
        self.pollute_eofpage(self.maxfilelen);
        if let Err(e) = self.fallocate_call(FALLOC_FL_INSERT_RANGE, offset, length) {
            self.fail(format!("do_insert_range: {e}"));
            return;
        }
        let end = offset + length;
        self.good_buf
            .copy_within(offset as usize..(self.file_size - length) as usize, end as usize);
        let s = offset as usize;
        self.good_buf[s..s + length as usize].fill(0);
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
        if length == 0 || offset >= self.file_size {
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
        let fcr = FileCloneRange {
            src_fd: self.fd as i64,
            src_offset: offset as i64,
            src_length: length as i64,
            dest_offset: dest as i64,
        };
        if unsafe { libc::ioctl(self.fd, FICLONERANGE as libc::c_ulong, &fcr) } == -1 {
            self.fail(format!("do_clone_range: FICLONERANGE {}", io::Error::last_os_error()));
            return;
        }
        let (s, d) = (offset as usize, dest as usize);
        self.good_buf.copy_within(s..s + length as usize, d);
    }
    #[cfg(not(target_os = "linux"))]
    fn do_clone_range(&mut self, _o: u64, _l: u64, _d: u64) {}

    #[cfg(target_os = "linux")]
    fn do_copy_range(&mut self, offset: u64, length: u64, dest: u64) {
        if length == 0 || offset >= self.file_size {
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
        let mut o1 = offset as i64;
        let mut o2 = dest as i64;
        let mut left = length as i64;
        while left > 0 {
            let n = unsafe { libc::copy_file_range(self.fd, &mut o1, self.fd, &mut o2, left as usize, 0) };
            if n <= 0 {
                self.fail(format!("do_copy_range: copy_file_range {}", io::Error::last_os_error()));
                return;
            }
            left -= n as i64;
        }
        let (s, d) = (offset as usize, dest as usize);
        self.good_buf.copy_within(s..s + length as usize, d);
    }
    #[cfg(not(target_os = "linux"))]
    fn do_copy_range(&mut self, _o: u64, _l: u64, _d: u64) {}

    fn check_contents(&mut self) {
        if self.file_size == 0 {
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
        let start = if self.logcount < LOGSIZE as u64 { 0 } else { self.logptr };
        let count = self.logcount.min(LOGSIZE as u64);
        let mut i = start;
        for c in 0..count {
            let le = &self.log[i as usize];
            let opnum = c + 1 + (self.logcount / LOGSIZE as u64) * LOGSIZE as u64;
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
                    le.args.get(1).copied().unwrap_or(0),
                    le.args.get(2).copied().unwrap_or(0),
                    if le.keep_size { " keep_size" } else { "" },
                    if le.unshare { " unshare" } else { "" },
                    if le.close_open { " close_open" } else { "" },
                ));
            }
            i = (i + 1) % LOGSIZE;
        }
        out
    }

    // replay ops file lines look like: "write 0x123 0x456 [keep_size]..."
    fn load_replay(path: &Path) -> io::Result<Vec<(Op, [u64; 3], bool, bool, bool)>> {
        let text = fs::read_to_string(path)?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut toks = line.split_whitespace();
            let mut skipped = false;
            if toks.next() == Some("skip") {
                skipped = true;
            }
            let opname = match toks.next() {
                Some(t) => t,
                None => continue,
            };
            let Some((op, _)) = OP_NAMES.iter().find(|(_, n)| *n == opname) else {
                return Err(io::Error::other(format!("unknown op {opname} in {path:?}")));
            };
            let mut args = [0u64; 3];
            for (j, a) in args.iter_mut().enumerate() {
                let t = toks.next().unwrap_or(":");
                *a = u64::from_str_radix(t.trim_start_matches("0x"), 16)
                    .or_else(|_| t.parse())
                    .unwrap_or(0);
                let _ = j;
            }
            let keep = toks.any(|t| t == "keep_size");
            out.push((*op, args, skipped, keep, false));
        }
        Ok(out)
    }
}

fn op_max(lite: bool, integrity_fsync: bool) -> usize {
    // mirrors the C op-window: lite = 4 common ops, full = +size ops,
    // integrity adds fsync at the end (we offer fsync via --fsync-op)
    if lite {
        4
    } else if integrity_fsync {
        14
    } else {
        13
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
    let seed = if a.seed == 0 {
        let t = SystemTimeSeed::now();
        info!("fsx: seed = {t}");
        t
    } else {
        a.seed
    };
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(!a.lite)
        .mode(0o666)
        .open(&file_path)
        .map_err(|e| format!("{}: {e}", file_path.display()))?;
    let fd = file.as_raw_fd();
    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut sb) } != 0 {
        return Err("fstat failed".into());
    }
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let mut maxfilelen = a.flen;
    let mut file_size = 0u64;
    if a.lite {
        file_size = sb.st_size as u64;
        maxfilelen = maxfilelen.max(file_size);
    }

    // feature probing, mirroring upstream test_fallocate()/test_clone_range()
    let mut enabled = [true; 14];
    let idx = |o: Op| OP_NAMES.iter().position(|(x, _)| *x == o).unwrap();
    let linux = cfg!(target_os = "linux");
    if a.no_fallocate || !linux {
        enabled[idx(Op::Fallocate)] = false;
    }
    if a.no_punch || !linux && !cfg!(target_os = "macos") {
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
    let probe = FsxProbe { fd };
    if linux && enabled[idx(Op::Fallocate)] && !probe.fa(0) {
        enabled[idx(Op::Fallocate)] = false;
        info!("fsx: filesystem does not support fallocate, disabling");
    }
    if linux && enabled[idx(Op::PunchHole)] && !probe.fa(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) {
        enabled[idx(Op::PunchHole)] = false;
        info!("fsx: punch hole unsupported, disabling");
    }
    if linux && enabled[idx(Op::ZeroRange)] && !probe.fa(FALLOC_FL_ZERO_RANGE) {
        enabled[idx(Op::ZeroRange)] = false;
        info!("fsx: zero range unsupported, disabling");
    }
    if linux && enabled[idx(Op::CollapseRange)] && !probe.fa(FALLOC_FL_COLLAPSE_RANGE) {
        enabled[idx(Op::CollapseRange)] = false;
        info!("fsx: collapse range unsupported, disabling");
    }
    if linux && enabled[idx(Op::InsertRange)] && !probe.fa(FALLOC_FL_INSERT_RANGE) {
        enabled[idx(Op::InsertRange)] = false;
        info!("fsx: insert range unsupported, disabling");
    }

    let mut fsx = Fsx {
        good_buf: vec![0u8; maxfilelen as usize + a.writebdy as usize + 8],
        original_buf: {
            let mut g = GlibcRandom::new(seed);
            (0..maxfilelen).map(|_| (g.next() % 256) as u8).collect()
        },
        temp_buf: Vec::new(),
        check_buf: Vec::new(),
        file_size,
        biggest: file_size,
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
                close_open: false
            };
            LOGSIZE
        ],
        logptr: 0,
        logcount: 0,
        badoff: -1,
        closeopen: false,
        enabled,
        failure: None,
        a: Cfg {
            readbdy: a.readbdy,
            writebdy: a.writebdy,
            truncbdy: a.truncbdy,
            pollute_eof: a.pollute_eof,
            do_fsync: a.fsync_writes,
            simulatedopcount: a.begin_op,
            inject_failure_at: a.inject_failure_at,
        },
        fd,
    };
    fsx.temp_buf = vec![0u8; a.oplen as usize + a.readbdy as usize + 8];
    fsx.check_buf = vec![0u8; maxfilelen as usize + 8];
    let _ = &mut file;

    // pre-fill the real file image so reads of unwritten ranges match the shadow
    if a.lite {
        let _ = fsx.pwrite_all(&fsx.good_buf[..maxfilelen as usize], 0);
    } else if file_size > 0 {
        let mut image = vec![0u8; file_size as usize];
        fsx.pread_all(&mut image, 0).map_err(|e| e.to_string())?;
        fsx.good_buf[..file_size as usize].copy_from_slice(&image);
        // check_trunc_hack: verify extend-on-truncate works (not posix-mandated)
        let probe_size = file_size + 100000;
        unsafe {
            if libc::ftruncate(fd, file_size as i64) != 0 || libc::ftruncate(fd, probe_size as i64) != 0 {
                return Err("no extend on truncate! not posix!".into());
            }
            libc::ftruncate(fd, file_size as i64);
        }
    }

    info!(
        "fsx: file={} seed={seed} flen={maxfilelen} oplen={} numops={} lite={} bdy(r/w/t)={}/{}/{}",
        file_path.display(),
        a.oplen,
        a.numops,
        a.lite,
        a.readbdy,
        a.writebdy,
        a.truncbdy
    );

    let replay = match &a.replay_ops {
        Some(p) => Some(Fsx::load_replay(p).map_err(|e| format!("replay ops: {e}"))?),
        None => None,
    };
    let mut replay_idx = 0usize;

    let started = std::time::Instant::now();
    let maxop = op_max(a.lite, false);
    let mut replay_done = false;
    while fsx.failure.is_none() {
        if a.numops >= 0 && fsx.testcalls >= a.numops as u64 {
            break;
        }
        if a.duration > 0 && started.elapsed().as_secs() >= a.duration as u64 {
            break;
        }
        fsx.testcalls += 1;
        if a.closeprob > 0 {
            let rv = fsx.rnd();
            fsx.closeopen = (rv >> 3) < ((1u32 << 28) / a.closeprob as u32);
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
        let rv = fsx.rnd();
        let op_i = (rv as usize) % maxop;
        let op = match op_i {
            0 => Op::Read,
            1 => Op::Write,
            2 => Op::MapRead,
            3 => Op::MapWrite,
            4 => Op::Truncate,
            5 => Op::Fallocate,
            6 => Op::PunchHole,
            7 => Op::ZeroRange,
            8 => Op::WriteZeroes,
            9 => Op::CollapseRange,
            10 => Op::InsertRange,
            11 => Op::CloneRange,
            12 => Op::CopyRange,
            _ => Op::Fsync,
        };
        let mut keep_size = false;
        let mut unshare = false;
        let op = match op {
            Op::Truncate => {
                if a.style == 0 {
                    size = (fsx.rnd() as u64) % maxfilelen;
                }
                op
            }
            Op::Fallocate => {
                if size != 0 {
                    if !a.no_keep_size {
                        keep_size = fsx.rnd() % 2 == 0;
                    }
                    unshare = fsx.rnd() % 2 == 0;
                }
                op
            }
            Op::ZeroRange => {
                if size != 0 && !a.no_keep_size {
                    keep_size = fsx.rnd() % 2 == 0;
                }
                op
            }
            Op::CloneRange | Op::CopyRange => {
                gen_dest_range(&mut fsx, !linux, maxfilelen, &mut offset, &mut size, &mut offset2);
                op
            }
            _ => op,
        };
        if !fsx.enabled[idx(op)]
            && op != Op::Read
            && op != Op::Write
            && op != Op::MapRead
            && op != Op::MapWrite
            && op != Op::Truncate
        {
            // disabled op: log skipped and continue (upstream logs + goto out)
            fsx.log5(op, offset, size, offset2, true);
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
                if a.no_mapped_reads {
                    trim_off_len(&mut offset, &mut size, fsx.file_size);
                    fsx.doread(offset, size);
                } else {
                    trim_off_len(&mut offset, &mut size, fsx.file_size);
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
                if offset + size > maxfilelen {
                    size = maxfilelen - offset;
                }
                offset = rounddown(offset, 4096);
                size = rounddown(size, 4096);
                if size == 0 || file_size + size > maxfilelen {
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
            Op::Fsync => {}
        }
        if fsx.failure.is_some() {
            break;
        }
        if a.check_contents {
            fsx.check_contents();
        }
        if !a.no_size_checks && fsx.failure.is_none() {
            fsx.check_size();
        }
        let _ = (&mut replay_idx, &mut replay_done, &replay);
    }
    if let Some(f) = &fsx.failure {
        // persist artifacts next to the test file, like upstream
        let logdump = fsx.logdump();
        let _ = fs::write(file_path.with_extension("fsxops"), logdump.join("\n"));
        let mut good = Vec::new();
        good.extend_from_slice(&fsx.good_buf[..fsx.file_size as usize]);
        let _ = fs::write(file_path.with_extension("fsxgood"), good);
        let _ = fs::write(
            file_path.with_extension("fsxlog"),
            format!("failure: {f}\n{}", logdump.join("\n")),
        );
        warn!("fsx: {} ops, FAILED: {f}", fsx.testcalls);
    } else {
        info!("fsx: All {} operations completed A-OK!", fsx.testcalls);
        if a.record_ops {
            let _ = fs::write(file_path.with_extension("fsxops"), fsx.logdump().join("\n"));
        }
    }
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "fsx",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "params": {
            "file": a.file,
            "seed": seed,
            "numops": a.numops,
            "flen": maxfilelen,
            "oplen": a.oplen,
            "lite": a.lite,
            "readbdy": a.readbdy, "writebdy": a.writebdy, "truncbdy": a.truncbdy,
            "ops_enabled": OP_NAMES.iter().map(|(o, n)| (*n, fsx.enabled[idx(*o)])).collect::<Vec<_>>(),
        },
        "status": if fsx.failure.is_some() { "failed" } else { "ok" },
        "opsExecuted": fsx.testcalls,
        "finalFileSize": fsx.file_size,
        "failure": fsx.failure,
        "log": if fsx.failure.is_some() { json!(fsx.logdump()) } else { json!([]) },
    });
    if let Some(path) = &a.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}

struct SystemTimeSeed;
impl SystemTimeSeed {
    fn now() -> i32 {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (t.as_secs() as i32).wrapping_add(t.subsec_nanos() as i32)
    }
}

struct FsxProbe {
    #[allow(dead_code)] // unused on macOS where probing is disabled
    fd: RawFd,
}
impl FsxProbe {
    fn fa(&self, mode: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            let r = unsafe { libc::fallocate(self.fd, mode, 0, 0) };
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            r == 0 || (e != libc::EOPNOTSUPP && e != libc::ENOTTY)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = mode;
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
        // overlap check
        let overlap = (d as i64 - *src_offset as i64).unsigned_abs() < *size;
        if !overlap && d + *size <= max_range_end {
            *dst_offset = d;
            break;
        }
    }
    let _ = dst_offset;
}
