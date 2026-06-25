use std::process::Command;

fn main() {
    emit_git_build_info();
    #[cfg(feature = "gui")]
    tauri_build::build();
}

fn emit_git_build_info() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    let commit =
        git_output(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let commit_epoch = git_output(&["show", "-s", "--format=%ct", "HEAD"])
        .and_then(|value| value.parse::<i64>().ok());
    let commit_time = commit_epoch
        .and_then(format_beijing_time)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AUTO_SYNC_GIT_COMMIT_SHORT={commit}");
    println!("cargo:rustc-env=AUTO_SYNC_GIT_COMMIT_TIME_BEIJING={commit_time}");
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn format_beijing_time(epoch_secs: i64) -> Option<String> {
    let utc = chrono::DateTime::from_timestamp(epoch_secs, 0)?;
    let beijing = utc.with_timezone(&chrono::FixedOffset::east_opt(8 * 60 * 60)?);
    Some(beijing.format("%Y%m%d %H:%M").to_string())
}
