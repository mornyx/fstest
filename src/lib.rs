pub mod bundle;
pub mod common;
pub mod errno_diff;
pub mod fio;
pub mod fsmark;
pub mod fsrand;
pub mod fsstress;
pub mod fsx;
pub mod git;
pub mod ltp;
pub mod mdtest;
pub mod mdworkbench;
pub mod pjdfstest;
pub mod smallfile;
pub mod sqlite;
pub mod stdfs;

use clap::{Parser, Subcommand};
use common::{MountSnapshot, enrich_report};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const HIDDEN_HELPER_MODES: &[&str] = &["__pjdfstest", "__sqlite-mptest", "__sqlite-shm-child", "__stdfs-rust"];

/// Check whether `argv_after_bin` begins with one of fstest's internal child-helper
/// modes (`__pjdfstest`, `__sqlite-mptest`, `__sqlite-shm-child`, `__stdfs-rust`) and execute it if so.
pub fn try_run_helper_mode(argv_after_bin: &[String]) -> Option<i32> {
    match argv_after_bin.first().map(String::as_str) {
        Some("__pjdfstest") => Some(pjdfstest::helper_main(&argv_after_bin[1..])),
        Some("__sqlite-mptest") => Some(sqlite::mptest_main(&argv_after_bin[1..])),
        Some("__sqlite-shm-child") => Some(sqlite::shm_child_main(&argv_after_bin[1..])),
        Some("__stdfs-rust") => Some(stdfs::rust_helper_main(&argv_after_bin[1..])),
        _ => None,
    }
}

#[derive(Parser)]
#[command(
    name = "fstest",
    version,
    about = "All-in-one filesystem functional & benchmark test suites",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// smallfile suite: POSIX metadata/data workload (port of smallfile)
    Smallfile {
        /// mount point / top directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: smallfile::Args,
    },

    /// fsmark suite: synchronous/async file creation benchmark (port of fs_mark)
    Fsmark {
        /// mount point / first test directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: fsmark::Args,
    },

    /// fio suite: pure pass-through to embedded upstream `axboe/fio` (or 5-phase matrix when called with `<MOUNTPOINT>` only)
    #[command(disable_help_flag = true, trailing_var_arg = true)]
    Fio {
        /// native `fio` options/jobfiles plus `<MOUNTPOINT>`
        #[arg(allow_hyphen_values = true)]
        argv: Vec<OsString>,
    },

    /// mdtest suite: metadata rate benchmark (port of mdtest, single node)
    Mdtest {
        /// mount point / top directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: mdtest::Args,
    },

    /// mdworkbench suite: steady-state metadata benchmark with a fixed working set (port of md-workbench)
    Mdworkbench {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: mdworkbench::Args,
    },

    /// ltp suite: Linux Test Project adapter (syscalls/fs/fs_perms_simple/fcntl-locktests)
    Ltp {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: ltp::Args,
    },

    /// fsstress suite: randomized directory-tree metadata+data stress (port of xfstests fsstress)
    Fsstress {
        /// mount point / base directory for operations
        mountpoint: PathBuf,

        #[command(flatten)]
        args: fsstress::Args,
    },

    /// fsx suite: single-file data integrity fuzzer (port of xfstests fsx)
    Fsx {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: fsx::Args,
    },

    /// fsrand suite: deterministic shadow-oracle tree fuzzer, random pwrite verifier, and concurrent rename stress
    Fsrand {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: fsrand::Args,
    },

    /// errno-diff suite: differential POSIX syscall errno & divergence contract probe against a local reference FS
    ErrnoDiff {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: errno_diff::Args,
    },

    /// pjdfstest suite: POSIX conformance functional tests (embedded upstream .t suite)
    Pjdfstest {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: pjdfstest::Args,
    },

    /// stdfs suite: Go/Python/Node/Rust standard-library fs tests run against the mount
    Stdfs {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: stdfs::Args,
    },

    /// git suite: git's own t/ test suite run against the mount
    Git {
        /// mount point / directory under test
        #[arg(required_unless_present = "list")]
        mountpoint: Option<PathBuf>,

        #[command(flatten)]
        args: git::Args,
    },

    /// sqlite suite: SQLite mptest (WAL + rollback) + crud + speedtest + kvtest + walthread + wal-lifecycle + mmap probes
    Sqlite {
        /// mount point / directory under test
        #[arg(required_unless_present = "list")]
        mountpoint: Option<PathBuf>,

        #[command(flatten)]
        args: sqlite::Args,
    },

    /// bundle management: inspect or extract the compile-time embedded external suite assets
    Bundle {
        #[command(subcommand)]
        cmd: bundle::BundleCmd,
    },
}

