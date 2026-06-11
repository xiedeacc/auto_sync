use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn check_destination_online(path: &Path) -> Result<()> {
    // If the effective subdirectory doesn't exist yet (first sync will create it),
    // verify the parent mount point is accessible instead.
    let probe_dir = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|p| p.exists() && p.is_dir())
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("destination path does not exist: {}", path.display()))?
    };
    if !probe_dir.is_dir() {
        bail!(
            "destination path is not a directory: {}",
            probe_dir.display()
        );
    }

    let probe = probe_dir.join(".auto_sync_probe");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe)
        .with_context(|| format!("destination is not writable: {}", path.display()))?;
    file.write_all(b"ok")?;
    file.sync_all()?;
    drop(file);
    fs::remove_file(&probe).ok();
    Ok(())
}

pub fn check_file_destination_online(path: &Path) -> Result<()> {
    if path.exists() && path.is_dir() {
        bail!("destination file path is a directory");
    }
    let Some(parent) = path.parent() else {
        bail!("destination file path has no parent");
    };
    check_destination_online(parent)
}
