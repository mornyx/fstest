// Port of `smallfile` (https://github.com/distributed-system-analysis/smallfile, Apache-2.0):
// multi-threaded POSIX small-file metadata and data benchmark.

use crate::common::{
    SplitMix64, fsync_raw, host_name, iso8601, posix_fallocate_fd, remove_dir_all_bounded,
    suffixed,
};
use log::{error, info, warn};
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
use xattr::FileExt as _;

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
    /// run only this operation instead of the default 7-phase sequence
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

    /// finish remaining files after stonewall fires (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub finish: bool,

    /// verify read data against what was written (Y/N)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub verify_read: bool,

    /// call fsync() after each file is modified (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub fsync: bool,

    /// fill data buffer with deterministic pseudo-random bytes to defeat compression (Y/N)
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub incompressible: bool,

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

    /// timeout in seconds for `await-create` polling
    #[arg(long, default_value_t = 300)]
    pub await_timeout: u64,

    /// write the enriched JSON result to this file
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
    incompressible: bool,
    hash_to_dir: bool,
    is_shared_dir: bool,
    pause_between_files_us: u64,
    auto_pause: bool,
    measure_rsptimes: bool,
    xattr_size: usize,
    xattr_count: usize,
    record_ctime_size: bool,
    await_timeout_sec: u64,
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
            100u32.saturating_sub(self.total_sz_kb / 100).max(10)
        } else {
            20
        }
    }
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
    fn mk_seq_dir_name(&self, file_num: u32) -> String {
        let mut dir_in = file_num / self.files_per_dir;
        let mut level_dirs = Vec::new();
        let mut dirs_for_this_level = self.dirs_per_dir as u64;
        while dirs_for_this_level <= dir_in as u64 && self.dirs_per_dir > 1 {
            level_dirs.push(dirs_for_this_level as u32);
            dirs_for_this_level = dirs_for_this_level.saturating_mul(self.dirs_per_dir as u64);
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
        while dir_num > 1 && self.dirs_per_dir > 1 {
            let dir_num_hash = dir_num as u64 * SOME_PRIME % self.dirs_per_dir as u64;
            pathlist.insert(0, format!("h_{dir_num_hash:03}"));
            dir_num /= self.dirs_per_dir as usize;
        }
        pathlist.join("/")
    }
}

// Shared payload buffer with 1 KiB tail so per-file slices at `unique_offset()` stay
// contiguous and deterministic between separate `create` and `read` invocations.
fn build_biggest_buf(incompressible: bool) -> Vec<u8> {
    if incompressible {
        let mut buf = vec![0u8; BIGGEST_BUF_SIZE + BUF_OFFSET_RANGE];
        let mut rng = SplitMix64::new(0x534D_414C_4C46_494C);
        rng.fill_bytes(&mut buf);
        return buf;
    }
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

fn ensure_deleted(p: &Path) -> io::Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(EIO)
}

