use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn check_destination_online(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("destination path does not exist");
    }
    if !path.is_dir() {
        bail!("destination path is not a directory");
    }

    let probe = path.join(".auto_sync_probe");
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
