param(
    [string]$InstallDir = "C:\auto_sync",
    [string]$RuntimeDir = "",
    [string]$Config = "",
    [string]$AuthorizedKeyFile = "",
    [int]$SshPort = 10022,
    [switch]$NoBuild,
    [switch]$InstallSshd,
    [switch]$SkipSshd,
    [switch]$UseBundledOpenSsh,
    [switch]$UserPath,
    [switch]$SkipService,
    [switch]$NoElevate
)

$ErrorActionPreference = "Stop"

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-PreferredPowerShell {
    $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
    if ($pwsh) {
        return $pwsh.Source
    }
    (Get-Command powershell.exe -ErrorAction Stop).Source
}

function ConvertTo-CommandLineArgument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    '"' + ($Value -replace '"', '\"') + '"'
}

function Invoke-ElevatedSelf {
    $powerShellExe = Get-PreferredPowerShell
    $args = @(
        "-NoLogo",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        $scriptPath,
        "-InstallDir",
        $InstallDir,
        "-RuntimeDir",
        $RuntimeDir,
        "-Config",
        $Config,
        "-AuthorizedKeyFile",
        $AuthorizedKeyFile,
        "-SshPort",
        ([string]$SshPort)
    )
    if ($NoBuild) { $args += "-NoBuild" }
    if ($InstallSshd) { $args += "-InstallSshd" }
    if ($SkipSshd) { $args += "-SkipSshd" }
    if ($UseBundledOpenSsh) { $args += "-UseBundledOpenSsh" }
    if ($UserPath) { $args += "-UserPath" }
    if ($SkipService) { $args += "-SkipService" }

    $argLine = ($args | ForEach-Object { ConvertTo-CommandLineArgument $_ }) -join " "
    Write-Host "Administrator privileges are required; requesting elevation with $powerShellExe ..."
    $process = Start-Process -FilePath $powerShellExe `
        -ArgumentList $argLine `
        -Verb RunAs `
        -WorkingDirectory $rootDir `
        -Wait `
        -PassThru
    exit $process.ExitCode
}

function Remove-DirectoryUnder {
    param(
        [string]$Path,
        [string]$Parent
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }

    $fullPath = [IO.Path]::GetFullPath($Path)
    $fullParent = [IO.Path]::GetFullPath($Parent).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $fullPath.StartsWith($fullParent, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove $fullPath because it is not under $fullParent"
    }
    Remove-Item -LiteralPath $fullPath -Recurse -Force
}

function Add-PathEntries {
    param(
        [string[]]$Paths,
        [ValidateSet("Machine", "User")]
        [string]$Scope
    )

    $currentPath = [Environment]::GetEnvironmentVariable("Path", $Scope)
    $entries = @()
    if (-not [string]::IsNullOrWhiteSpace($currentPath)) {
        $entries = $currentPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }

    foreach ($path in $Paths) {
        if ([string]::IsNullOrWhiteSpace($path)) {
            continue
        }
        if (-not (Test-Path -LiteralPath $path)) {
            continue
        }
        if (-not ($entries -contains $path)) {
            $entries += $path
        }
        if (-not (($env:Path -split ";") -contains $path)) {
            $env:Path = "$env:Path;$path"
        }
    }

    [Environment]::SetEnvironmentVariable("Path", ($entries -join ";"), $Scope)
}

function Copy-ReleaseBinaries {
    param(
        [string]$RootDir,
        [string]$BinDir
    )

    if (-not $NoBuild) {
        Push-Location $RootDir
        try {
            cargo build --release --bins
        }
        finally {
            Pop-Location
        }
    }

    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $artifactDir = Join-Path $RootDir "target\release"
    foreach ($binary in @("auto_syncd.exe", "auto_syncctl.exe", "auto_sync_web.exe", "auto_sync_gui.exe")) {
        $source = Join-Path $artifactDir $binary
        if (-not (Test-Path -LiteralPath $source)) {
            throw "Missing build artifact: $source"
        }
        Copy-Item -LiteralPath $source -Destination (Join-Path $BinDir $binary) -Force
    }
}

function Initialize-Config {
    param(
        [string]$SourceConfig,
        [string]$TargetConfig
    )

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $TargetConfig) | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path (Split-Path -Parent $TargetConfig) "state") | Out-Null
    if (Test-Path -LiteralPath $TargetConfig) {
        return "left-existing"
    }
    if (-not (Test-Path -LiteralPath $SourceConfig)) {
        throw "Missing config template: $SourceConfig"
    }
    Copy-Item -LiteralPath $SourceConfig -Destination $TargetConfig -Force
    "seeded"
}

function Install-WindowsRuntime {
    param(
        [string]$RuntimeDir,
        [string]$TargetRuntime
    )

    $openSshZip = Join-Path $RuntimeDir "OpenSSH-Win64.zip"

    New-Item -ItemType Directory -Force -Path $TargetRuntime | Out-Null
    if (Test-Path -LiteralPath (Join-Path $RuntimeDir "SHA256SUMS")) {
        Copy-Item -Force -LiteralPath (Join-Path $RuntimeDir "SHA256SUMS") -Destination (Join-Path $TargetRuntime "SHA256SUMS")
    }

    $openSshBin = $null
    if (Test-Path -LiteralPath $openSshZip) {
        Copy-Item -Force -LiteralPath $openSshZip -Destination (Join-Path $TargetRuntime "OpenSSH-Win64.zip")
        $openSshDir = Join-Path $TargetRuntime "openssh"
        Remove-DirectoryUnder -Path $openSshDir -Parent $TargetRuntime
        Expand-Archive -Force (Join-Path $TargetRuntime "OpenSSH-Win64.zip") $openSshDir
        $openSshBin = Join-Path $openSshDir "OpenSSH-Win64"
    }

    [PSCustomObject]@{
        OpenSshBin = $openSshBin
    }
}

function Get-SystemOpenSshBin {
    $systemOpenSshBin = Join-Path $env:WINDIR "System32\OpenSSH"
    if (Test-Path -LiteralPath (Join-Path $systemOpenSshBin "ssh.exe")) {
        return $systemOpenSshBin
    }
    $ssh = Get-Command ssh.exe -ErrorAction SilentlyContinue
    if ($ssh) {
        return (Split-Path -Parent $ssh.Source)
    }
    $null
}

function Get-SshdServicePath {
    $service = Get-CimInstance Win32_Service -Filter "Name='sshd'" -ErrorAction SilentlyContinue
    if (-not $service -or [string]::IsNullOrWhiteSpace($service.PathName)) {
        return $null
    }
    if ($service.PathName -match '^"([^"]+)"') {
        return $Matches[1]
    }
    ($service.PathName -split "\s+")[0]
}

function Get-WindowsCapabilityState {
    param([string]$Name)
    try {
        $capability = Get-WindowsCapability -Online -Name $Name -ErrorAction Stop
        if ($capability) {
            return [string]$capability.State
        }
    }
    catch {
        Write-Warning "Could not query Windows capability ${Name}: $($_.Exception.Message)"
    }
    $null
}

function Install-SystemOpenSshServer {
    $capabilityName = "OpenSSH.Server~~~~0.0.1.0"
    $state = Get-WindowsCapabilityState -Name $capabilityName
    if ($state -and $state -ne "Installed") {
        Write-Host "Installing Windows OpenSSH Server optional feature ..."
        Add-WindowsCapability -Online -Name $capabilityName | Out-Host
    }

    $systemOpenSshBin = Join-Path $env:WINDIR "System32\OpenSSH"
    $systemSshd = Join-Path $systemOpenSshBin "sshd.exe"
    if (-not (Test-Path -LiteralPath $systemSshd)) {
        return $false
    }

    if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
        New-Service -Name sshd `
            -DisplayName "OpenSSH SSH Server" `
            -BinaryPathName "`"$systemSshd`"" `
            -Description "OpenSSH SSH Server" `
            -StartupType Automatic | Out-Null
    }
    $true
}

function Install-BundledOpenSshServer {
    param([string]$OpenSshBin)

    if ([string]::IsNullOrWhiteSpace($OpenSshBin)) {
        throw "Bundled OpenSSH runtime is missing. Expected OpenSSH-Win64.zip under $RuntimeDir."
    }

    $installSshd = Join-Path $OpenSshBin "install-sshd.ps1"
    if (-not (Test-Path -LiteralPath $installSshd)) {
        throw "Missing $installSshd"
    }
    $powerShellExe = Get-PreferredPowerShell
    Write-Host "Installing bundled OpenSSH Server with $powerShellExe ..."
    & $powerShellExe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $installSshd
}

function Ensure-SshHostKeys {
    param([string]$OpenSshBin)

    $programDataSsh = Join-Path $env:ProgramData "ssh"
    New-Item -ItemType Directory -Force -Path $programDataSsh | Out-Null

    $config = Join-Path $programDataSsh "sshd_config"
    $defaultConfig = Join-Path $OpenSshBin "sshd_config_default"
    if (-not (Test-Path -LiteralPath $config) -and (Test-Path -LiteralPath $defaultConfig)) {
        Copy-Item -LiteralPath $defaultConfig -Destination $config -Force
    }

    $sshKeygen = Join-Path $OpenSshBin "ssh-keygen.exe"
    if (-not (Test-Path -LiteralPath $sshKeygen)) {
        $sshKeygen = (Get-Command ssh-keygen.exe -ErrorAction SilentlyContinue).Source
    }
    if ([string]::IsNullOrWhiteSpace($sshKeygen)) {
        Write-Warning "ssh-keygen.exe was not found; host keys could not be generated proactively."
        return
    }

    $hostKey = Join-Path $programDataSsh "ssh_host_ed25519_key"
    if (-not (Test-Path -LiteralPath $hostKey)) {
        & $sshKeygen -A | Out-Host
    }
}

function Set-SshdConfigOption {
    param(
        [string]$Path,
        [string]$Key,
        [string]$Value
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "Missing sshd_config: $Path"
    }

    $lines = @(Get-Content -LiteralPath $Path)
    $pattern = "^\s*#?\s*$([regex]::Escape($Key))(\s+|$)"
    $replacement = "$Key $Value"
    $updated = New-Object System.Collections.Generic.List[string]
    $replaced = $false
    foreach ($line in $lines) {
        if ($line -match $pattern) {
            if (-not $replaced) {
                $updated.Add($replacement)
                $replaced = $true
            }
            continue
        }
        $updated.Add($line)
    }
    if (-not $replaced) {
        $updated.Add($replacement)
    }
    Set-Content -LiteralPath $Path -Value $updated -Encoding ascii
}

function Configure-OpenSshServer {
    param([int]$Port)

    $config = Join-Path $env:ProgramData "ssh\sshd_config"
    Set-SshdConfigOption -Path $config -Key "Port" -Value ([string]$Port)
    Set-SshdConfigOption -Path $config -Key "PubkeyAuthentication" -Value "yes"
    Set-SshdConfigOption -Path $config -Key "PasswordAuthentication" -Value "no"
    Set-SshdConfigOption -Path $config -Key "KbdInteractiveAuthentication" -Value "no"
    Set-SshdConfigOption -Path $config -Key "ChallengeResponseAuthentication" -Value "no"
}

function Ensure-OpenSshFirewall {
    param([int]$Port)

    $ruleName = "OpenSSH-Server-In-TCP-$Port"
    try {
        $rule = Get-NetFirewallRule -Name $ruleName -ErrorAction SilentlyContinue
        if ($rule) {
            Enable-NetFirewallRule -Name $ruleName | Out-Null
        }
        else {
            New-NetFirewallRule -Name $ruleName `
                -DisplayName "OpenSSH SSH Server ($Port)" `
                -Enabled True `
                -Direction Inbound `
                -Protocol TCP `
                -Action Allow `
                -LocalPort $Port | Out-Null
        }

        $targetPort = [string]$Port
        Get-NetFirewallRule -Direction Inbound -ErrorAction Stop |
            Where-Object { $_.Name -like "*OpenSSH*" -or $_.DisplayName -like "*OpenSSH*" -or $_.Group -like "*OpenSSH*" } |
            ForEach-Object {
                $rule = $_
                $hasTargetPort = $false
                $rule | Get-NetFirewallPortFilter -ErrorAction SilentlyContinue |
                    Where-Object { $_.Protocol -eq "TCP" } |
                    ForEach-Object {
                        if ([string]$_.LocalPort -eq $targetPort) {
                            $hasTargetPort = $true
                        }
                    }
                if (-not $hasTargetPort) {
                    Disable-NetFirewallRule -Name $rule.Name | Out-Null
                }
            }
    }
    catch {
        netsh advfirewall firewall add rule name="OpenSSH SSH Server ($Port)" dir=in action=allow protocol=TCP localport=$Port | Out-Null
        if ($Port -ne 22) {
            netsh advfirewall firewall set rule name="OpenSSH SSH Server" dir=in new enable=no 2>$null | Out-Null
            netsh advfirewall firewall set rule name="OpenSSH SSH Server (22)" dir=in new enable=no 2>$null | Out-Null
            netsh advfirewall firewall set rule name="OpenSSH SSH Server (sshd)" dir=in new enable=no 2>$null | Out-Null
        }
    }
}

function Repair-OpenSshProgramDataPermissions {
    $programDataSsh = Join-Path $env:ProgramData "ssh"
    if (-not (Test-Path -LiteralPath $programDataSsh)) {
        return
    }
    $currentIdentity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    $administrators = New-Object Security.Principal.NTAccount("BUILTIN", "Administrators")

    function Set-AdministratorsOwner {
        param([string]$Path)

        $acl = Get-Acl -LiteralPath $Path
        $acl.SetOwner($administrators)
        Set-Acl -LiteralPath $Path -AclObject $acl
    }

    icacls $programDataSsh `
        /inheritance:r `
        /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-11:(OI)(CI)RX" | Out-Null
    Set-AdministratorsOwner -Path $programDataSsh

    Get-ChildItem -LiteralPath $programDataSsh -File -Force |
        Where-Object { $_.Name -like "ssh_host_*_key" -and $_.Extension -ne ".pub" } |
        ForEach-Object {
            Set-AdministratorsOwner -Path $_.FullName
            icacls $_.FullName `
                /inheritance:r `
                /grant:r "*S-1-5-18:F" "*S-1-5-32-544:F" | Out-Null
            icacls $_.FullName `
                /remove:g $currentIdentity "*S-1-5-32-545" "*S-1-5-11" "*S-1-1-0" | Out-Null
        }

    Get-ChildItem -LiteralPath $programDataSsh -File -Force |
        Where-Object { $_.Name -like "ssh_host_*_key.pub" -or $_.Name -eq "sshd_config" } |
        ForEach-Object {
            Set-AdministratorsOwner -Path $_.FullName
            icacls $_.FullName `
                /inheritance:r `
                /grant:r "*S-1-5-18:F" "*S-1-5-32-544:F" "*S-1-5-11:RX" | Out-Null
            icacls $_.FullName `
                /remove:g $currentIdentity "*S-1-5-32-545" "*S-1-1-0" | Out-Null
        }
}

function Set-OpenSshDefaultShell {
    $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
    if (-not $pwsh) {
        return $null
    }

    $openSshRegPath = "HKLM:\SOFTWARE\OpenSSH"
    New-Item -Path $openSshRegPath -Force | Out-Null
    New-ItemProperty -Path $openSshRegPath -Name DefaultShell -Value $pwsh.Source -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $openSshRegPath -Name DefaultShellCommandOption -Value "-c" -PropertyType String -Force | Out-Null
    $pwsh.Source
}

function Ensure-Sshd {
    param(
        [string]$BundledOpenSshBin
    )

    $existingSshd = Get-SshdServicePath
    if ($existingSshd -and -not $UseBundledOpenSsh) {
        $selectedOpenSshBin = Split-Path -Parent $existingSshd
        Write-Host "Using existing sshd service: $existingSshd"
    }
    else {
        $systemReady = $false
        if (-not $UseBundledOpenSsh) {
            try {
                $systemReady = Install-SystemOpenSshServer
            }
            catch {
                Write-Warning "Windows OpenSSH Server optional feature is not available: $($_.Exception.Message)"
                $systemReady = $false
            }
        }

        if ($systemReady) {
            $selectedOpenSshBin = Join-Path $env:WINDIR "System32\OpenSSH"
            Write-Host "Using Windows OpenSSH Server optional feature."
        }
        else {
            Install-BundledOpenSshServer -OpenSshBin $BundledOpenSshBin
            $selectedOpenSshBin = $BundledOpenSshBin
            Write-Host "Using bundled OpenSSH Server."
        }
    }

    Ensure-SshHostKeys -OpenSshBin $selectedOpenSshBin
    Configure-OpenSshServer -Port $SshPort
    Repair-OpenSshProgramDataPermissions
    Ensure-OpenSshFirewall -Port $SshPort
    Set-Service -Name sshd -StartupType Automatic
    if ((Get-Service sshd -ErrorAction SilentlyContinue).Status -eq "Running") {
        Restart-Service -Name sshd -Force
    }
    else {
        Start-Service -Name sshd
    }
    $defaultShell = Set-OpenSshDefaultShell

    [PSCustomObject]@{
        OpenSshBin = $selectedOpenSshBin
        ServiceStatus = (Get-Service sshd).Status
        DefaultShell = $defaultShell
        Port = $SshPort
    }
}

function Ensure-AuthorizedKey {
    param([string]$PublicKeyFile)

    if ([string]::IsNullOrWhiteSpace($PublicKeyFile)) {
        $PublicKeyFile = Join-Path $HOME ".ssh\id_ed25519.pub"
    }
    if (-not (Test-Path -LiteralPath $PublicKeyFile)) {
        Write-Warning "Public key not found, skipping authorized_keys setup: $PublicKeyFile"
        return $null
    }

    $sshDir = Join-Path $HOME ".ssh"
    $authorizedKeys = Join-Path $sshDir "authorized_keys"
    New-Item -ItemType Directory -Force -Path $sshDir | Out-Null

    $key = (Get-Content -LiteralPath $PublicKeyFile -Raw).Trim()
    if ([string]::IsNullOrWhiteSpace($key)) {
        throw "Public key is empty: $PublicKeyFile"
    }

    if (-not (Test-Path -LiteralPath $authorizedKeys)) {
        Set-Content -LiteralPath $authorizedKeys -Value $key -Encoding ascii
        $action = "created"
    }
    else {
        $lines = Get-Content -LiteralPath $authorizedKeys
        if ($lines -contains $key) {
            $action = "already-present"
        }
        else {
            Add-Content -LiteralPath $authorizedKeys -Value $key -Encoding ascii
            $action = "appended"
        }
    }

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    icacls $sshDir /inheritance:r /grant:r "$($identity):(OI)(CI)F" "SYSTEM:(OI)(CI)F" /remove:g "Everyone" "BUILTIN\Users" "Authenticated Users" 2>$null | Out-Null
    icacls $authorizedKeys /inheritance:r /grant:r "$($identity):F" "SYSTEM:F" /remove:g "Everyone" "BUILTIN\Users" "Authenticated Users" 2>$null | Out-Null

    $adminAuthorizedKeys = $null
    $adminAction = $null
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        $programDataSsh = Join-Path $env:ProgramData "ssh"
        if (Test-Path -LiteralPath $programDataSsh) {
            $adminAuthorizedKeys = Join-Path $programDataSsh "administrators_authorized_keys"
            if (-not (Test-Path -LiteralPath $adminAuthorizedKeys)) {
                Set-Content -LiteralPath $adminAuthorizedKeys -Value $key -Encoding ascii
                $adminAction = "created"
            }
            else {
                $adminLines = Get-Content -LiteralPath $adminAuthorizedKeys
                if ($adminLines -contains $key) {
                    $adminAction = "already-present"
                }
                else {
                    Add-Content -LiteralPath $adminAuthorizedKeys -Value $key -Encoding ascii
                    $adminAction = "appended"
                }
            }

            $administrators = New-Object Security.Principal.NTAccount("BUILTIN", "Administrators")
            $acl = Get-Acl -LiteralPath $adminAuthorizedKeys
            $acl.SetOwner($administrators)
            Set-Acl -LiteralPath $adminAuthorizedKeys -AclObject $acl
            icacls $adminAuthorizedKeys `
                /inheritance:r `
                /grant:r "*S-1-5-18:F" "*S-1-5-32-544:F" `
                /remove:g $identity "*S-1-5-32-545" "*S-1-5-11" "*S-1-1-0" | Out-Null
        }
    }

    [PSCustomObject]@{
        Path = $authorizedKeys
        Action = $action
        AdministratorsPath = $adminAuthorizedKeys
        AdministratorsAction = $adminAction
    }
}

