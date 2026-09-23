// Behavior-level port of md-workbench from https://github.com/hpc/ior (Julian
// Kunkl, HHU; GPL-2.0). MPI ranks become threads. The FIFO working-set model is
// preserved exactly: precreate populates N objects per data set, the benchmark
// phase repeatedly stats/reads/deletes the oldest object and recreates a new one
// (reader/writer ranks shifted by --offset), cleanup removes the remaining tail.

use crate::mdtest::content_pattern;
use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    /// stop each benchmark iteration after this many seconds (rank-specific progress unless --stonewall-wear-out)
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

    /// verify the data on read
    #[arg(short = 'X', long)]
    pub verify_read: bool,

    /// random seed for the data pattern (defaults to time-based)
    #[arg(short = 'G', long, default_value_t = 0)]
    pub random_seed: u64,

    /// iteration number to start with (skips precreate accordingly)
    #[arg(long, default_value_t = 0)]
    pub start_item: u64,

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
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn lat_json(v: &mut Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
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

// per-benchmark-iteration stonewall wear-out state: the first rank to hit the
// timer flips the flag, every rank contributes its position, and a brief settle
// emulates md-workbench's Allreduce(MAX) so all ranks stop at the same count
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

fn mdw_wait(runtime: f64, factor: f64) {
    let wait = runtime * factor;
    if wait <= 0.0 {
        return;
    }
    if wait < 0.01 {
        let end = Instant::now() + Duration::from_secs_f64(wait);
        while Instant::now() < end {}
    } else {
        thread::sleep(Duration::from_secs_f64(wait));
    }
}

fn run_precreate_rank(p: &Params, rank: u32, s: &mut PhaseStat, current_index: u64) -> io::Result<()> {
    for d in 0..p.dset_count {
        match fs::create_dir(p.dset(rank, d)) {
            Ok(()) => s.dset_create += 1,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                s.errors += 1;
                warn!("rank {rank}: cannot create dset {}: {e}", p.dset(rank, d).display());
                return Err(e);
            }
        }
    }
    for f in current_index..p.precreate {
        for d in 0..p.dset_count {
            let path = p.obj(rank, d, f);
            let t0 = Instant::now();
            let mut fh = OpenOptions::new().write(true).create(true).open(&path)?;
            let buf = content_pattern(p.seed, rank, f * p.dset_count + d, p.file_size);
            if fh.write(&buf)? == p.file_size {
                s.obj_create += 1;
            } else {
                s.errors += 1;
            }
            drop(fh);
            s.record(OpKind::Create, t0);
        }
    }
    Ok(())
}

fn run_benchmark_rank(
    p: &Params,
    rank: u32,
    s: &mut PhaseStat,
    current_index: &mut u64,
    wo: &WearOut,
) -> io::Result<()> {
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
            let read_rank =
                ((rank as i64 + size as i64).wrapping_sub((p.offset * (d + 1)) as i64)).rem_euclid(size as i64) as u32;
            let obj = p.obj(read_rank, d, prev_file);
            let t0 = Instant::now();
            match fs::metadata(&obj) {
                Ok(_) => s.obj_stat += 1,
                Err(_) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot stat {}", obj.display());
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
                    } else if p.verify_read
                        && p.file_size > 0
                        && scratch != content_pattern(p.seed, read_rank, prev_file * p.dset_count + d, p.file_size)
                    {
                        s.errors += 1;
                        warn!("rank {rank}: verify error on {}", obj.display());
                    } else {
                        s.obj_read += 1;
                    }
                }
                Err(_) => {
                    s.errors += 1;
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
                Ok(()) => {
                    s.obj_delete += 1;
                    s.record(OpKind::Delete, t0);
                }
                Err(e) => {
                    s.errors += 1;
                    warn!("rank {rank}: cannot delete {}: {e}", obj.display());
                }
            }
            let write_rank = ((rank as u64 + p.offset * (d + 1)) % size) as u32;
            let new_index = p.precreate + prev_file;
            let new_obj = p.obj(write_rank, d, new_index);
            let t0 = Instant::now();
            match OpenOptions::new().write(true).create(true).open(&new_obj) {
                Ok(mut fh) => {
                    let buf = content_pattern(p.seed, write_rank, new_index * p.dset_count + d, p.file_size);
                    if fh.write(&buf)? == p.file_size {
                        s.obj_create += 1;
                    } else {
                        s.errors += 1;
                    }
                }
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
        if armed && s.elapsed() >= p.stonewall_timer {
            if !p.stonewall_wear_out {
                info!("rank {rank} stonewall at {f} iterations ({:.2}s)", s.elapsed());
                break;
            }
            if !contributed {
                wo.armed.store(1, Ordering::SeqCst);
                wo.total.fetch_max(f + 1, Ordering::SeqCst);
                wo.contributors.fetch_add(1, Ordering::SeqCst);
                // block until every rank has either contributed its position or
                // left the loop, which is what md-workbench's blocking
                // Allreduce(MAX) guarantees
                while wo.contributors.load(Ordering::SeqCst) + wo.finished.load(Ordering::SeqCst) < p.threads as u64 {
                    thread::yield_now();
                }
                contributed = true;
            }
            let tot = wo.total.load(Ordering::SeqCst);
            if tot > 0 {
                total_num = tot;
            }
        }
    }
    wo.finished.fetch_add(1, Ordering::SeqCst);
    log::debug!(
        "rank {rank} benchmark done: f={f} total_num={total_num} wo_total={} contrib={} fin={}",
        wo.total.load(Ordering::SeqCst),
        wo.contributors.load(Ordering::SeqCst),
        wo.finished.load(Ordering::SeqCst)
    );
    if !p.read_only {
        *current_index += f;
    }
    s.t = s.elapsed();
    Ok(())
}

