#[cfg(not(target_os = "linux"))]
compile_error!(
    "auto_sync currently supports Linux only. Windows support is intentionally disabled."
);

pub mod core;