function Ensure-AutoSyncDaemonService {
    param(
        [string]$BinDir,
        [string]$ConfigPath
    )

    $serviceName = "auto_syncd"
    $daemonExe = Join-Path $BinDir "auto_syncd.exe"
    if (-not (Test-Path -LiteralPath $daemonExe)) {
        throw "Missing daemon binary: $daemonExe"
    }
    $binaryPath = "`"$daemonExe`" --config `"$ConfigPath`""
    $existing = Get-CimInstance Win32_Service -Filter "Name='$serviceName'" -ErrorAction SilentlyContinue
    if ($existing) {
        if ($existing.State -eq "Running") {
            Stop-Service -Name $serviceName -Force -ErrorAction SilentlyContinue
        }
        & sc.exe config $serviceName binPath= $binaryPath start= auto | Out-Null
        Set-Service -Name $serviceName -StartupType Automatic
    }
    else {
        New-Service `
            -Name $serviceName `
            -DisplayName "auto_sync daemon" `
            -BinaryPathName $binaryPath `
            -Description "auto_sync realtime watcher and sync scheduler" `
            -StartupType Automatic | Out-Null
    }
    Start-Service -Name $serviceName
    Get-Service -Name $serviceName
}

if ($InstallSshd -and $SkipSshd) {
    throw "-InstallSshd and -SkipSshd cannot be used together."
}

$scriptPath = $MyInvocation.MyCommand.Path
$scriptDir = Split-Path -Parent $scriptPath
$rootDir = Split-Path -Parent $scriptDir
if ([string]::IsNullOrWhiteSpace($RuntimeDir)) {
    $RuntimeDir = Join-Path $rootDir "bin\windows"
}
if ([string]::IsNullOrWhiteSpace($Config)) {
    $Config = Join-Path $rootDir "conf\auto_sync.toml"
}
if ([string]::IsNullOrWhiteSpace($AuthorizedKeyFile)) {
    $AuthorizedKeyFile = Join-Path $HOME ".ssh\id_ed25519.pub"
}

$ensureSshd = -not $SkipSshd
$ensureService = -not $SkipService
$needsAdmin = (-not $UserPath) -or $ensureSshd -or $ensureService
if ($needsAdmin -and -not (Test-IsAdministrator)) {
    if ($NoElevate) {
        throw "Administrator privileges are required. Re-run without -NoElevate or start PowerShell as Administrator."
    }
    Invoke-ElevatedSelf
}

$binDir = Join-Path $InstallDir "bin"
$confDir = Join-Path $InstallDir "conf"
$logDir = Join-Path $InstallDir "logs"
$targetConfig = Join-Path $confDir "auto_sync.toml"
$targetRuntime = Join-Path $InstallDir "runtime"

New-Item -ItemType Directory -Force -Path $InstallDir, $binDir, $confDir, $logDir | Out-Null
Copy-ReleaseBinaries -RootDir $rootDir -BinDir $binDir
$configAction = Initialize-Config -SourceConfig $Config -TargetConfig $targetConfig
$runtime = Install-WindowsRuntime -RuntimeDir $RuntimeDir -TargetRuntime $targetRuntime

$sshResult = $null
if ($ensureSshd) {
    $sshResult = Ensure-Sshd -BundledOpenSshBin $runtime.OpenSshBin
    $openSshBinForPath = $sshResult.OpenSshBin
}
else {
    $openSshBinForPath = Get-SystemOpenSshBin
    if ([string]::IsNullOrWhiteSpace($openSshBinForPath)) {
        $openSshBinForPath = $runtime.OpenSshBin
    }
}

$pathScope = if ($UserPath) { "User" } else { "Machine" }
Add-PathEntries -Scope $pathScope -Paths @($openSshBinForPath, $binDir)
$authorizedKeyResult = Ensure-AuthorizedKey -PublicKeyFile $AuthorizedKeyFile
$daemonService = $null
if ($ensureService) {
    $daemonService = Ensure-AutoSyncDaemonService -BinDir $binDir -ConfigPath $targetConfig
}

Write-Host "auto_sync installed under $InstallDir"
Write-Host "Binaries: $binDir"
Write-Host "Config: $targetConfig ($configAction)"
Write-Host "Runtime: $targetRuntime"
Write-Host "OpenSSH bin: $openSshBinForPath"
Write-Host "PATH scope: $pathScope"
if ($sshResult) {
    Write-Host "sshd status: $($sshResult.ServiceStatus)"
    Write-Host "sshd port: $($sshResult.Port)"
    if ($sshResult.DefaultShell) {
        Write-Host "OpenSSH default shell: $($sshResult.DefaultShell)"
    }
}
else {
    Write-Host "sshd setup skipped by -SkipSshd"
}
if ($daemonService) {
    Write-Host "auto_syncd service: $($daemonService.Status)"
}
else {
    Write-Host "auto_syncd service setup skipped by -SkipService"
}
if ($authorizedKeyResult) {
    Write-Host "authorized_keys: $($authorizedKeyResult.Path) ($($authorizedKeyResult.Action))"
    if ($authorizedKeyResult.AdministratorsPath) {
        Write-Host "administrators_authorized_keys: $($authorizedKeyResult.AdministratorsPath) ($($authorizedKeyResult.AdministratorsAction))"
    }
}
