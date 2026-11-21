use crate::common::{cache_dir_candidates, primary_cache_dir};
use flate2::read::GzDecoder;
use log::{info, warn};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/embedded_bundle.rs"));

#[derive(clap::Subcommand)]
pub enum BundleCmd {
    /// Inspect the compile-time embedded suite bundle carried inside this `fstest` binary
    Info,

    /// Explicitly extract the embedded suite bundle to a local directory and print its path
    Extract {
        /// destination directory (defaults to the content-addressed local cache)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

static EXTRACTED_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Ensure the compile-time embedded suite bundle (or `FSTEST_BUNDLE_DIR` override) is
/// extracted into a host-local cache directory (never on the FUSE mount under test)
/// and return its root path.
pub fn ensure_extracted() -> Option<PathBuf> {
    EXTRACTED_ROOT
        .get_or_init(|| {
            if let Ok(d) = std::env::var("FSTEST_BUNDLE_DIR")
                && !d.trim().is_empty()
            {
                let p = PathBuf::from(d);
                if p.join("manifest.json").is_file() {
                    return Some(p);
                }
            }
            if EMBEDDED_BUNDLE_BYTES.is_empty() || EMBEDDED_BUNDLE_HASH.is_empty() {
                return None;
            }
            let rel = format!("bundles/{EMBEDDED_BUNDLE_HASH}");
            for cand_base in cache_dir_candidates() {
                let dir = cand_base.join(&rel);
                if dir.join(".ready").is_file() {
                    return Some(dir);
                }
            }
            let dest = primary_cache_dir().join(&rel);
            match extract_embedded_to(&dest) {
                Ok(()) => Some(dest),
                Err(e) => {
                    warn!(
                        "failed to extract embedded suite bundle to {}: {e}",
                        dest.display()
                    );
                    None
                }
            }
        })
        .clone()
}

fn extract_embedded_to(dest: &Path) -> Result<(), String> {
    if dest.join(".ready").is_file() {
        return Ok(());
    }
    let parent = dest
        .parent()
        .ok_or_else(|| format!("invalid bundle dest {}", dest.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let staging = parent.join(format!(
        ".staging-{EMBEDDED_BUNDLE_HASH}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("mkdir {}: {e}", staging.display()))?;

    info!(
        "bundle: extracting embedded suite assets ({:.1} MiB) to {}...",
        EMBEDDED_BUNDLE_BYTES.len() as f64 / 1_048_576.0,
        dest.display()
    );
    let gz = GzDecoder::new(EMBEDDED_BUNDLE_BYTES);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive
        .unpack(&staging)
        .map_err(|e| format!("unpack embedded bundle tar: {e}"))?;

    relocate_git_tree(&staging, dest)?;
    fs::write(staging.join(".ready"), EMBEDDED_BUNDLE_HASH.as_bytes())
        .map_err(|e| e.to_string())?;

    if let Err(e) = fs::rename(&staging, dest) {
        let _ = fs::remove_dir_all(&staging);
        if dest.join(".ready").is_file() {
            return Ok(());
        }
        return Err(format!(
            "rename {} -> {}: {e}",
            staging.display(),
            dest.display()
        ));
    }
    Ok(())
}

/// Rewrite hardcoded `GIT_BUILD_DIR` inside `git/GIT-BUILD-OPTIONS` and `git/bin-wrappers/*`
/// so a packed Git test tree runs cleanly from its final extracted directory `final_dest`.
fn relocate_git_tree(staging_root: &Path, final_dest: &Path) -> Result<(), String> {
    let manifest_path = staging_root.join("manifest.json");
    let Ok(raw) = fs::read_to_string(&manifest_path) else {
        return Ok(());
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&raw) else {
        return Ok(());
    };
    let Some(orig_dir) = manifest["components"]["git"]["originalBuildDir"].as_str() else {
        return Ok(());
    };
    let staged_git_dir = staging_root.join("git");
    if !staged_git_dir.is_dir() {
        return Ok(());
    }
    let final_git_str = final_dest.join("git").display().to_string();
    let mut targets = vec![staged_git_dir.join("GIT-BUILD-OPTIONS")];
    if let Ok(rd) = fs::read_dir(staged_git_dir.join("bin-wrappers")) {
        for e in rd.flatten() {
            targets.push(e.path());
        }
    }
    if let Ok(rd) = fs::read_dir(&staged_git_dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name_s = name.to_string_lossy();
            if name_s.starts_with("git-")
                && let Ok(meta) = fs::symlink_metadata(&p)
                && meta.is_file()
                && meta.len() <= 512 * 1024
            {
                targets.push(p);
            }
        }
    }
    for p in targets {
        if !p.is_file() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&p)
            && text.contains(orig_dir)
        {
            let mode = fs::metadata(&p)
                .map(|m| m.permissions().mode())
                .unwrap_or(0o755);
            let replaced = text.replace(orig_dir, &final_git_str);
            fs::write(&p, replaced).map_err(|e| format!("relocate {}: {e}", p.display()))?;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

pub fn run(cmd: &BundleCmd) -> Result<Value, String> {
    match cmd {
        BundleCmd::Info => run_info(),
        BundleCmd::Extract { dir } => run_extract(dir.as_deref()),
    }
}

fn run_info() -> Result<Value, String> {
    if EMBEDDED_BUNDLE_BYTES.is_empty() {
        return Ok(json!({
            "suite": "bundle",
            "status": "ok",
            "bundled": false,
        }));
    }
    let gz = GzDecoder::new(EMBEDDED_BUNDLE_BYTES);
    let mut ar = tar::Archive::new(gz);
    let mut manifest = Value::Null;
    if let Ok(entries) = ar.entries() {
        for ent in entries.flatten() {
            if let Ok(p) = ent.path()
                && p == Path::new("manifest.json")
            {
                let mut s = String::new();
                let mut ent = ent;
                if ent.read_to_string(&mut s).is_ok() {
                    manifest = serde_json::from_str(&s).unwrap_or(Value::Null);
                }
                break;
            }
        }
    }
    Ok(json!({
        "suite": "bundle",
        "status": "ok",
        "bundled": true,
        "hash": EMBEDDED_BUNDLE_HASH,
        "payloadBytes": EMBEDDED_BUNDLE_BYTES.len(),
        "defaultCacheDir": primary_cache_dir()
            .join("bundles")
            .join(EMBEDDED_BUNDLE_HASH)
            .display()
            .to_string(),
        "manifest": manifest,
    }))
}

fn run_extract(dir: Option<&Path>) -> Result<Value, String> {
    if EMBEDDED_BUNDLE_BYTES.is_empty() {
        return Err(
            "this fstest binary was compiled without an embedded suite bundle (rebuild without FSTEST_SKIP_BUNDLE / --no-default-features)".into(),
        );
    }
    let dest = match dir {
        Some(d) => d.to_path_buf(),
        None => primary_cache_dir().join("bundles").join(EMBEDDED_BUNDLE_HASH),
    };
    extract_embedded_to(&dest)?;
    Ok(json!({
        "suite": "bundle",
        "status": "ok",
        "extractedDir": dest.display().to_string(),
        "hash": EMBEDDED_BUNDLE_HASH,
    }))
}