fn status_str(status: i32) -> String {
    if status == OK {
        "ok".to_string()
    } else {
        io::Error::from_raw_os_error(status).to_string()
    }
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
    err_msg: Option<String>,
    rng: SplitMix64,
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
            rng: SplitMix64::new(h.finish()),
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
            err_msg: None,
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
        (self.rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

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

    fn adjust_pause_time(&mut self, end: f64, rsp: f64) {
        match self.pause_idx {
            None => {
                self.pause_hist_start = end - rsp;
                self.pause_hist = [rsp; PAUSE_RSTIME_COUNT];
                self.pause_idx = Some(1);
                self.pause_samples = 1;
                self.pause_sec = self.throttle * rsp;
            }
            Some(i) => {
                self.pause_hist[i] = rsp;
                self.pause_idx = Some((i + 1) % PAUSE_RSTIME_COUNT);
                self.pause_samples += 1;
                if self.pause_hist_start + PAUSE_HIST_DURATION < end
                    || self.pause_samples > PAUSE_RSTIME_COUNT / 2
                {
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
            info!(
                "thread {} saw stonewall after {} files",
                self.tid_label, self.filenum
            );
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
        if self.pause_sec > 0.0 && self.filenum % FILES_BETWEEN_PAUSE == 0 {
            thread::sleep(Duration::from_secs_f64(
                self.pause_sec * FILES_BETWEEN_PAUSE as f64,
            ));
        }
        true
    }

    fn make_all_subdirs(&self) -> io::Result<()> {
        if self.p.is_shared_dir && self.tid != 0 {
            return Ok(());
        }
        let nums = self.p.subdir_file_nums();
        let mut dirset = BTreeSet::new();
        for tree in [&self.src_dirs, &self.dest_dirs] {
            for &j in &nums {
                dirset.insert(self.mk_file_nm(tree, j).parent().unwrap().to_path_buf());
            }
        }
        for d in dirset {
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
                        Err(e) => match e.raw_os_error() {
                            Some(x) if x == libc::ENOENT => {}
                            Some(x)
                                if x == libc::EACCES
                                    || x == libc::EBUSY
                                    || x == libc::ENOTEMPTY =>
                            {
                                break;
                            }
                            _ => return Err(e),
                        },
                    }
                    cur = match cur.parent() {
                        Some(p) => p.to_path_buf(),
                        None => break,
                    };
                }
            }
            let _ = fs::remove_dir(root);
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
                self.remember_ctime_size(&f)?;
            }
            if self.p.fsync {
                fsync_raw(&f)?;
            }
            self.op_end(t0);
        }
        Ok(())
    }

    fn remember_ctime_size(&self, f: &File) -> io::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let kb = f.metadata()?.len() / 1024;
        f.set_xattr(CTIME_XATTR, format!("{ts},{kb}").as_bytes())
    }

    fn do_write(&mut self, flag: &AtomicBool, append: bool, truncate: bool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let t0 = self.now();
            let mut f = OpenOptions::new().write(true).truncate(truncate).open(&fnm)?;
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
                self.remember_ctime_size(&f)?;
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
                        "read: file {} contents matched up through byte {matched}",
                        fnm.display(),
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
                "delete" => fs::remove_file(&fnm)?,
                "stat" => {
                    let _ = fs::metadata(&fnm)?;
                }
                "chmod" => fs::set_permissions(&fnm, fs::Permissions::from_mode(0o646))?,
                "mkdir" => fs::create_dir(suffixed(&fnm, ".d"))?,
                "rmdir" => fs::remove_dir(suffixed(&fnm, ".d"))?,
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
            return Err(io::Error::other(
                "cannot do readdir test with --hash-into-dirs option",
            ));
        }
        let top = self.p.top.clone();
        let opname = self.p.opname.clone();
        let thread_tag = format!("_{}_{}_", self.p.host, self.tid_label);
        let shared = self.p.is_shared_dir;
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
                if !prev_dir.is_empty() && file_count != dir_map.len() {
                    return Err(io::Error::other(format!(
                        "readdir: expected {} files in {prev_dir}, saw {}",
                        file_count,
                        dir_map.len()
                    )));
                }
                let t0 = self.now();
                dir_map = fs::read_dir(top.join(&common_dir))?
                    .map(|e| e.map(|d| d.file_name().to_string_lossy().into_owned()))
                    .collect::<Result<HashSet<_>, _>>()?
                    .into_iter()
                    .filter(|n| !n.starts_with("d_") && !n.starts_with("h_"))
                    .filter(|n| !shared || n.contains(&thread_tag))
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
            let leaf = fnm.file_name().unwrap().to_string_lossy().into_owned();
            if !dir_map.contains(&leaf) {
                return Err(io::Error::other(format!(
                    "readdir: file {leaf} missing from directory {prev_dir}"
                )));
            }
        }
        if !prev_dir.is_empty()
            && self.filenum >= self.p.iterations as usize
            && file_count != dir_map.len()
        {
            return Err(io::Error::other(format!(
                "readdir: expected {} files in final directory {prev_dir}, saw {}",
                file_count,
                dir_map.len()
            )));
        }
        Ok(())
    }

    fn do_cleanup(&mut self, flag: &AtomicBool) -> io::Result<()> {
        let (saved_stonewall, saved_finish) = (self.p.stonewall, self.p.finish_all_rq);
        self.p.stonewall = false;
        self.p.finish_all_rq = true;
        let result = (|| {
            while self.do_another_file(flag) {
                ensure_deleted(&suffixed(&self.this_file_nm(&self.dest_dirs), ".s"))?;
                let basenm = self.this_file_nm(&self.src_dirs);
                ensure_deleted(&basenm)?;
                ensure_deleted(&suffixed(&basenm, ".tmp"))?;
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
                f.set_xattr(&format!("{XATTR_SUFFIX}{j}"), self.xattr_value(&biggest, uo, j))?;
            }
            if self.p.fsync {
                fsync_raw(&f)?;
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
            let f = File::open(&fnm)?;
            for j in 0..self.p.xattr_count {
                let name = format!("{XATTR_SUFFIX}{j}");
                match f.get_xattr(&name)? {
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

    fn do_swift_put(&mut self, flag: &AtomicBool) -> io::Result<()> {
        while self.do_another_file(flag) {
            let finalnm = self.this_file_nm(&self.src_dirs);
            let tmpnm = suffixed(&finalnm, ".tmp");
            let next_fsz = self.get_next_file_size_kb();
            let biggest = self.biggest_buf.clone();
            let uo = self.unique_offset();
            let t0 = self.now();
            let result = (|| -> io::Result<()> {
                let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmpnm)?;
                let fd = f.as_raw_fd();
                if unsafe { libc::fchmod(fd, 0o667) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                let fszbytes = next_fsz as i64 * 1024;
                if fszbytes > 0 {
                    let _ = posix_fallocate_fd(fd, 0, fszbytes);
                }
                let mut remaining_kb = next_fsz;
                let rszkb = self.p.record_size_to_use();
                while remaining_kb > 0 {
                    let next_kb = rszkb.min(remaining_kb);
                    f.write_all(&biggest[uo..uo + next_kb as usize * 1024])?;
                    remaining_kb -= next_kb;
                }
                for j in 0..self.p.xattr_count {
                    let _ = f.get_xattr(&format!("{SWIFT_XATTR_PREFIX}{j}"))?;
                }
                for j in 0..self.p.xattr_count {
                    f.set_xattr(
                        &format!("{SWIFT_XATTR_PREFIX}{j}"),
                        self.xattr_value(&biggest, uo, j),
                    )?;
                }
                if self.p.fsync {
                    fsync_raw(&f)?;
                }
                #[cfg(target_os = "linux")]
                {
                    // `posix_fadvise` returns a positive errno directly without setting `errno`,
                    // and MUST run while `f` is still open.
                    let rc = unsafe {
                        libc::posix_fadvise(fd, 0, fszbytes, libc::POSIX_FADV_DONTNEED)
                    };
                    if rc != 0 {
                        return Err(io::Error::from_raw_os_error(rc));
                    }
                }
                drop(f);
                fs::rename(&tmpnm, &finalnm)?;
                self.rq += 1;
                Ok(())
            })();
            if let Err(e) = result {
                let _ = ensure_deleted(&tmpnm);
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
                let _ = f.get_xattr(&format!("{SWIFT_XATTR_PREFIX}{j}"))?;
            }
            self.op_end(t0);
        }
        Ok(())
    }

    fn do_await_create(&mut self, flag: &AtomicBool) -> io::Result<()> {
        let bound = Duration::from_secs(self.p.await_timeout_sec.max(1));
        while self.do_another_file(flag) {
            let fnm = self.this_file_nm(&self.src_dirs);
            let wait_t0 = Instant::now();
            while !fnm.exists() {
                if wait_t0.elapsed() > bound {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("await-create timed out waiting for {}", fnm.display()),
                    ));
                }
                thread::sleep(Duration::from_millis(200));
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
                if wait_t0.elapsed() > bound {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("await-create timed out waiting for xattr on {}", fnm.display()),
                    ));
                }
                thread::sleep(Duration::from_millis(200));
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
                if wait_t0.elapsed() > bound {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("await-create timed out waiting for full size on {}", fnm.display()),
                    ));
                }
                thread::sleep(Duration::from_millis(5));
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
            err_msg: self.err_msg,
            elapsed: self.end.zip(self.start).map(|(e, s)| (e - s).as_secs_f64()),
            files: self.filenum_final.unwrap_or(0),
            records: self.rq_final.unwrap_or(0),
        }
    }
}

