// Single-node threaded port of md-workbench (https://github.com/hpc/ior, GPL-2.0).

use crate::common::{fsync_raw, iso8601};
use crate::mdtest::content_pattern;
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STATUS_FILE: &str = "md-workbench.status";

#[derive(clap::Args)]
pub struct Args {
    /// number of threads (ranks)
    #[arg(long, default_value_t = 1)]
    pub threads: u32,

    /// number of I/O operations per data set in the benchmark phase
    #[arg(short = 'I', long, default_value_t = 1000)]
    pub obj_per_proc: u64,

    /// number of objects to precreate per data set
    #[arg(short = 'P', long, default_value_t = 3000)]
    pub precreate_per_set: u64,

    /// number of data sets per rank
    #[arg(short = 'D', long, default_value_t = 10)]
    pub data_sets: u64,

    /// object size in bytes
    #[arg(short = 'S', long, default_value_t = 3901)]
    pub object_size: usize,

    /// number of times to rerun the benchmark phase
    #[arg(short = 'R', long, default_value_t = 3)]
    pub iterations: u32,

    /// rank offset between readers and writers (1 makes each rank read its neighbor's objects)
    #[arg(short = 'O', long, default_value_t = 1)]
    pub offset: u64,

    /// working directory under the mount point (data sets live in <dir>/<rank>_<d>)
    #[arg(short = 'o', long, default_value = "out")]
    pub out_dir: String,

    /// waiting time relative to op runtime (1.0 = 100%); throttles the benchmark phase
    #[arg(short = 't', long, default_value_t = 0.0)]
    pub waiting_time: f64,

    /// stop each benchmark iteration after this many seconds
    #[arg(short = 'w', long, default_value_t = 0.0)]
    pub stonewall_timer: f64,

    /// with --stonewall-timer: all threads perform the same number of iterations
    #[arg(short = 'W', long)]
    pub stonewall_wear_out: bool,

    /// run only the precreate phase
    #[arg(short = '1', long)]
    pub run_precreate: bool,

    /// run only the benchmark phase
    #[arg(short = '2', long)]
    pub run_benchmark: bool,

    /// run only the cleanup phase
    #[arg(short = '3', long)]
    pub run_cleanup: bool,

    /// benchmark phase only stats/reads (no deletes/writes)
    #[arg(long)]
    pub read_only: bool,

    /// fsync each created object
    #[arg(short = 'y', long)]
    pub fsync: bool,

    /// verify the data on read
    #[arg(short = 'X', long)]
    pub verify_read: bool,

    /// random seed for the data pattern (defaults to persisted or time-based)
    #[arg(short = 'G', long, default_value_t = 0)]
    pub random_seed: u64,

    /// iteration number to start with (skips precreate accordingly)
    #[arg(long, default_value_t = 0)]
    pub start_item: u64,

