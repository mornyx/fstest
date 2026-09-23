// Port of https://github.com/distributed-system-analysis/smallfile (Apache-2.0).
// Semantics (dir naming, buffer layout, stonewall, per-op request accounting,
// result aggregation) intentionally mirror the Python implementation.

use log::{debug, error, info, warn};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::ParseIntError;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const OP_NAMES: &[&str] = &[
    "create",
    "read",
    "append",
    "overwrite",
    "truncate-overwrite",
    "rename",
    "delete",
    "delete-renamed",
    "cleanup",
    "stat",
    "chmod",
    "mkdir",
    "rmdir",
    "symlink",
    "readdir",
    "ls-l",
    "setxattr",
    "getxattr",
    "swift-put",
    "swift-get",
    "await-create",
];

const DEFAULT_PHASES: &[&str] = &[
    "cleanup",
    "create",
    "read",
    "append",
    "rename",
    "delete-renamed",
    "cleanup",
];

const OK: i32 = 0;
const EIO: i32 = 5;
const BIGGEST_BUF_SIZE: usize = 1 << 20;
const BUF_OFFSET_RANGE: usize = 1 << 10;
const RENAME_SUFFIX: &str = ".rnm";
const KB_PER_GB: f64 = (1 << 20) as f64;
const KIB_PER_MIB: f64 = 1024.0;
const PCT_FILES_MIN: f64 = 70.0;
const RANDOM_SIZE_LIMIT: u32 = 8;
const SOME_PRIME: u64 = 900593;
const PAUSE_RSTIME_COUNT: usize = 100;
const FILES_BETWEEN_PAUSE: usize = 5;
const PAUSE_HIST_DURATION: f64 = 1.0;
const CTIME_XATTR: &str = "user.smallfile-ctime-size";
const XATTR_SUFFIX: &str = "user.smallfile-";
const SWIFT_XATTR_PREFIX: &str = "user.smallfile-all-";

#[derive(clap::Args)]
pub struct Args {
    /// run only this operation instead of the default phase sequence
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(OP_NAMES.iter().copied()))]
    pub operation: Option<String>,

    /// files processed per thread
    #[arg(long, default_value_t = 200, value_parser = positive_u32)]
    pub files: u32,

    /// threads per client
    #[arg(long, default_value_t = 2, value_parser = positive_u32)]
    pub threads: u32,

    /// record size (KB), 0 = use file size
    #[arg(long, default_value_t = 0)]
    pub record_size: u32,

    /// file size (KB)
    #[arg(long, default_value_t = 64)]
    pub file_size: u32,

    /// files per (sub)directory
    #[arg(long, default_value_t = 100, value_parser = positive_u32)]
    pub files_per_dir: u32,

    /// subdirectories per directory
    #[arg(long, default_value_t = 10, value_parser = positive_u32)]
    pub dirs_per_dir: u32,

    /// stop measuring as soon as first thread is done (Y/N)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub stonewall: bool,

    /// finish remaining files after first thread is done (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub finish: bool,

    /// verify read data against what was written (Y/N)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub verify_read: bool,

    /// call fsync() after each file is written (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub fsync: bool,

    /// filename prefix
    #[arg(long, default_value = "")]
    pub prefix: String,

    /// filename suffix
    #[arg(long, default_value = "")]
    pub suffix: String,

    /// file size distribution: fixed or exponential
    #[arg(long, default_value = "fixed", value_parser = distr_parser)]
    pub file_size_distribution: String,

    /// all threads share the same directory (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub same_dir: bool,

    /// hash file number into directory names (Y/N), disables readdir/ls-l
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub hash_into_dirs: bool,

    /// pause between each file (microsec)
    #[arg(long, default_value_t = 0)]
    pub pause: u64,

    /// adjust pause between files automatically from response times (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub auto_pause: bool,

    /// record response time of each file op to CSV (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub response_times: bool,

    /// extended attribute value size (bytes)
    #[arg(long, default_value_t = 0)]
    pub xattr_size: usize,

    /// number of extended attributes per file
    #[arg(long, default_value_t = 0)]
    pub xattr_count: usize,

    /// record create time + size as xattr for await-create (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub record_ctime_size: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,

    /// verbose logging
    #[arg(short, long)]
    pub verbose: bool,
}

fn positive_u32(s: &str) -> Result<u32, String> {
    let v: u32 = s.parse().map_err(|e: ParseIntError| e.to_string())?;
    if v == 0 {
        Err("must be a positive integer".into())
    } else {
        Ok(v)
    }
}

// smallfile_cli accepts Y/N for boolean options, mirror that
pub(crate) fn yn_bool(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" => Ok(true),
        "n" | "no" | "false" => Ok(false),
        _ => Err("expected Y or N".into()),
    }
}