struct Outcome {
    tid: String,
    status: i32,
    err_msg: Option<String>,
    elapsed: Option<f64>,
    files: usize,
    records: u64,
}

fn run_phase(p: &Params, biggest_buf: &Arc<Vec<u8>>) -> io::Result<Vec<Outcome>> {
    if p.opname != "cleanup" {
        for d in [p.src_base(), p.dest_base()] {
            fs::create_dir_all(&d)?;
        }
    }
    let n = p.threads as usize;
    let barrier = Barrier::new(n);
    let setup_failed = AtomicBool::new(false);
    let stonewall_flag = AtomicBool::new(false);

    let outcomes: Vec<Outcome> = thread::scope(|s| {
        let mut joins = Vec::with_capacity(n);
        for tid in 0..n {
            let barrier = &barrier;
            let setup_failed = &setup_failed;
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
                    w.err_msg = Some(e.to_string());
                    setup_failed.store(true, Ordering::SeqCst);
                    error!("thread {}: {}", w.tid_label, e);
                }
                // Always enter `barrier.wait()` even after setup failure so sibling threads never deadlock.
                barrier.wait();
                if setup_failed.load(Ordering::SeqCst) {
                    return w.into_outcome();
                }
                w.start = Some(Instant::now());
                w.start_wall = Some(SystemTime::now());
                if let Err(e) = w.run_op(flag) {
                    w.status = errno(&e);
                    w.err_msg = Some(e.to_string());
                    error!("thread {} op {} failed: {}", w.tid_label, w.p.opname, e);
                }
                w.end_test(flag);
                if let Err(e) = w.save_rsptimes() {
                    warn!("thread {}: could not save rsptimes: {}", w.tid_label, e);
                }
                w.into_outcome()
            }));
        }
        joins.into_iter().map(|j| j.join().unwrap()).collect()
    });

    if p.opname == "cleanup" {
        let _ = fs::remove_dir(p.src_base());
        let _ = fs::remove_dir(p.dest_base());
        let _ = fs::remove_dir(p.top.join("file_srcdir"));
        let _ = fs::remove_dir(p.top.join("file_dstdir"));
    }
    Ok(outcomes)
}