    /// verbose debug logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

#[derive(Default)]
struct PhaseStat {
    phase_start: Option<Instant>,
    dset_create: u64,
    dset_delete: u64,
    obj_create: u64,
    obj_read: u64,
    obj_stat: u64,
    obj_delete: u64,
    errors: u64,
    t: f64,
    max_op_time: f64,
    lat_create: Vec<f64>,
    lat_read: Vec<f64>,
    lat_stat: Vec<f64>,
    lat_delete: Vec<f64>,
}

enum OpKind {
    Create,
    Read,
    Stat,
    Delete,
}

impl PhaseStat {
    fn start(&mut self) {
        self.phase_start = Some(Instant::now());
    }
    fn elapsed(&self) -> f64 {
        self.phase_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
    }
    fn record(&mut self, kind: OpKind, t0: Instant) {
        let dt = t0.elapsed().as_secs_f64();
        if dt > self.max_op_time {
            self.max_op_time = dt;
        }
        match kind {
            OpKind::Create => self.lat_create.push(dt),
            OpKind::Read => self.lat_read.push(dt),
            OpKind::Stat => self.lat_stat.push(dt),
            OpKind::Delete => self.lat_delete.push(dt),
        }
    }
}

struct Params {
    threads: u32,
    num: u64,
    precreate: u64,
    dset_count: u64,
    file_size: usize,
    iterations: u32,
    offset: u64,
    prefix: PathBuf,
    waiting_factor: f64,
    stonewall_timer: f64,
    stonewall_wear_out: bool,
    phase_precreate: bool,
    phase_benchmark: bool,
    phase_cleanup: bool,
    read_only: bool,
    fsync: bool,
    verify_read: bool,
    seed: u64,
    start_item: u64,
    top: PathBuf,
}

impl Params {
    fn dset(&self, rank: u32, d: u64) -> PathBuf {
        self.prefix.join(format!("{rank}_{d}"))
    }
    fn obj(&self, rank: u32, d: u64, i: u64) -> PathBuf {
        self.dset(rank, d).join(format!("file-{i}"))
    }
    fn status_path(&self) -> PathBuf {
        self.prefix.join(STATUS_FILE)
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn lat_json(v: &mut [f64]) -> Value {
    v.sort_by(|a, b| a.total_cmp(b));
    json!({
        "count": v.len(),
        "mean": if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 },
        "min": v.first().copied().unwrap_or(0.0),
        "max": v.last().copied().unwrap_or(0.0),
        "p50": percentile(v, 0.50),
        "p90": percentile(v, 0.90),
        "p99": percentile(v, 0.99),
    })
}

struct Shared {
    barrier: Barrier,
}

struct WearOut {
    armed: AtomicU64,
    total: AtomicU64,
    contributors: AtomicU64,
    finished: AtomicU64,
}

impl WearOut {
    fn new() -> Self {
        WearOut {
            armed: AtomicU64::new(0),
            total: AtomicU64::new(0),
            contributors: AtomicU64::new(0),
            finished: AtomicU64::new(0),
        }
    }
}

struct FinishGuard<'a>(&'a WearOut);

impl Drop for FinishGuard<'_> {
    fn drop(&mut self) {
        self.0.finished.fetch_add(1, Ordering::SeqCst);
    }
}

fn mdw_wait(runtime: f64, factor: f64) {
    let wait = runtime * factor;
    if wait <= 0.0 {
        return;
    }
    if wait < 0.01 {
        let end = Instant::now() + Duration::from_secs_f64(wait);
        while Instant::now() < end {
            thread::yield_now();
        }
    } else {
        thread::sleep(Duration::from_secs_f64(wait));
    }
}

fn create_object(p: &Params, path: &Path, writer_rank: u32, d: u64, index: u64) -> io::Result<()> {
    let mut fh = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    if p.file_size > 0 {
        let buf = content_pattern(p.seed, writer_rank, index * p.dset_count + d, p.file_size);
        fh.write_all(&buf)?;
    }
    if p.fsync {
        fsync_raw(&fh)?;
    }
    Ok(())
}

fn run_precreate_rank(p: &Params, rank: u32, s: &mut PhaseStat, current_index: u64) {
    for d in 0..p.dset_count {
        match fs::create_dir(p.dset(rank, d)) {
            Ok(()) => s.dset_create += 1,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                s.errors += 1;
                warn!("rank {rank}: cannot create dset {}: {e}", p.dset(rank, d).display());
                return;
            }
        }
    }
    for f in current_index..p.precreate {
        for d in 0..p.dset_count {
            let path = p.obj(rank, d, f);
            let t0 = Instant::now();
            match create_object(p, &path, rank, d, f) {
                Ok(()) => s.obj_create += 1,
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot precreate {}: {e}", path.display());
                }
            }
            s.record(OpKind::Create, t0);
        }
    }
}