fn init_log(verbose: bool) {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose {
        "debug"
    } else {
        "info"
    }))
    .try_init();
}

/// Entrypoint accepting a full argv iterator (including `argv[0]`). Returns process exit code.
pub fn cli_main_from<I, T>(itr: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let raw: Vec<OsString> = itr.into_iter().map(Into::into).collect();
    let tail_strings: Vec<String> = raw.iter().skip(1).map(|s| s.to_string_lossy().into_owned()).collect();
    if let Some(code) = try_run_helper_mode(&tail_strings) {
        return code;
    }

    let cli = match Cli::try_parse_from(raw) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };

    match cli.command {
        Command::Smallfile { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = smallfile::run(&mountpoint, &args).map_err(|e| e.to_string());
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Fsmark { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = fsmark::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Fio { argv } => {
            init_log(false);
            match fio::parse_invocation(&argv) {
                Ok(fio::ParsedInvocation::InfoHandled(code)) => code,
                Ok(fio::ParsedInvocation::Run {
                    mountpoint,
                    json_out,
                    fio_args,
                }) => {
                    let snap = MountSnapshot::capture(&mountpoint);
                    let report = fio::run(&mountpoint, &fio_args);
                    finish(report, snap.as_ref(), json_out.as_deref())
                }
                Err(e) => {
                    log::error!("{e}");
                    1
                }
            }
        }
        Command::Mdtest { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = mdtest::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Mdworkbench { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = mdworkbench::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Ltp { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = ltp::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Fsstress { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = fsstress::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Fsx { mountpoint, args } => {
            init_log(args.debug);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = fsx::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Fsrand { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = fsrand::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::ErrnoDiff { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = errno_diff::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Pjdfstest { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = pjdfstest::run(&mountpoint, &args).map_err(|e| e.to_string());
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Stdfs { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = stdfs::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Git { mountpoint, args } => {
            init_log(false);
            if args.list {
                return match git::list(&args) {
                    Ok(()) => 0,
                    Err(e) => {
                        log::error!("{e}");
                        1
                    }
                };
            }
            let Some(mountpoint) = mountpoint else {
                log::error!("mount point required (or pass --list to show the selected scripts)");
                return 1;
            };
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = git::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Sqlite { mountpoint, args } => {
            init_log(false);
            if args.list {
                sqlite::list(&args);
                return 0;
            }
            let Some(mountpoint) = mountpoint else {
                log::error!("mount point required (or pass --list to show the embedded scripts)");
                return 1;
            };
            let json_path = args.json.clone();
            let snap = MountSnapshot::capture(&mountpoint);
            let report = sqlite::run(&mountpoint, &args);
            finish(report, snap.as_ref(), json_path.as_deref())
        }
        Command::Bundle { cmd } => {
            init_log(false);
            match bundle::run(&cmd) {
                Ok(report) => {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                    if report["status"] == serde_json::json!("ok") {
                        0
                    } else {
                        1
                    }
                }
                Err(e) => {
                    log::error!("{e}");
                    1
                }
            }
        }
    }
}

fn finish(report: Result<serde_json::Value, String>, snap: Option<&MountSnapshot>, json_path: Option<&Path>) -> i32 {
    match report {
        Ok(mut report) => {
            enrich_report(&mut report, snap);
            if let Some(path) = json_path
                && let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap())
            {
                log::error!("failed to write {}: {e}", path.display());
            }
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if report["status"] == serde_json::json!("ok") {
                0
            } else {
                1
            }
        }
        Err(e) => {
            log::error!("{e}");
            1
        }
    }
}
