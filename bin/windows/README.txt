Windows runtime bundled by scripts/download_windows_runtime.sh

OpenSSH:
  OpenSSH-Win64.zip
  Source: https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.0.0.0p2-Preview/OpenSSH-Win64.zip

rsync:
  cwrsync_6.2.5_x64_free.zip
  Source package: https://community.chocolatey.org/api/v2/package/rsync/6.2.5

Extracted folders:
  openssh/
  cwrsync/

On Windows, put the extracted OpenSSH and cwRsync bin directories in PATH for
the auto_sync rsync transport, or copy their executables into a shared tool
directory.