fn run_benchmark_rank(
    p: &Params,
    rank: u32,
    s: &mut PhaseStat,
    current_index: &mut u64,
    wo: &WearOut,
) {
    let _finish_guard = FinishGuard(wo);
    let mut scratch = vec![0u8; p.file_size];
    let size = p.threads as u64;
    let start_index = *current_index;
    let mut total_num = p.num;
    let armed = p.stonewall_timer > 0.0;
    let mut contributed = false;
    let mut f: u64 = 0;

    while f < total_num {
        for d in 0..p.dset_count {
            let prev_file = f + start_index;
            let read_rank = ((rank as i64 + size as i64)
                .wrapping_sub((p.offset * (d + 1)) as i64))
            .rem_euclid(size as i64) as u32;
            let obj = p.obj(read_rank, d, prev_file);

            let t0 = Instant::now();
            match fs::metadata(&obj) {
                Ok(_) => s.obj_stat += 1,
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot stat {}: {e}", obj.display());
                    s.record(OpKind::Stat, t0);
                    continue;
                }
            }
            s.record(OpKind::Stat, t0);
            if p.waiting_factor > 0.0 {
                mdw_wait(t0.elapsed().as_secs_f64(), p.waiting_factor);
            }

            let t0 = Instant::now();
            match File::open(&obj) {
                Ok(mut fh) => {
                    if p.file_size > 0 && fh.read_exact(&mut scratch).is_err() {
                        s.errors += 1;
                        warn!("rank {rank}: short/failed read on {}", obj.display());
                    } else if p.verify_read
                        && p.file_size > 0
                        && scratch
                            != content_pattern(p.seed, read_rank, prev_file * p.dset_count + d, p.file_size)
                    {
                        s.errors += 1;
                        warn!("rank {rank}: verify error on {}", obj.display());
                    } else {
                        s.obj_read += 1;
                    }
                }
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot open {}: {e}", obj.display());
                }
            }
            s.record(OpKind::Read, t0);
            if p.waiting_factor > 0.0 {
                mdw_wait(t0.elapsed().as_secs_f64(), p.waiting_factor);
            }
            if p.read_only {
                continue;
            }

            let t0 = Instant::now();
            match fs::remove_file(&obj) {
                Ok(()) => s.obj_delete += 1,
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot delete {}: {e}", obj.display());
                }
            }
            s.record(OpKind::Delete, t0);

            let write_rank = ((rank as u64 + p.offset * (d + 1)) % size) as u32;
            let new_index = p.precreate + prev_file;
            let new_obj = p.obj(write_rank, d, new_index);
            let t0 = Instant::now();
            match create_object(p, &new_obj, write_rank, d, new_index) {
                Ok(()) => s.obj_create += 1,
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot create {}: {e}", new_obj.display());
                }
            }
            s.record(OpKind::Create, t0);
            if p.waiting_factor > 0.0 {
                mdw_wait(t0.elapsed().as_secs_f64(), p.waiting_factor);
            }
        }

        f += 1;
        if armed {
            if !p.stonewall_wear_out {
                if s.elapsed() >= p.stonewall_timer {
                    info!("rank {rank} stonewall at {f} iterations ({:.2}s)", s.elapsed());
                    break;
                }
            } else if wo.armed.load(Ordering::SeqCst) != 0 || s.elapsed() >= p.stonewall_timer {
                if !contributed {
                    wo.armed.store(1, Ordering::SeqCst);
                    wo.total.fetch_max(f.min(p.num), Ordering::SeqCst);
                    wo.contributors.fetch_add(1, Ordering::SeqCst);
                    while wo.contributors.load(Ordering::SeqCst) + wo.finished.load(Ordering::SeqCst)
                        < p.threads as u64
                    {
                        thread::yield_now();
                    }
                    contributed = true;
                }
                let tot = wo.total.load(Ordering::SeqCst);
                if tot > 0 {
                    total_num = tot.min(p.num);
                }
            }
        }
    }

    if armed && p.stonewall_wear_out && !contributed {
        wo.total.fetch_max(f.min(p.num), Ordering::SeqCst);
    }
    if !p.read_only {
        *current_index += f;
    }
    s.t = s.elapsed();
}