struct Agg {
    status: i32,
    failure: Option<String>,
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
        failure: None,
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
            _ => (0.0, 0.0, 0.0, 0.0),
        };
        let t_status = o.err_msg.clone().unwrap_or_else(|| status_str(o.status));
        let mut t = json!({
            "status": if o.status == OK { "ok".into() } else { t_status.clone() },
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
        if agg.status == OK && o.status != OK {
            agg.status = o.status;
            agg.failure = Some(t_status);
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
    let max_files = (p.iterations as f64 * outcomes.len() as f64).max(1.0);
    let pct_files = 100.0 * agg.files as f64 / max_files;
    let mut r = json!({
        "status": if agg.status == OK { "ok".into() } else { agg.failure.clone().unwrap_or_else(|| status_str(agg.status)) },
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
        return Err(io::Error::other(
            "record size must not be greater than file size",
        ));
    }
    let ops: Vec<String> = match &args.operation {
        Some(op) => vec![op.clone()],
        None => DEFAULT_PHASES.iter().map(|s| s.to_string()).collect(),
    };
    let host = host_name();
    let mut stonewall = args.stonewall;
    if args.files < 10 {
        stonewall = false;
    }
    if args.operation.is_none() && stonewall {
        stonewall = false;
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
        incompressible: args.incompressible,
        hash_to_dir: args.hash_into_dirs,
        is_shared_dir: args.same_dir,
        pause_between_files_us: args.pause,
        auto_pause: args.auto_pause,
        measure_rsptimes: args.response_times,
        xattr_size: args.xattr_size,
        xattr_count: args.xattr_count,
        record_ctime_size: args.record_ctime_size,
        await_timeout_sec: args.await_timeout,
        top: top.to_path_buf(),
        host: host.clone(),
    };

    info!(
        "smallfile: top={} threads={} files/thread={} file_size={}KB record_size={}KB stonewall={} fsync={} incompressible={} verify_read={}",
        top.display(),
        p.threads,
        p.iterations,
        p.total_sz_kb,
        p.record_sz_kb,
        p.stonewall,
        p.fsync,
        p.incompressible,
        p.verify_read
    );

    let biggest_buf = Arc::new(build_biggest_buf(p.incompressible));
    let suite_start = SystemTime::now();
    let mut phases = Vec::new();
    let mut all_ok = true;
    let mut first_failure: Option<String> = None;

    for (idx, op) in ops.iter().enumerate() {
        let mut po = p.clone();
        po.opname = op.clone();
        // Populate/cleanup operations must always finish creating/removing all items even when
        // stonewall ends measurement early (upstream `parse.py` behavior).
        if matches!(op.as_str(), "cleanup" | "create" | "mkdir" | "swift-put") {
            po.finish_all_rq = true;
        }
        if op == "cleanup" {
            po.pause_between_files_us = 0;
            po.auto_pause = false;
        }
        let phase_start = SystemTime::now();
        let outcomes = run_phase(&po, &biggest_buf)?;
        let (results, agg) = phase_results(&po, &outcomes, phase_start);
        let ok = agg.status == OK;
        all_ok &= ok;
        if !ok && first_failure.is_none() {
            first_failure = agg.failure.clone();
        }
        let mut line = format!(
            "phase '{op}' finished: status={} elapsed={:.3}s files={} filesPerSec={:.1}",
            if ok { "ok" } else { "failed" },
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
        let mut phase_obj = json!({
            "operation": op,
            "status": if ok { "ok" } else { "failed" },
            "date": iso8601(phase_start),
            "results": results,
        });
        if let Some(f) = &agg.failure {
            phase_obj["failure"] = json!(f);
        }
        phases.push(phase_obj);

        if op == "cleanup" && idx + 1 == ops.len() && ops.len() > 1 {
            remove_dir_all_bounded(&top.join("file_srcdir"), Duration::from_secs(10));
            remove_dir_all_bounded(&top.join("file_dstdir"), Duration::from_secs(10));
        }
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
        "incompressible": p.incompressible,
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
    if let Some(f) = first_failure {
        report["failure"] = json!(f);
    }
    if ops.len() == 1 {
        report["results"] = phases.remove(0)["results"].clone();
    } else {
        report["phases"] = json!(phases);
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
    fn smallfile_multiphase_and_swift_and_readdir_shared_dir_pass_cleanly() {
        let dir = std::env::temp_dir().join(format!("fstest-smallfile-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 1. Default multi-phase run with `--incompressible Y` leaves zero leftover directories.
        let cli = TestCli::parse_from([
            "smallfile",
            "--files",
            "20",
            "--threads",
            "2",
            "--file-size",
            "8",
            "--incompressible",
            "Y",
        ]);
        let rep = run(&dir, &cli.args).expect("multiphase");
        assert_eq!(rep["status"], json!("ok"), "rep: {rep}");
        assert!(fs::read_dir(&dir).unwrap().next().is_none());

        // 2. Single-op `swift-put` -> `swift-get` -> `readdir` (`--same-dir Y` + `--prefix data`) -> `cleanup`.
        for op in ["swift-put", "swift-get", "ls-l", "cleanup"] {
            let op_cli = TestCli::parse_from([
                "smallfile",
                "--operation",
                op,
                "--files",
                "16",
                "--threads",
                "2",
                "--files-per-dir",
                "8",
                "--file-size",
                "4",
                "--same-dir",
                "Y",
                "--prefix",
                "data",
                "--xattr-count",
                "2",
                "--xattr-size",
                "16",
            ]);
            let r = run(&dir, &op_cli.args).expect(op);
            assert_eq!(r["status"], json!("ok"), "{op} failed: {r}");
        }

        // 3. Large `--file-size` (>= 10100 KB) never underflows `files_between_checks()`.
        let mut p_large = Params {
            opname: "create".into(),
            iterations: 10,
            threads: 1,
            record_sz_kb: 0,
            total_sz_kb: 25_000,
            filesize_distr: Distr::Fixed,
            files_per_dir: 10,
            dirs_per_dir: 2,
            prefix: String::new(),
            suffix: String::new(),
            stonewall: true,
            finish_all_rq: false,
            verify_read: true,
            fsync: false,
            incompressible: false,
            hash_to_dir: false,
            is_shared_dir: false,
            pause_between_files_us: 0,
            auto_pause: false,
            measure_rsptimes: false,
            xattr_size: 0,
            xattr_count: 0,
            record_ctime_size: false,
            await_timeout_sec: 5,
            top: dir.clone(),
            host: "test".into(),
        };
        assert_eq!(p_large.files_between_checks(), 10);
        p_large.total_sz_kb = 0;
        assert_eq!(p_large.files_between_checks(), 20);

        let _ = fs::remove_dir_all(&dir);
    }
}
