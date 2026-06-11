param(
    [string]$InstallDir = "C:\auto_sync",
    [string]$RuntimeDir = "",
    [switch]$InstallSshd,
    [switch]$UserPath
)

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$rootDir = Split-Path -Parent $scriptDir
if ([string]::IsNullOrWhiteSpace($RuntimeDir)) {
    $RuntimeDir = Join-Path $rootDir "bin\windows"
}

$openSshZip = Join-Path $RuntimeDir "OpenSSH-Win64.zip"
$cwrsyncZip = Join-Path $RuntimeDir "cwrsync_6.2.5_x64_free.zip"
if (-not (Test-Path $openSshZip)) {
    throw "Missing $openSshZip. Run scripts/download_windows_runtime.sh on Linux first, or commit/pull bin/windows."
}
if (-not (Test-Path $cwrsyncZip)) {
    throw "Missing $cwrsyncZip. Run scripts/download_windows_runtime.sh on Linux first, or commit/pull bin/windows."
}

$targetRuntime = Join-Path $InstallDir "runtime"
$openSshDir = Join-Path $targetRuntime "openssh"
$cwrsyncDir = Join-Path $targetRuntime "cwrsync"

New-Item -ItemType Directory -Force -Path $targetRuntime | Out-Null
Copy-Item -Force $openSshZip (Join-Path $targetRuntime "OpenSSH-Win64.zip")
Copy-Item -Force $cwrsyncZip (Join-Path $targetRuntime "cwrsync_6.2.5_x64_free.zip")
if (Test-Path (Join-Path $RuntimeDir "SHA256SUMS")) {
    Copy-Item -Force (Join-Path $RuntimeDir "SHA256SUMS") (Join-Path $targetRuntime "SHA256SUMS")
}

Remove-Item -Recurse -Force $openSshDir, $cwrsyncDir -ErrorAction SilentlyContinue
Expand-Archive -Force (Join-Path $targetRuntime "OpenSSH-Win64.zip") $openSshDir
Expand-Archive -Force (Join-Path $targetRuntime "cwrsync_6.2.5_x64_free.zip") $cwrsyncDir

$openSshBin = Join-Path $openSshDir "OpenSSH-Win64"
$cwrsyncBin = Join-Path $cwrsyncDir "cwrsync_6.2.5_x64_free\bin"
$pathScope = if ($UserPath) { "User" } else { "Machine" }
$currentPath = [Environment]::GetEnvironmentVariable("Path", $pathScope)
foreach ($path in @($openSshBin, $cwrsyncBin)) {
    if (-not (($currentPath -split ";") -contains $path)) {
        $currentPath = "$currentPath;$path"
    }
}
[Environment]::SetEnvironmentVariable("Path", $currentPath, $pathScope)
$env:Path = "$env:Path;$openSshBin;$cwrsyncBin"

if ($InstallSshd) {
    $installSshd = Join-Path $openSshBin "install-sshd.ps1"
    if (-not (Test-Path $installSshd)) {
        throw "Missing $installSshd"
    }
    & powershell.exe -ExecutionPolicy Bypass -File $installSshd
    Set-Service -Name sshd -StartupType Automatic
    Start-Service sshd
}

Write-Host "Windows runtime installed under $targetRuntime"
Write-Host "OpenSSH bin: $openSshBin"
Write-Host "cwRsync bin: $cwrsyncBin"
Write-Host "PATH scope: $pathScope"
Write-Host "Use -InstallSshd from an elevated PowerShell if this Windows machine should accept SSH connections."