fn run_cleanup_rank(p: &Params, rank: u32, s: &mut PhaseStat, start_index: u64) {
    for d in 0..p.dset_count {
        for f in 0..p.precreate {
            let path = p.obj(rank, d, f + start_index);
            let t0 = Instant::now();
            match fs::remove_file(&path) {
                Ok(()) => {
                    s.obj_delete += 1;
                    s.record(OpKind::Delete, t0);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot delete {}: {e}", path.display());
                }
            }
        }
        let dset_path = p.dset(rank, d);
        match fs::remove_dir(&dset_path) {
            Ok(()) => s.dset_delete += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                s.errors += 1;
                warn!("rank {rank}: cannot rmdir {}: {e}", dset_path.display());
            }
        }
    }
}

fn read_status_file(path: &Path) -> Option<(u64, u64)> {
    let raw = fs::read_to_string(path).ok()?;
    let mut parts = raw.split_whitespace();
    let idx = parts.next()?.parse::<u64>().ok()?;
    let seed = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    Some((idx, seed))
}

fn phase_rates(name: &str, read_only: bool, file_size: usize, st: &PhaseStat, dt: f64) -> (u64, f64, f64, f64) {
    if dt <= 0.0 {
        return (0, 0.0, 0.0, 0.0);
    }
    let mib = 1_048_576.0;
    if name == "precreate" {
        let objs = st.obj_create;
        let rate = objs as f64 / dt;
        let iops = (st.dset_create + st.obj_create) as f64 / dt;
        let tp = objs as f64 * file_size as f64 / mib / dt;
        (objs, rate, iops, tp)
    } else if name == "cleanup" {
        let objs = st.obj_delete;
        let rate = objs as f64 / dt;
        let iops = (st.obj_delete + st.dset_delete) as f64 / dt;
        (objs, rate, iops, 0.0)
    } else if read_only {
        let objs = st.obj_read;
        let rate = objs as f64 / dt;
        let iops = (st.obj_stat + st.obj_read) as f64 / dt;
        let tp = objs as f64 * file_size as f64 / mib / dt;
        (objs, rate, iops, tp)
    } else {
        let objs = st.obj_read;
        let rate = objs as f64 / dt;
        let iops = (st.obj_stat + st.obj_read + st.obj_delete + st.obj_create) as f64 / dt;
        let tp = (st.obj_read + st.obj_create) as f64 * file_size as f64 / mib / dt;
        (objs, rate, iops, tp)
    }
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let t_suite = Instant::now();
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let threads = args.threads.max(1);
    let (phase_precreate, phase_benchmark, phase_cleanup) =
        if !args.run_precreate && !args.run_benchmark && !args.run_cleanup {
            (true, true, true)
        } else {
            (args.run_precreate, args.run_benchmark, args.run_cleanup)
        };
    if phase_benchmark && !phase_precreate && args.stonewall_timer > 0.0 && !args.stonewall_wear_out {
        warn!("benchmark phase with stonewall without --stonewall-wear-out may leave unaligned tail files");
    }
    if args.obj_per_proc > args.precreate_per_set {
        warn!("obj-per-proc > precreate-per-set: objects to read may not be available");
    }

    let prefix = mountpoint.join(&args.out_dir);
    let persisted = if !phase_precreate {
        read_status_file(&prefix.join(STATUS_FILE))
    } else {
        None
    };
    let start_item = if args.start_item > 0 {
        args.start_item
    } else {
        persisted.map(|(idx, _)| idx).unwrap_or(0)
    };
    let seed = if args.random_seed != 0 {
        args.random_seed
    } else if let Some((_, s)) = persisted
        && s != 0
    {
        s
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            | 1
    };

    let p = Params {
        threads,
        num: args.obj_per_proc,
        precreate: args.precreate_per_set,
        dset_count: args.data_sets,
        file_size: args.object_size,
        iterations: args.iterations,
        offset: args.offset,
        prefix,
        waiting_factor: args.waiting_time,
        stonewall_timer: args.stonewall_timer,
        stonewall_wear_out: args.stonewall_wear_out,
        phase_precreate,
        phase_benchmark,
        phase_cleanup,
        read_only: args.read_only,
        fsync: args.fsync,
        verify_read: args.verify_read,
        seed,
        start_item,
        top: mountpoint.to_path_buf(),
    };

    let workingset_mib =
        threads as f64 * p.dset_count as f64 * p.precreate as f64 * p.file_size as f64 / 1_048_576.0;
    info!(
        "mdworkbench: top={} threads={} dsets={} precreate={} num={} size={}B iterations={} offset={} workingset={:.1}MiB",
        p.top.display(),
        p.threads,
        p.dset_count,
        p.precreate,
        p.num,
        p.file_size,
        p.iterations,
        p.offset,
        workingset_mib
    );
    if p.phase_precreate {
        fs::create_dir_all(&p.prefix).map_err(|e| e.to_string())?;
    }

    let mut phase_names: Vec<String> = Vec::new();
    if p.phase_precreate {
        phase_names.push("precreate".into());
    }
    if p.phase_benchmark {
        for i in 0..p.iterations {
            phase_names.push(format!("benchmark-{i}"));
        }
    }
    if p.phase_cleanup {
        phase_names.push("cleanup".into());
    }
    let slots: Arc<Vec<Mutex<Vec<(u32, PhaseStat)>>>> =
        Arc::new(phase_names.iter().map(|_| Mutex::new(Vec::new())).collect());
    let shared = Arc::new(Shared {
        barrier: Barrier::new(threads as usize),
    });
    let wear_outs: Arc<Vec<Arc<WearOut>>> =
        Arc::new((0..p.iterations).map(|_| Arc::new(WearOut::new())).collect());
    let final_index = AtomicU64::new(p.start_item);

    thread::scope(|s| {
        let mut joins = Vec::new();
        for rank in 0..threads {
            let p = &p;
            let shared = shared.clone();
            let slots = slots.clone();
            let wear_outs = wear_outs.clone();
            let final_idx = &final_index;
            joins.push(s.spawn(move || {
                let mut current_index = p.start_item;
                let mut slot = 0usize;
                if p.phase_precreate {
                    shared.barrier.wait();
                    let mut st = PhaseStat::default();
                    st.start();
                    run_precreate_rank(p, rank, &mut st, current_index);
                    st.t = st.elapsed();
                    slots[slot].lock().unwrap().push((rank, st));
                    slot += 1;
                    shared.barrier.wait();
                }
                if p.phase_benchmark {
                    for it in 0..p.iterations {
                        shared.barrier.wait();
                        let mut st = PhaseStat::default();
                        st.start();
                        let wo = wear_outs[it as usize].clone();
                        run_benchmark_rank(p, rank, &mut st, &mut current_index, &wo);
                        slots[slot].lock().unwrap().push((rank, st));
                        slot += 1;
                        shared.barrier.wait();
                    }
                }
                if rank == 0 {
                    final_idx.store(current_index, Ordering::SeqCst);
                }
                if p.phase_cleanup {
                    shared.barrier.wait();
                    let mut st = PhaseStat::default();
                    st.start();
                    run_cleanup_rank(p, rank, &mut st, current_index);
                    st.t = st.elapsed();
                    slots[slot].lock().unwrap().push((rank, st));
                    shared.barrier.wait();
                    if rank == 0 {
                        let _ = fs::remove_file(p.status_path());
                        let _ = fs::remove_dir(&p.prefix);
                    }
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
    });

    if !p.phase_cleanup && p.prefix.is_dir() {
        let status_body = format!("{} {}\n", final_index.load(Ordering::SeqCst), p.seed);
        if let Err(e) = fs::write(p.status_path(), status_body) {
            warn!("failed to persist {}: {e}", p.status_path().display());
        }
    }

    let mut phases_json = Vec::new();
    let mut total_errors = 0u64;
    for (name, slot) in phase_names.iter().zip(slots.iter()) {
        let mut entries: Vec<(u32, PhaseStat)> = slot.lock().unwrap().drain(..).collect();
        entries.sort_by_key(|(r, _)| *r);
        let mut ranks = Map::new();
        let mut agg = PhaseStat::default();
        let mut times = Vec::new();
        for (r, st) in &mut entries {
            times.push(st.t);
            agg.obj_create += st.obj_create;
            agg.obj_read += st.obj_read;
            agg.obj_stat += st.obj_stat;
            agg.obj_delete += st.obj_delete;
            agg.dset_create += st.dset_create;
            agg.dset_delete += st.dset_delete;
            agg.errors += st.errors;
            agg.max_op_time = agg.max_op_time.max(st.max_op_time);
            agg.lat_create.extend_from_slice(&st.lat_create);
            agg.lat_read.extend_from_slice(&st.lat_read);
            agg.lat_stat.extend_from_slice(&st.lat_stat);
            agg.lat_delete.extend_from_slice(&st.lat_delete);
            total_errors += st.errors;

            let (_, rank_rate, _, _) = phase_rates(name, p.read_only, p.file_size, st, st.t);
            ranks.insert(
                r.to_string(),
                json!({
                    "t": st.t,
                    "errors": st.errors,
                    "objCreate": st.obj_create,
                    "objRead": st.obj_read,
                    "objStat": st.obj_stat,
                    "objDelete": st.obj_delete,
                    "dsetCreate": st.dset_create,
                    "dsetDelete": st.dset_delete,
                    "rateObjPerSec": rank_rate,
                    "opMaxSec": st.max_op_time,
                    "latencySec": {
                        "create": lat_json(&mut st.lat_create),
                        "read": lat_json(&mut st.lat_read),
                        "stat": lat_json(&mut st.lat_stat),
                        "delete": lat_json(&mut st.lat_delete),
                    },
                }),
            );
        }
        let t_max = times.iter().cloned().fold(0.0, f64::max);
        let t_mean = times.iter().sum::<f64>() / times.len().max(1) as f64;
        let t_min = times.iter().cloned().fold(f64::INFINITY, f64::min);
        let balance = if t_max > 0.0 { t_min / t_max * 100.0 } else { 0.0 };
        let (objs, rate, iops, tp) = phase_rates(name, p.read_only, p.file_size, &agg, t_max);
        info!(
            "{name} process max:{t_max:.2}s mean:{t_mean:.2}s balance:{balance:.1}% iops:{iops:.1}/s objects:{objs} rate:{rate:.1} obj/s tp:{tp:.1}MiB/s op-max:{:.3e}s errors:{}",
            agg.max_op_time, agg.errors
        );
        phases_json.push(json!({
            "name": name,
            "aggregate": {
                "t": t_max, "tMean": t_mean, "balancePct": balance,
                "objCreate": agg.obj_create, "objRead": agg.obj_read,
                "objStat": agg.obj_stat, "objDelete": agg.obj_delete,
                "dsetCreate": agg.dset_create, "dsetDelete": agg.dset_delete,
                "iops": iops, "rateObjPerSec": rate,
                "throughputMiBps": tp, "opMaxSec": agg.max_op_time, "errors": agg.errors,
                "latencySec": {
                    "create": lat_json(&mut agg.lat_create),
                    "read": lat_json(&mut agg.lat_read),
                    "stat": lat_json(&mut agg.lat_stat),
                    "delete": lat_json(&mut agg.lat_delete),
                },
            },
            "rank": ranks,
        }));
    }

    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    Ok(json!({
        "suite": "mdworkbench",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "durationMs": t_suite.elapsed().as_millis() as u64,
        "top": mountpoint.display().to_string(),
        "params": {
            "threads": p.threads, "obj_per_proc": p.num, "precreate_per_set": p.precreate,
            "data_sets": p.dset_count, "object_size": p.file_size, "iterations": p.iterations,
            "offset": p.offset, "out_dir": args.out_dir, "waiting_factor": p.waiting_factor,
            "stonewall_timer": p.stonewall_timer, "stonewall_wear_out": p.stonewall_wear_out,
            "read_only": p.read_only, "fsync": p.fsync, "verify_read": p.verify_read,
            "seed": p.seed, "start_item": p.start_item, "working_set_mib": workingset_mib,
        },
        "status": if total_errors > 0 { "failed" } else { "ok" },
        "errors": total_errors,
        "phases": phases_json,
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

    fn parse_args(itr: &[&str]) -> Args {
        let mut full = vec!["mdworkbench"];
        full.extend_from_slice(itr);
        TestCli::parse_from(full).args
    }

    fn make_temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fstest-mdwb-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn latencies_and_phase_metrics_populated_accurately() {
        let dir = make_temp_dir("lat");
        let res = run(
            &dir,
            &parse_args(&[
                "--threads",
                "2",
                "-D",
                "2",
                "-P",
                "10",
                "-I",
                "5",
                "-S",
                "1024",
                "-R",
                "1",
                "-X",
                "-y",
            ]),
        )
        .unwrap();
        assert_eq!(res["status"], "ok");
        assert_eq!(res["errors"], 0);

        let phases = res["phases"].as_array().unwrap();
        assert_eq!(phases.len(), 3);

        let pre = &phases[0]["aggregate"];
        assert!(pre["iops"].as_f64().unwrap() > 0.0);
        assert!(pre["rateObjPerSec"].as_f64().unwrap() > 0.0);
        assert!(pre["throughputMiBps"].as_f64().unwrap() > 0.0);
        assert_eq!(pre["latencySec"]["create"]["count"], 40);
        assert!(pre["latencySec"]["create"]["p99"].as_f64().unwrap() > 0.0);

        let bench = &phases[1]["aggregate"];
        let rate = bench["rateObjPerSec"].as_f64().unwrap();
        let iops = bench["iops"].as_f64().unwrap();
        assert!((iops - rate * 4.0).abs() < 1e-3);
        assert_eq!(bench["latencySec"]["read"]["count"], 20);
        assert!(bench["latencySec"]["read"]["p99"].as_f64().unwrap() > 0.0);

        let clean = &phases[2]["aggregate"];
        assert!(clean["iops"].as_f64().unwrap() > 0.0);
        assert_eq!(clean["throughputMiBps"].as_f64().unwrap(), 0.0);
        assert!(!dir.join("out").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_phases_persist_status_and_read_only_doubles_iops() {
        let dir = make_temp_dir("split");
        let p1 = run(
            &dir,
            &parse_args(&["--threads", "2", "-D", "2", "-P", "8", "-I", "4", "-S", "256", "-1"]),
        )
        .unwrap();
        assert_eq!(p1["status"], "ok");

        let ro = run(
            &dir,
            &parse_args(&[
                "--threads",
                "2",
                "-D",
                "2",
                "-P",
                "8",
                "-I",
                "4",
                "-S",
                "256",
                "-R",
                "1",
                "--read-only",
                "-X",
                "-2",
            ]),
        )
        .unwrap();
        assert_eq!(ro["status"], "ok");
        let ro_agg = &ro["phases"][0]["aggregate"];
        assert!((ro_agg["iops"].as_f64().unwrap() - ro_agg["rateObjPerSec"].as_f64().unwrap() * 2.0).abs() < 1e-3);

        let rw = run(
            &dir,
            &parse_args(&[
                "--threads",
                "2",
                "-D",
                "2",
                "-P",
                "8",
                "-I",
                "4",
                "-S",
                "256",
                "-R",
                "1",
                "-X",
                "-2",
            ]),
        )
        .unwrap();
        assert_eq!(rw["status"], "ok");

        let p3 = run(
            &dir,
            &parse_args(&["--threads", "2", "-D", "2", "-P", "8", "-I", "4", "-S", "256", "-3"]),
        )
        .unwrap();
        assert_eq!(p3["status"], "ok");
        assert_eq!(p3["errors"], 0);
        assert!(!dir.join("out").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