fn distr_parser(s: &str) -> Result<String, String> {
    match s {
        "fixed" | "exponential" => Ok(s.to_string()),
        _ => Err("file size distribution must be \"fixed\" or \"exponential\"".into()),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Distr {
    Fixed,
    Exponential,
}

#[derive(Clone)]
struct Params {
    opname: String,
    iterations: u32,
    threads: u32,
    record_sz_kb: u32,
    total_sz_kb: u32,
    filesize_distr: Distr,
    files_per_dir: u32,
    dirs_per_dir: u32,
    prefix: String,
    suffix: String,
    stonewall: bool,
    finish_all_rq: bool,
    verify_read: bool,
    fsync: bool,
    hash_to_dir: bool,
    is_shared_dir: bool,
    pause_between_files_us: u64,
    auto_pause: bool,
    measure_rsptimes: bool,
    xattr_size: usize,
    xattr_count: usize,
    record_ctime_size: bool,
    top: PathBuf,
    host: String,
}

impl Params {
    fn src_base(&self) -> PathBuf {
        self.top.join("file_srcdir").join(&self.host)
    }
    fn dest_base(&self) -> PathBuf {
        self.top.join("file_dstdir").join(&self.host)
    }
    fn src_dirs(&self, tid: &str) -> Vec<PathBuf> {
        // shared dir keeps all threads (and hosts) in one tree; filenames carry tid/host
        if self.is_shared_dir {
            vec![self.top.join("file_srcdir")]
        } else {
            vec![self.src_base().join(format!("thrd_{tid}"))]
        }
    }
    fn dest_dirs(&self, tid: &str) -> Vec<PathBuf> {
        if self.is_shared_dir {
            vec![self.top.join("file_dstdir")]
        } else {
            vec![self.dest_base().join(format!("thrd_{tid}"))]
        }
    }
    fn record_size_to_use(&self) -> u32 {
        let rszkb = if self.record_sz_kb == 0 {
            self.total_sz_kb
        } else {
            self.record_sz_kb
        };
        rszkb.min((BIGGEST_BUF_SIZE / 1024) as u32)
    }
    fn files_between_checks(&self) -> u32 {
        if self.total_sz_kb > 0 {
            (100 - self.total_sz_kb / 100).max(10)
        } else {
            20
        }
    }
    // file numbers whose directory names must be created/removed up front; with
    // hashing every file number can land in a different directory, so no stepping
    fn subdir_file_nums(&self) -> Vec<usize> {
        if self.hash_to_dir {
            (0..=self.iterations as usize).collect()
        } else {
            let (it, fpd) = (self.iterations as usize, self.files_per_dir as usize);
            (0..it + fpd).step_by(fpd).collect()
        }
    }
    fn mk_dir_name(&self, file_num: u32) -> String {
        if self.hash_to_dir {
            self.mk_hashed_dir_name(file_num)
        } else {
            self.mk_seq_dir_name(file_num)
        }
    }
    // subdirectories are named like radix-`dirs_per_dir` numbers: d_000, d_001, ... d_000/d_003, ...
    fn mk_seq_dir_name(&self, file_num: u32) -> String {
        let mut dir_in = file_num / self.files_per_dir;
        let mut level_dirs = Vec::new();
        let mut dirs_for_this_level = self.dirs_per_dir;
        while dirs_for_this_level <= dir_in {
            level_dirs.push(dirs_for_this_level);
            dirs_for_this_level *= self.dirs_per_dir;
        }
        let mut parts = Vec::with_capacity(level_dirs.len() + 1);
        for &lvl in level_dirs.iter().rev() {
            let quotient = dir_in / lvl;
            dir_in -= quotient * lvl;
            parts.push(format!("d_{quotient:03}"));
        }
        parts.push(format!("d_{dir_in:03}"));
        parts.join("/")
    }
    fn mk_hashed_dir_name(&self, file_num: u32) -> String {
        let mut pathlist = Vec::new();
        let mut dir_num =
            ((file_num as u64 * SOME_PRIME) % self.iterations as u64 / self.files_per_dir as u64) as usize;
        while dir_num > 1 {
            let dir_num_hash = dir_num as u64 * SOME_PRIME % self.dirs_per_dir as u64;
            pathlist.insert(0, format!("h_{dir_num_hash:03}"));
            dir_num /= self.dirs_per_dir as usize;
        }
        pathlist.join("/")
    }
}

// biggest_buf is deterministic (k%128 pattern, 0x5c replaced by '!') and shared by all
// threads. The 1 KiB tail exists so each file can start at a different offset
// ((tid+1)*filenum % 1024) while still being a contiguous slice; read verification
// depends on this exact layout, so it must never diverge between create and read.
fn build_biggest_buf() -> Vec<u8> {
    let first: Vec<u8> = (0..BUF_OFFSET_RANGE as u32)
        .map(|k| {
            let b = (k % 128) as u8;
            if b == b'\\' { b'!' } else { b }
        })
        .collect();
    let mut buf = first.clone();
    for _ in 0..10 {
        let double = buf.clone();
        buf.extend_from_slice(&double);
    }
    buf.extend_from_slice(&first);
    debug_assert_eq!(buf.len(), BIGGEST_BUF_SIZE + BUF_OFFSET_RANGE);
    buf
}

fn suffixed(p: &Path, s: &str) -> PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push(s);
    os.into()
}

fn ensure_deleted(p: &Path) -> io::Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// mode-0 fallocate: linux has posix_fallocate, macOS only offers fcntl(F_PREALLOCATE)
#[cfg(target_os = "linux")]
pub(crate) fn preallocate(fd: i32, len: i64) -> io::Result<()> {
    let rc = unsafe { libc::posix_fallocate(fd, 0, len) };
    if rc != 0 {
        Err(io::Error::from_raw_os_error(rc))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn preallocate(fd: i32, len: i64) -> io::Result<()> {
    let mut st = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: len,
        fst_bytesalloc: 0,
    };
    if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut st) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn preallocate(_fd: i32, _len: i64) -> io::Result<()> {
    Ok(())
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(EIO)
}

// pitfall: std's File::sync_all() maps to fcntl(F_FULLFSYNC) on Apple targets,
// which flushes all the way to stable storage and is orders of magnitude heavier
// than the plain fsync(2) the upstream tools (smallfile, fs_mark) issue
pub(crate) fn fsync_raw(f: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::fsync(f.as_raw_fd()) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn status_str(status: i32) -> String {
    if status == OK {
        "ok".to_string()
    } else {
        io::Error::from_raw_os_error(status).to_string()
    }
}

pub(crate) fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

struct Workload {
    p: Params,
    tid: usize,
    tid_label: String,
    src_dirs: Vec<PathBuf>,
    dest_dirs: Vec<PathBuf>,
    file_dirs: Vec<String>,
    biggest_buf: Arc<Vec<u8>>,
    scratch: Vec<u8>,
    filenum: usize,
    rq: u64,
    ended: bool,
    start: Option<Instant>,
    end: Option<Instant>,
    filenum_final: Option<usize>,
    rq_final: Option<u64>,
    status: i32,
    // exponential file-size distribution: deterministic per (top, host, tid) seed, so
    // separate invocations (create then read) regenerate identical size sequences
    // without smallfile's seed sidecar file
    rng_state: u64,
    pause_sec: f64,
    pause_hist: [f64; PAUSE_RSTIME_COUNT],
    pause_idx: Option<usize>,
    pause_samples: usize,
    pause_hist_start: f64,
    throttle: f64,
    rsptimes: Vec<(String, f64, f64)>,
    start_wall: Option<SystemTime>,
}

impl Workload {
    fn new(p: Params, tid: usize, biggest_buf: Arc<Vec<u8>>) -> Self {
        let tid_label = format!("{tid:02}");
        let file_dirs = (0..p.iterations + p.files_per_dir).map(|j| p.mk_dir_name(j)).collect();
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        p.top.hash(&mut h);
        p.host.hash(&mut h);
        tid.hash(&mut h);
        Workload {
            src_dirs: p.src_dirs(&tid_label),
            dest_dirs: p.dest_dirs(&tid_label),
            rng_state: h.finish(),
            pause_sec: p.pause_between_files_us as f64 / 1e6,
            pause_hist: [0.0; PAUSE_RSTIME_COUNT],
            pause_idx: None,
            pause_samples: 0,
            pause_hist_start: 0.0,
            throttle: 0.1 * (p.threads as f64 + 1.0).log2(),
            rsptimes: Vec::new(),
            start_wall: None,
            p,
            tid,
            tid_label,
            file_dirs,
            scratch: vec![0u8; BIGGEST_BUF_SIZE],
            biggest_buf,
            filenum: 0,
            rq: 0,
            ended: false,
            start: None,
            end: None,
            filenum_final: None,
            rq_final: None,
            status: OK,
        }
    }

    fn mk_file_nm(&self, base_dirs: &[PathBuf], filenum: usize) -> PathBuf {
        let tree = &base_dirs[filenum % base_dirs.len()];
        tree.join(&self.file_dirs[filenum]).join(format!(
            "{}_{}_{}_{}_{}",
            self.p.prefix, self.p.host, self.tid_label, filenum, self.p.suffix
        ))
    }

    fn this_file_nm(&self, base_dirs: &[PathBuf]) -> PathBuf {
        self.mk_file_nm(base_dirs, self.filenum)
    }

    fn unique_offset(&self) -> usize {
        ((self.tid + 1) * self.filenum) % BUF_OFFSET_RANGE
    }

    fn next_rand_f64(&mut self) -> f64 {
        self.rng_state = self.rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }

    // equivalent of python random.Random.expovariate(1.0/mean) with size clamping
    fn get_next_file_size_kb(&mut self) -> u32 {
        if self.p.filesize_distr == Distr::Fixed {
            return self.p.total_sz_kb;
        }
        let mean = self.p.total_sz_kb as f64;
        let v = -((1.0 - self.next_rand_f64()).ln()) * mean;
        (v as u32)
            .max(1)
            .min(self.p.total_sz_kb.saturating_mul(RANDOM_SIZE_LIMIT))
    }

    // seconds since this thread's measured interval began
    fn now(&self) -> f64 {
        self.start.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0)
    }

    fn record_rsp(&mut self, opname: &str, rel_start: f64, rsp: f64) {
        if self.p.measure_rsptimes {
            self.rsptimes.push((opname.to_string(), rel_start, rsp));
        }
    }

    fn op_end(&mut self, t0: f64) {
        let opname = self.p.opname.clone();
        self.op_end_as(t0, &opname);
    }

    fn op_end_as(&mut self, t0: f64, opname: &str) {
        let end = self.now();
        let rsp = end - t0;
        self.record_rsp(opname, t0, rsp);
        if self.p.auto_pause {
            self.adjust_pause_time(end, rsp);
        }
    }

    // port of smallfile's auto-pause: Little's-law throttle converging on the pause
    // that keeps utilization just below saturation
    fn adjust_pause_time(&mut self, end: f64, rsp: f64) {
        match self.pause_idx {
            None => {
                self.pause_hist_start = end - rsp;
                self.pause_hist = [rsp; PAUSE_RSTIME_COUNT];
                self.pause_idx = Some(1);
                self.pause_samples = 1;
                self.pause_sec = self.throttle * rsp;
                info!(
                    "thread {} per-thread pause initialized to {:.6}",
                    self.tid_label, self.pause_sec
                );
            }
            Some(i) => {
                self.pause_hist[i] = rsp;
                self.pause_idx = Some((i + 1) % PAUSE_RSTIME_COUNT);
                self.pause_samples += 1;
                if self.pause_hist_start + PAUSE_HIST_DURATION < end || self.pause_samples > PAUSE_RSTIME_COUNT / 2 {
                    self.calculate_pause_time(end);
                    self.pause_hist_start = end;
                    self.pause_samples = 0;
                }
            }
        }
    }

    fn calculate_pause_time(&mut self, end: f64) {
        let mean = self.pause_hist.iter().sum::<f64>() / PAUSE_RSTIME_COUNT as f64;
        let time_so_far = end - self.pause_hist_start;
        if time_so_far <= 0.0 {
            return;
        }
        let est_throughput = self.pause_samples as f64 * self.p.threads as f64 / time_so_far;
        let mean_util = mean * est_throughput;
        let old_pause = self.pause_sec;
        let new_pause = mean_util * mean * self.throttle;
        self.pause_sec = (old_pause + 2.0 * new_pause) / 3.0;
        info!(
            "thread {} per-thread pause changed from {old_pause:.6} to {:.6}",
            self.tid_label, self.pause_sec
        );
    }

    fn save_rsptimes(&self) -> io::Result<()> {
        if self.rsptimes.is_empty() {
            return Ok(());
        }
        let dir = self.p.top.join("network_shared");
        fs::create_dir_all(&dir)?;
        let ts = self
            .start_wall
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64())
            .unwrap_or(0.0);
        let path = dir.join(format!(
            "rsptimes_{}_{}_{}_{ts:.2}.csv",
            self.tid_label, self.p.host, self.p.opname
        ));
        let mut f = File::create(&path)?;
        for (opname, rel_start, rsp) in &self.rsptimes {
            writeln!(f, "{opname:>8}, {rel_start:9.6}, {rsp:9.6}")?;
        }
        fsync_raw(&f)?;
        debug!("thread {} wrote {}", self.tid_label, path.display());
        Ok(())
    }

    fn end_test(&mut self, flag: &AtomicBool) {
        if self.ended {
            return;
        }
        self.rq_final = Some(self.rq);
        self.filenum_final = Some(self.filenum);
        self.end = Some(Instant::now());
        if self.filenum >= self.p.iterations as usize {
            // first thread to finish all its files ends measurement for everyone
            flag.store(true, Ordering::SeqCst);
        }
        self.ended = true;
    }

    fn test_ended(&self) -> bool {
        self.ended
    }

    fn do_another_file(&mut self, flag: &AtomicBool) -> bool {
        if self.p.stonewall
            && (self.filenum as u32 + 1) % self.p.files_between_checks() == 0
            && flag.load(Ordering::SeqCst)
        {
            info!("thread {} saw stonewall after {} files", self.tid_label, self.filenum);
            self.end_test(flag);
        }
        if !self.p.finish_all_rq && self.test_ended() {
            return false;
        }
        if self.status != OK {
            self.end_test(flag);
            return false;
        }
        if self.filenum >= self.p.iterations as usize {
            self.end_test(flag);
            return false;
        }
        self.filenum += 1;
        // python gates the sleep on `iterations % files_between_pause == 0`, an apparent
        // typo for filenum that garbles pacing; we sleep every 5 files for the same
        // average per-file pause
        if self.pause_sec > 0.0 && self.filenum % FILES_BETWEEN_PAUSE == 0 {
            thread::sleep(Duration::from_secs_f64(self.pause_sec * FILES_BETWEEN_PAUSE as f64));
        }
        true
    }

    fn make_all_subdirs(&self) -> io::Result<()> {
        if self.p.is_shared_dir && self.tid != 0 {
            return Ok(()); // thread 00 pre-creates the shared tree for everyone
        }
        let nums = self.p.subdir_file_nums();
        let mut dirset = BTreeSet::new();
        for tree in [&self.src_dirs, &self.dest_dirs] {
            for &j in &nums {
                dirset.insert(self.mk_file_nm(tree, j).parent().unwrap().to_path_buf());
            }
        }
        for d in dirset {
            // create_dir_all tolerates the EEXIST races shared-dir threads run into
            fs::create_dir_all(&d)?;
        }
        Ok(())
    }

    fn clean_all_subdirs(&self) -> io::Result<()> {
        if self.p.is_shared_dir && self.tid != 0 {
            return Ok(());
        }
        let nums = self.p.subdir_file_nums();
        for tree in [&self.src_dirs, &self.dest_dirs] {
            let root = &tree[0];
            let mut dirset = BTreeSet::new();
            for &j in &nums {
                dirset.insert(self.mk_file_nm(tree, j).parent().unwrap().to_path_buf());
            }
            for d in dirset {
                let mut cur = d;
                while cur.starts_with(root) && cur != *root {
                    match fs::remove_dir(&cur) {
                        Ok(()) => {}
                        Err(e) => {
                            // ENOTEMPTY differs between Linux(39) and macOS(66); other
                            // "expected" failures: EACCES(13), EBUSY(16); ENOENT(2) falls through
                            match e.raw_os_error() {
                                Some(2) => {}
                                Some(13) | Some(16) | Some(39) | Some(66) => break,
                                _ => return Err(e),
                            }
                        }
                    }
                    cur = match cur.parent() {
                        Some(p) => p.to_path_buf(),
                        None => break,
                    };
                }
            }
        }
        Ok(())
    }

    fn run_op(&mut self, flag: &AtomicBool) -> io::Result<()> {
        match self.p.opname.as_str() {
            "create" => self.do_create(flag),
            "read" => self.do_read(flag),
            "append" => self.do_write(flag, true, false),
            "overwrite" => self.do_write(flag, false, false),
            "truncate-overwrite" => self.do_write(flag, false, true),
            "rename" => self.do_rename(flag),
            "delete" | "stat" | "chmod" | "mkdir" | "rmdir" => self.do_simple(flag),
            "delete-renamed" => self.do_delete_renamed(flag),
            "symlink" => self.do_symlink(flag),
            "readdir" => self.do_readdir(flag, false),
            "ls-l" => self.do_readdir(flag, true),
            "cleanup" => self.do_cleanup(flag),
            "setxattr" => self.do_setxattr(flag),
            "getxattr" => self.do_getxattr(flag),
            "swift-put" => self.do_swift_put(flag),
            "swift-get" => self.do_swift_get(flag),
            "await-create" => self.do_await_create(flag),
            _ => unreachable!("op validated at CLI"),
        }
    }

    fn do_create(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let t0 = self.now();
            // O_CREAT|O_EXCL: a pre-existing file must fail the run, not be reused
            let mut f = OpenOptions::new().write(true).create_new(true).open(&fnm)?;
            let biggest = self.biggest_buf.clone();
            let mut remaining_kb = self.get_next_file_size_kb();
            let uo = self.unique_offset();
            let rszkb = self.p.record_size_to_use();
            while remaining_kb > 0 {
                let next_kb = rszkb.min(remaining_kb);
                f.write_all(&biggest[uo..uo + next_kb as usize * 1024])?;
                self.rq += 1;
                remaining_kb -= next_kb;
            }
            if self.p.record_ctime_size {
                self.remember_ctime_size(&fnm, &f)?;
            }
            if self.p.fsync {
                fsync_raw(&f)?;
            }
            self.op_end(t0);
        }
        Ok(())
    }

    // async-replication latency support: remember wall time + size via xattr so a
    // later await-create pass can measure how long propagation took
    fn remember_ctime_size(&self, fnm: &Path, f: &File) -> io::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let kb = f.metadata()?.len() / 1024;
        xattr::set(fnm, CTIME_XATTR, format!("{ts},{kb}").as_bytes())
    }

    fn do_write(&mut self, flag: &AtomicBool, append: bool, truncate: bool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let t0 = self.now();
            let mut f = OpenOptions::new().write(true).truncate(truncate).open(&fnm)?;
            // deliberately lseek instead of O_APPEND: O_APPEND changes write atomicity
            // semantics, smallfile's append is "seek to end then write"
            if append {
                f.seek(SeekFrom::End(0))?;
            }
            let biggest = self.biggest_buf.clone();
            let mut remaining_kb = self.get_next_file_size_kb();
            let uo = self.unique_offset();
            let rszkb = self.p.record_size_to_use();
            while remaining_kb > 0 {
                let next_kb = rszkb.min(remaining_kb);
                f.write_all(&biggest[uo..uo + next_kb as usize * 1024])?;
                self.rq += 1;
                remaining_kb -= next_kb;
            }
            if self.p.record_ctime_size {
                self.remember_ctime_size(&fnm, &f)?;
            }
            if self.p.fsync {
                fsync_raw(&f)?;
            }
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_read(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let t0 = self.now();
            let next_fsz = self.get_next_file_size_kb();
            let mut f = File::open(&fnm)?;
            let biggest = self.biggest_buf.clone();
            let uo = self.unique_offset();
            // reads expect the first next_fsz of the file regardless of appends, because
            // get_next_file_size() replays the same (fixed or seeded) size sequence
            let mut remaining_kb = next_fsz;
            let rszkb = self.p.record_size_to_use();
            while remaining_kb > 0 {
                let next_kb = rszkb.min(remaining_kb);
                let n = next_kb as usize * 1024;
                f.read_exact(&mut self.scratch[..n])?;
                self.rq += 1;
                if self.p.verify_read && self.scratch[..n] != biggest[uo..uo + n] {
                    let matched = self.scratch[..n]
                        .iter()
                        .zip(&biggest[uo..uo + n])
                        .position(|(a, b)| a != b)
                        .unwrap_or(n);
                    return Err(io::Error::other(format!(
                        "read: file {} contents matched up through byte {}",
                        fnm.display(),
                        matched
                    )));
                }
                remaining_kb -= next_kb;
            }
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_rename(&mut self, flag: &AtomicBool) -> io::Result<()> {
        let in_same_dir = self.src_dirs == self.dest_dirs;
        while self.do_another_file(flag) {
            let fn1 = self.this_file_nm(&self.src_dirs);
            let fn2 = self.this_file_nm(&self.dest_dirs);
            let fn2 = if in_same_dir {
                suffixed(&fn2, RENAME_SUFFIX)
            } else {
                fn2
            };
            let t0 = self.now();
            fs::rename(&fn1, &fn2)?;
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_simple(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let t0 = self.now();
            match self.p.opname.as_str() {
                "delete" => {
                    fs::remove_file(&fnm)?;
                }
                "stat" => {
                    fs::metadata(&fnm)?;
                }
                "chmod" => {
                    fs::set_permissions(&fnm, fs::Permissions::from_mode(0o646))?;
                }
                "mkdir" => {
                    fs::create_dir(suffixed(&fnm, ".d"))?;
                }
                "rmdir" => {
                    fs::remove_dir(suffixed(&fnm, ".d"))?;
                }
                _ => unreachable!(),
            }
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_delete_renamed(&mut self, flag: &AtomicBool) -> io::Result<()> {
        let in_same_dir = self.src_dirs == self.dest_dirs;
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.dest_dirs);
            let fnm = if in_same_dir {
                suffixed(&fnm, RENAME_SUFFIX)
            } else {
                fnm
            };
            let t0 = self.now();
            fs::remove_file(&fnm)?;
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_symlink(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let orig = self.this_file_nm(&self.src_dirs);
            let fn2 = suffixed(&self.this_file_nm(&self.dest_dirs), ".s");
            let t0 = self.now();
            symlink(&orig, &fn2)?;
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_readdir(&mut self, flag: &AtomicBool, stat_each: bool) -> io::Result<()> {
        if self.p.hash_to_dir {
            return Err(io::Error::other("cannot do readdir test with --hash-into-dirs option"));
        }
        let top = self.p.top.clone();
        let opname = self.p.opname.clone();
        let mut prev_dir = String::new();
        let mut dir_map: HashSet<String> = HashSet::new();
        let mut file_count = 0usize;
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let dir = fnm.parent().unwrap();
            let common_dir = dir
                .strip_prefix(&top)
                .map_err(|e| io::Error::other(format!("readdir: {e}")))?
                .to_string_lossy()
                .into_owned();
            if common_dir != prev_dir {
                if file_count != dir_map.len() {
                    return Err(io::Error::other(format!(
                        "readdir: not all files in directory {prev_dir} were found"
                    )));
                }
                let t0 = self.now();
                dir_map = fs::read_dir(top.join(&common_dir))?
                    .map(|e| e.map(|d| d.file_name().to_string_lossy().into_owned()))
                    .collect::<Result<HashSet<_>, _>>()?
                    .into_iter()
                    .filter(|n| !n.starts_with('d'))
                    .collect();
                self.op_end_as(t0, &format!("{opname}-readdir"));
                prev_dir = common_dir;
                file_count = 0;
            }
            if stat_each {
                let t0 = self.now();
                fs::metadata(&fnm)?;
                self.op_end_as(t0, &format!("{opname}-stat"));
            }
            file_count += 1;
            if !dir_map.contains(&fnm.file_name().unwrap().to_string_lossy().into_owned()) {
                return Err(io::Error::other(format!(
                    "readdir: file missing from directory {prev_dir}"
                )));
            }
        }
        Ok(())
    }

    fn do_cleanup(&mut self, flag: &AtomicBool) -> io::Result<()> {
        let (saved_stonewall, saved_finish) = (self.p.stonewall, self.p.finish_all_rq);
        self.p.stonewall = false;
        self.p.finish_all_rq = true; // cleanup must always run to completion
        let result = (|| {
            while self.do_another_file(flag) {
                ensure_deleted(&suffixed(&self.this_file_nm(&self.dest_dirs), ".s"))?;
                let basenm = self.this_file_nm(&self.src_dirs);
                ensure_deleted(&basenm)?;
                ensure_deleted(&suffixed(&basenm, RENAME_SUFFIX))?;
                ensure_deleted(&self.this_file_nm(&self.dest_dirs))?;
                ensure_deleted(&suffixed(&self.this_file_nm(&self.dest_dirs), RENAME_SUFFIX))?;
                let dir = suffixed(&basenm, ".d");
                if dir.exists() {
                    fs::remove_dir(&dir)?;
                }
            }
            self.clean_all_subdirs()
        })();
        self.p.stonewall = saved_stonewall;
        self.p.finish_all_rq = saved_finish;
        result
    }

    // xattr j gets the pattern bytes [j, j+xattr_size) of this file's buffer slice,
    // exactly what do_getxattr compares against
    fn xattr_value<'a>(&self, biggest: &'a [u8], uo: usize, j: usize) -> &'a [u8] {
        let begin = (uo + j).min(biggest.len());
        let end = (uo + j + self.p.xattr_size).min(biggest.len());
        &biggest[begin..end]
    }

    fn do_setxattr(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let biggest = self.biggest_buf.clone();
            let uo = self.unique_offset();
            let t0 = self.now();
            let f = OpenOptions::new().write(true).open(&fnm)?;
            for j in 0..self.p.xattr_count {
                xattr::set(&fnm, &format!("{XATTR_SUFFIX}{j}"), self.xattr_value(&biggest, uo, j))?;
            }
            if self.p.fsync {
                fsync_raw(&f)?; // fsync also flushes xattr values and metadata
            }
            drop(f);
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_getxattr(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let biggest = self.biggest_buf.clone();
            let uo = self.unique_offset();
            let t0 = self.now();
            for j in 0..self.p.xattr_count {
                let name = format!("{XATTR_SUFFIX}{j}");
                match xattr::get(&fnm, &name)? {
                    Some(v) if v == self.xattr_value(&biggest, uo, j) => {}
                    Some(v) => {
                        return Err(io::Error::other(format!(
                            "getxattr: value contents wrong for {name} ({} bytes)",
                            v.len()
                        )));
                    }
                    None => {
                        return Err(io::Error::other(format!(
                            "getxattr: {name} does not exist on {}",
                            fnm.display()
                        )));
                    }
                }
            }
            self.op_end(t0);
        }
        Ok(())
    }

    // emulates an OpenStack Swift PUT: preallocate, write, attach xattrs, drop the
    // page cache, then rename into place -- one request per file regardless of records
    fn do_swift_put(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let finalnm = self.this_file_nm(&self.src_dirs);
            let tmpnm = suffixed(&finalnm, ".tmp");
            let next_fsz = self.get_next_file_size_kb();
            let biggest = self.biggest_buf.clone();
            let uo = self.unique_offset();
            let t0 = self.now();
            let result = (|| -> io::Result<()> {
                let mut f = OpenOptions::new().write(true).create(true).open(&tmpnm)?;
                let fd = f.as_raw_fd();
                if unsafe { libc::fchmod(fd, 0o667) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                let fszbytes = next_fsz as i64 * 1024;
                preallocate(fd, fszbytes)?;
                let mut remaining_kb = next_fsz;
                let rszkb = self.p.record_size_to_use();
                while remaining_kb > 0 {
                    let next_kb = rszkb.min(remaining_kb);
                    f.write_all(&biggest[uo..uo + next_kb as usize * 1024])?;
                    remaining_kb -= next_kb;
                }
                for j in 0..self.p.xattr_count {
                    let name = format!("{SWIFT_XATTR_PREFIX}{j}");
                    if xattr::get(&tmpnm, &name)?.is_none() {
                        error!("xattr {name} does not exist");
                    }
                }
                for j in 0..self.p.xattr_count {
                    xattr::set(
                        &tmpnm,
                        &format!("{SWIFT_XATTR_PREFIX}{j}"),
                        self.xattr_value(&biggest, uo, j),
                    )?;
                }
                if self.p.fsync {
                    fsync_raw(&f)?; // flush both data and metadata with one fsync
                }
                drop(f);
                #[cfg(target_os = "linux")]
                // we assume here that data will not be read anytime soon
                if unsafe { libc::posix_fadvise(fd, 0, fszbytes, libc::POSIX_FADV_DONTNEED) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                fs::rename(&tmpnm, &finalnm)?;
                self.rq += 1;
                Ok(())
            })();
            if let Err(e) = result {
                ensure_deleted(&tmpnm)?;
                return Err(e);
            }
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_swift_get(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let next_fsz = self.get_next_file_size_kb();
            let biggest = self.biggest_buf.clone();
            let t0 = self.now();
            let mut f = File::open(&fnm)?;
            let uo = self.unique_offset();
            let mut remaining_kb = next_fsz;
            let rszkb = self.p.record_size_to_use();
            while remaining_kb > 0 {
                let next_kb = rszkb.min(remaining_kb);
                let n = next_kb as usize * 1024;
                f.read_exact(&mut self.scratch[..n])?;
                if self.p.verify_read && self.scratch[..n] != biggest[uo..uo + n] {
                    return Err(io::Error::other(format!(
                        "swift-get: file {} contents wrong",
                        fnm.display()
                    )));
                }
                remaining_kb -= next_kb;
                self.rq += 1;
            }
            for j in 0..self.p.xattr_count {
                xattr::get(&fnm, &format!("{SWIFT_XATTR_PREFIX}{j}"))?;
            }
            self.op_end(t0);
        }
        Ok(())
    }

    // for async (geo-)replication testing: wait for files created elsewhere to appear
    // and reach their original size; the response time then measures replication lag
    fn do_await_create(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            debug!("awaiting file {}", fnm.display());
            while !fnm.exists() {
                thread::sleep(Duration::from_secs(1));
            }
            let (ctime, size_kb) = loop {
                if let Some(v) = xattr::get(&fnm, CTIME_XATTR)? {
                    let s = String::from_utf8_lossy(&v);
                    let mut it = s.split(',');
                    let ctime: f64 = it
                        .next()
                        .and_then(|t| t.parse().ok())
                        .ok_or_else(|| io::Error::other("await-create: bad ctime xattr"))?;
                    let kb: u64 = it
                        .next()
                        .and_then(|t| t.split('.').next())
                        .and_then(|t| t.parse().ok())
                        .ok_or_else(|| io::Error::other("await-create: bad size xattr"))?;
                    break (ctime, kb);
                }
                thread::sleep(Duration::from_secs(1));
            };
            loop {
                let len = fs::metadata(&fnm)?.len();
                if len > size_kb * 1024 {
                    return Err(io::Error::other(format!(
                        "asynchronously created replica in {} is larger than original {size_kb} KB",
                        fnm.display()
                    )));
                }
                if len == size_kb * 1024 {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            let now_ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let rel_start = self
                .start_wall
                .map(|w| ctime - w.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64())
                .unwrap_or(ctime);
            self.record_rsp("await-create", rel_start, now_ts - ctime);
        }
        Ok(())
    }

    fn into_outcome(self) -> Outcome {
        Outcome {
            tid: self.tid_label,
            status: self.status,
            elapsed: self.end.zip(self.start).map(|(e, s)| (e - s).as_secs_f64()),
            files: self.filenum_final.unwrap_or(0),
            records: self.rq_final.unwrap_or(0),
        }
    }
}

struct Outcome {
    tid: String,
    status: i32,
    elapsed: Option<f64>,
    files: usize,
    records: u64,
}

fn run_phase(p: &Params) -> io::Result<Vec<Outcome>> {
    for d in [p.src_base(), p.dest_base()] {
        fs::create_dir_all(&d)?;
    }
    let n = p.threads as usize;
    let barrier = Barrier::new(n);
    let stonewall_flag = AtomicBool::new(false);
    let biggest_buf = Arc::new(build_biggest_buf());
    let outcomes = thread::scope(|s| {
        let mut joins = Vec::with_capacity(n);
        for tid in 0..n {
            let barrier = &barrier;
            let flag = &stonewall_flag;
            let buf = biggest_buf.clone();
            let tp = p.clone();
            joins.push(s.spawn(move || {
                let mut w = Workload::new(tp, tid, buf);
                let pre = if matches!(w.p.opname.as_str(), "create" | "mkdir" | "swift-put") {
                    w.make_all_subdirs()
                } else {
                    Ok(())
                };
                if let Err(e) = pre {
                    w.status = errno(&e);
                    error!("thread {}: {}", w.tid_label, e);
                    return w.into_outcome();
                }
                barrier.wait();
                w.start = Some(Instant::now());
                w.start_wall = Some(SystemTime::now());
                if let Err(e) = w.run_op(flag) {
                    w.status = errno(&e);
                    error!("thread {} op {} failed: {}", w.tid_label, w.p.opname, e);
                }
                w.end_test(flag);
                if let Err(e) = w.save_rsptimes() {
                    warn!("thread {}: could not save rsptimes: {}", w.tid_label, e);
                }
                let o = w.into_outcome();
                debug!("thread {} done: {:?}", o.tid, (o.files, o.records, o.status));
                o
            }));
        }
        joins.into_iter().map(|j| j.join().unwrap()).collect()
    });
    Ok(outcomes)
}

struct Agg {
    status: i32,
    elapsed: f64,
    files: usize,
    records: u64,
    files_per_sec: f64,
    iops: f64,
    mibps: f64,
}

fn phase_results(p: &Params, outcomes: &[Outcome], phase_start: SystemTime) -> (Value, Agg) {
    let rszkb = p.record_size_to_use();
    let mut threads = Map::new();
    let mut agg = Agg {
        status: OK,
        elapsed: 0.0,
        files: 0,
        records: 0,
        files_per_sec: 0.0,
        iops: 0.0,
        mibps: 0.0,
    };
    for o in outcomes {
        let (elapsed, fps, iops, mibps) = match o.elapsed {
            Some(el) if el > 0.0 => {
                let fps = o.files as f64 / el;
                if o.records > 0 {
                    (
                        el,
                        fps,
                        o.records as f64 / el,
                        o.records as f64 * rszkb as f64 / KIB_PER_MIB / el,
                    )
                } else {
                    (el, fps, 0.0, 0.0)
                }
            }
            _ => {
                warn!("thread {} never completed", o.tid);
                (1e8, 0.0, 0.0, 0.0)
            }
        };
        let mut t = json!({
            "status": status_str(o.status),
            "elapsed": elapsed,
            "files": o.files,
            "records": o.records,
            "filesPerSec": fps,
        });
        if o.records > 0 {
            t["IOPS"] = json!(iops);
            t["MiBps"] = json!(mibps);
        }
        threads.insert(o.tid.clone(), t);
        // aggregation semantics from smallfile's output_results: rates are summed
        // per-thread, elapsed is the max, status is the first non-OK thread
        if agg.status == OK {
            agg.status = o.status;
        }
        agg.elapsed = agg.elapsed.max(elapsed);
        agg.files += o.files;
        agg.records += o.records;
        if elapsed > 0.0 {
            agg.files_per_sec += fps;
            if o.records > 0 {
                agg.iops += iops;
                agg.mibps += mibps;
            }
        }
    }
    let max_files = p.iterations as f64 * outcomes.len() as f64;
    let pct_files = 100.0 * agg.files as f64 / max_files;
    let mut r = json!({
        "status": status_str(agg.status),
        "elapsed": agg.elapsed,
        "files": agg.files,
        "records": agg.records,
        "filesPerSec": agg.files_per_sec,
        "totalThreads": outcomes.len(),
        "pctFilesDone": pct_files,
        "startTime": phase_start.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64(),
        "thread": threads,
    });
    if agg.records > 0 {
        r["IOPS"] = json!(agg.iops);
        r["MiBps"] = json!(agg.mibps);
        r["totalDataGB"] = json!(agg.records as f64 * rszkb as f64 / KB_PER_GB);
    }
    if agg.status != OK {
        warn!("at least one thread encountered error, phase may be incomplete");
    } else if pct_files < PCT_FILES_MIN {
        warn!("only {pct_files:.2}% of requested files processed, threshold is {PCT_FILES_MIN}%");
    }
    (r, agg)
}

pub fn run(top: &Path, args: &Args) -> io::Result<Value> {
    let meta = fs::symlink_metadata(top)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", top.display())));
    }
    if args.record_size > args.file_size && args.file_size != 0 {
        return Err(io::Error::other("record size must not be greater than file size"));
    }
    let ops: Vec<String> = match &args.operation {
        Some(op) => vec![op.clone()],
        None => DEFAULT_PHASES.iter().map(|s| s.to_string()).collect(),
    };
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let mut stonewall = args.stonewall;
    if args.files < 10 {
        stonewall = false;
    }
    if args.operation.is_none() && stonewall {
        // multi-phase suite must not truncate slow threads: later phases would fail
        // on files the stonewalled threads never wrote (smallfile's regtest.sh
        // likewise disables stonewall for sequential op runs)
        stonewall = false;
        info!("stonewall disabled for multi-phase suite run");
    }
    let p = Params {
        opname: String::new(),
        iterations: args.files,
        threads: args.threads,
        record_sz_kb: args.record_size,
        total_sz_kb: args.file_size,
        filesize_distr: if args.file_size_distribution == "exponential" {
            Distr::Exponential
        } else {
            Distr::Fixed
        },
        files_per_dir: args.files_per_dir,
        dirs_per_dir: args.dirs_per_dir,
        prefix: args.prefix.clone(),
        suffix: args.suffix.clone(),
        stonewall,
        finish_all_rq: args.finish,
        verify_read: args.verify_read,
        fsync: args.fsync,
        hash_to_dir: args.hash_into_dirs,
        is_shared_dir: args.same_dir,
        pause_between_files_us: args.pause,
        auto_pause: args.auto_pause,
        measure_rsptimes: args.response_times,
        xattr_size: args.xattr_size,
        xattr_count: args.xattr_count,
        record_ctime_size: args.record_ctime_size,
        top: top.to_path_buf(),
        host: host.clone(),
    };
    info!(
        "fstest smallfile: top={} threads={} files/thread={} file_size={}KB record_size={}KB files_per_dir={} dirs_per_dir={} stonewall={} fsync={} verify_read={}",
        top.display(),
        p.threads,
        p.iterations,
        p.total_sz_kb,
        p.record_sz_kb,
        p.files_per_dir,
        p.dirs_per_dir,
        p.stonewall,
        p.fsync,
        p.verify_read
    );
    let suite_start = SystemTime::now();
    let mut phases = Vec::new();
    let mut all_ok = true;
    for op in &ops {
        let mut po = p.clone();
        po.opname = op.clone();
        let phase_start = SystemTime::now();
        info!("phase '{op}' started");
        let outcomes = run_phase(&po)?;
        let (results, agg) = phase_results(&po, &outcomes, phase_start);
        all_ok &= agg.status == OK;
        let mut line = format!(
            "phase '{op}' finished: status={} elapsed={:.3}s files={} filesPerSec={:.1}",
            status_str(agg.status),
            agg.elapsed,
            agg.files,
            agg.files_per_sec
        );
        if agg.records > 0 {
            line += &format!(
                " IOPS={:.1} MiBps={:.2} totalData={:.3}GiB",
                agg.iops,
                agg.mibps,
                agg.records as f64 * po.record_size_to_use() as f64 / KB_PER_GB
            );
        }
        info!("{line}");
        phases.push(json!({
            "operation": op,
            "date": iso8601(phase_start),
            "results": results,
        }));
    }
    let params_json = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "top": top.display().to_string(),
        "operation": if ops.len() == 1 { json!(ops[0]) } else { json!(ops) },
        "files_per_thread": p.iterations,
        "threads": p.threads,
        "file_size": p.total_sz_kb,
        "file_size_distr": if p.filesize_distr == Distr::Exponential { "exponential" } else { "fixed" },
        "record_size": p.record_sz_kb,
        "record_size_effective": p.record_size_to_use(),
        "files_per_dir": p.files_per_dir,
        "dirs_per_dir": p.dirs_per_dir,
        "stonewall": p.stonewall,
        "finish_all_requests": p.finish_all_rq,
        "verify_read": p.verify_read,
        "fsync_after_modify": p.fsync,
        "fname_prefix": p.prefix,
        "fname_suffix": p.suffix,
        "share_dir": p.is_shared_dir,
        "hash_to_dir": p.hash_to_dir,
        "pause_between_files_us": p.pause_between_files_us,
        "auto_pause": p.auto_pause,
        "measure_rsptimes": p.measure_rsptimes,
        "xattr_size": p.xattr_size,
        "xattr_count": p.xattr_count,
        "record_ctime_size": p.record_ctime_size,
    });
    let mut report = json!({
        "suite": "smallfile",
        "host": host,
        "date": iso8601(suite_start),
        "top": top.display().to_string(),
        "params": params_json,
        "status": if all_ok { "ok" } else { "failed" },
    });
    if ops.len() == 1 {
        report["results"] = phases.remove(0)["results"].clone();
    } else {
        report["phases"] = json!(phases);
    }
    Ok(report)
}
