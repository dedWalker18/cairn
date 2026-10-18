//! Filesystem locations and raw-byte path helpers.
//!
//! Linux file names are arbitrary bytes, not UTF-8. Everything that is stored
//! or sent over IPC keeps the raw bytes; lossy strings are for display only.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub fn home_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => bail!("HOME is not set"),
    }
}

fn xdg(var: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if Path::new(&v).is_absolute() => Ok(PathBuf::from(v)),
        _ => Ok(home_dir()?.join(fallback)),
    }
}

/// Root of everything Cairn writes. `CAIRN_DATA_DIR` overrides (tests, VMs).
pub fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CAIRN_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    Ok(xdg("XDG_DATA_HOME", ".local/share")?.join("cairn"))
}

pub fn models_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CAIRN_MODELS_DIR") {
        return Ok(PathBuf::from(d));
    }
    Ok(data_dir()?.join("models"))
}

/// Directory holding the control socket. Per-user, mode 0700.
pub fn runtime_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CAIRN_RUNTIME_DIR") {
        return Ok(PathBuf::from(d));
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if Path::new(&v).is_absolute() => Ok(PathBuf::from(v).join("cairn")),
        _ => Ok(data_dir()?.join("run")),
    }
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("cairnd.sock"))
}

/// Roots to index. `CAIRN_ROOTS` (colon separated) overrides; default is $HOME.
pub fn index_roots() -> Result<Vec<PathBuf>> {
    if let Some(r) = std::env::var_os("CAIRN_ROOTS") {
        let roots: Vec<PathBuf> = r
            .as_bytes()
            .split(|b| *b == b':')
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(OsStr::from_bytes(s)))
            .collect();
        if roots.iter().any(|p| !p.is_absolute()) {
            bail!("CAIRN_ROOTS entries must be absolute paths");
        }
        return Ok(roots);
    }
    Ok(vec![home_dir()?])
}

pub fn to_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

pub fn from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b))
}

pub fn display(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Create a directory (and parents) with mode 0700.
pub fn ensure_private_dir(p: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .with_context(|| format!("creating {}", p.display()))?;
    let meta = std::fs::metadata(p)?;
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write a small file atomically: temp file, fsync, rename, fsync dir.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().context("atomic_write: no parent")?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}
