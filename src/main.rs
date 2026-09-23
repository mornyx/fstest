mod fio;
mod fsmark;
mod fsstress;
mod fsx;
mod ltp;
mod mdtest;
mod mdworkbench;
mod pjdfstest;
mod smallfile;
mod stdfs;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "fstest",
    about = "Filesystem functional & benchmark test suites",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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

    /// fio suite: raw IO throughput/latency benchmark (wraps the system fio binary)
    Fio {
        /// mount point / directory under test
        mountpoint: PathBuf,

        #[command(flatten)]
        args: fio::Args,
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

    /// ltp suite: Linux Test Project adapter (syscalls/fs_bind/fs_perms/smoketest/locks)
    Ltp {
        /// mount point / directory under test
        #[arg(required_unless_present = "prepare_script")]
        mountpoint: Option<PathBuf>,

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
}

fn init_log(verbose: bool) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose { "debug" } else { "info" }))
        .init();
}

fn main() -> ExitCode {
    // hidden helper mode used by the embedded pjdfstest shell suite; bypass clap
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("__pjdfstest") {
        std::process::exit(pjdfstest::helper_main(&argv[1..]));
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Smallfile { mountpoint, args } => {
            init_log(args.verbose);
            let json_path = args.json.clone();
            let report = smallfile::run(&mountpoint, &args).map_err(|e| e.to_string());
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Fsmark { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = fsmark::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Fio { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = fio::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Mdtest { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = mdtest::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Mdworkbench { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = mdworkbench::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Ltp { mountpoint, args } => {
            if args.prepare_script {
                print!("{}", ltp::PREPARE_SCRIPT);
                return ExitCode::SUCCESS;
            }
            init_log(false);
            let Some(mountpoint) = mountpoint else {
                log::error!("mount point required (or pass --prepare-script to print the LTP setup script)");
                return ExitCode::FAILURE;
            };
            let json_path = args.json.clone();
            let report = ltp::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Fsstress { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = fsstress::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Fsx { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = fsx::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Pjdfstest { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = pjdfstest::run(&mountpoint, &args).map_err(|e| e.to_string());
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
        Command::Stdfs { mountpoint, args } => {
            init_log(false);
            let json_path = args.json.clone();
            let report = stdfs::run(&mountpoint, &args);
            if let Ok(r) = &report {
                if let Some(path) = &json_path {
                    if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(r).unwrap()) {
                        log::error!("failed to write {}: {e}", path.display());
                    }
                }
            }
            emit(report)
        }
    }
}

fn emit(report: Result<serde_json::Value, String>) -> ExitCode {
    match report {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if report["status"] == serde_json::json!("ok") {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