fn run_cleanup_rank(p: &Params, rank: u32, s: &mut PhaseStat, start_index: u64) -> io::Result<()> {
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
        match fs::remove_dir(p.dset(rank, d)) {
            Ok(()) => s.dset_delete += 1,
            Err(_) => s.dset_delete += 1, // md-workbench counts the attempt
        }
    }
    Ok(())
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
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
        warn!("benchmark phase with stonewall but without wear-out leaves files the cleanup phase cannot fully remove");
    }
    if args.obj_per_proc > args.precreate_per_set {
        warn!("obj-per-proc > precreate-per-set: objects to read may not be available");
    }
    let seed = if args.random_seed == 0 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    } else {
        args.random_seed
    };
    let p = Params {
        threads,
        num: args.obj_per_proc,
        precreate: args.precreate_per_set,
        dset_count: args.data_sets,
        file_size: args.object_size,
        iterations: args.iterations,
        offset: args.offset,
        prefix: mountpoint.join(&args.out_dir),
        waiting_factor: args.waiting_time,
        stonewall_timer: args.stonewall_timer,
        stonewall_wear_out: args.stonewall_wear_out,
        phase_precreate,
        phase_benchmark,
        phase_cleanup,
        read_only: args.read_only,
        verify_read: args.verify_read,
        seed,
        start_item: args.start_item,
        top: mountpoint.to_path_buf(),
    };
    let workingset_mib = threads as f64 * p.dset_count as f64 * p.precreate as f64 * p.file_size as f64 / 1048576.0;
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
    // deterministic phase registry: precreate, benchmark-<i>..., cleanup
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
    let slots: Vec<Mutex<Vec<(u32, PhaseStat)>>> = phase_names.iter().map(|_| Mutex::new(Vec::new())).collect();
    let shared = Arc::new(Shared {
        barrier: Barrier::new(threads as usize),
    });
    let wear_outs: Arc<Vec<Arc<WearOut>>> = Arc::new((0..p.iterations).map(|_| Arc::new(WearOut::new())).collect());
    let slots = Arc::new(slots);
    thread::scope(|s| {
        let mut joins = Vec::new();
        for rank in 0..threads {
            let p = &p;
            let shared = shared.clone();
            let slots = slots.clone();
            let wear_outs = wear_outs.clone();
            let names = &phase_names;
            joins.push(s.spawn(move || {
                let mut current_index = p.start_item;
                let mut slot = 0usize;
                if p.phase_precreate {
                    shared.barrier.wait();
                    let mut st = PhaseStat::default();
                    st.start();
                    if let Err(e) = run_precreate_rank(p, rank, &mut st, current_index) {
                        warn!("rank {rank} precreate failed: {e}");
                    }
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
                        if let Err(e) = run_benchmark_rank(p, rank, &mut st, &mut current_index, &wo) {
                            warn!("rank {} benchmark failed: {e}", names[slot]);
                        }
                        slots[slot].lock().unwrap().push((rank, st));
                        slot += 1;
                        // md-workbench's end_phase MPI_Reduce is an implicit sync;
                        // the next phase must not race the previous one's tail
                        shared.barrier.wait();
                    }
                }
                if p.phase_cleanup {
                    shared.barrier.wait();
                    let mut st = PhaseStat::default();
                    st.start();
                    if let Err(e) = run_cleanup_rank(p, rank, &mut st, current_index) {
                        warn!("rank {rank} cleanup failed: {e}");
                    }
                    st.t = st.elapsed();
                    slots[slot].lock().unwrap().push((rank, st));
                    shared.barrier.wait();
                    if rank == 0 {
                        let _ = fs::remove_dir(&p.prefix);
                    }
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
    });
    let mut phases_json = Vec::new();
    let mut total_errors = 0u64;
    for (name, slot) in phase_names.iter().zip(slots.iter()) {
        let mut entries: Vec<(u32, PhaseStat)> = slot.lock().unwrap().drain(..).collect();
        entries.sort_by_key(|(r, _)| *r);
        let mut ranks = Map::new();
        let mut agg = PhaseStat::default();
        let mut times = Vec::new();
        for (r, st) in &entries {
            times.push(st.t);
            agg.obj_create += st.obj_create;
            agg.obj_read += st.obj_read;
            agg.obj_stat += st.obj_stat;
            agg.obj_delete += st.obj_delete;
            agg.dset_create += st.dset_create;
            agg.dset_delete += st.dset_delete;
            agg.errors += st.errors;
            agg.max_op_time = agg.max_op_time.max(st.max_op_time);
            total_errors += st.errors;
            ranks.insert(
                r.to_string(),
                json!({
                    "t": st.t, "errors": st.errors,
                    "objCreate": st.obj_create, "objRead": st.obj_read,
                    "objStat": st.obj_stat, "objDelete": st.obj_delete,
                    "dsetCreate": st.dset_create, "dsetDelete": st.dset_delete,
                    "rateObjPerSec": if st.t > 0.0 { st.obj_read as f64 / st.t } else { 0.0 },
                    "opMaxSec": st.max_op_time,
                    "latencySec": {
                        "create": lat_json(&mut st.lat_create.clone()),
                        "read": lat_json(&mut st.lat_read.clone()),
                        "stat": lat_json(&mut st.lat_stat.clone()),
                        "delete": lat_json(&mut st.lat_delete.clone()),
                    },
                }),
            );
        }
        let t_max = times.iter().cloned().fold(0.0, f64::max);
        let t_mean = times.iter().sum::<f64>() / times.len().max(1) as f64;
        let t_min = times.iter().cloned().fold(f64::INFINITY, f64::min);
        let balance = if t_max > 0.0 { t_min / t_max * 100.0 } else { 0.0 };
        // md-workbench counts stat+read+delete+create as 4 ops per object
        let iops_per_obj = if name.starts_with("benchmark") && !p.read_only {
            4
        } else {
            1
        };
        let iops = agg.obj_read as f64 * iops_per_obj as f64 / t_max.max(1e-9);
        let rate = agg.obj_read as f64 / t_max.max(1e-9);
        let tp = agg.obj_read as f64 * p.file_size as f64 / 1048576.0 / t_max.max(1e-9);
        info!(
            "{name} process max:{t_max:.2}s mean:{t_mean:.2}s balance:{balance:.1}% iops:{iops:.1}/s objects:{} rate:{rate:.1} obj/s tp:{tp:.1}MiB/s op-max:{:.3e}s errors:{}",
            agg.obj_read, agg.max_op_time, agg.errors
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
    let report = json!({
        "suite": "mdworkbench",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "params": {
            "threads": p.threads, "obj_per_proc": p.num, "precreate_per_set": p.precreate,
            "data_sets": p.dset_count, "object_size": p.file_size, "iterations": p.iterations,
            "offset": p.offset, "out_dir": args.out_dir, "waiting_factor": p.waiting_factor,
            "stonewall_timer": p.stonewall_timer, "stonewall_wear_out": p.stonewall_wear_out,
            "read_only": p.read_only, "verify_read": p.verify_read, "seed": p.seed,
            "working_set_mib": workingset_mib,
        },
        "status": if total_errors > 0 { "failed" } else { "ok" },
        "errors": total_errors,
        "phases": phases_json,
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}
