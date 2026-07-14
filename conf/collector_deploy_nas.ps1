$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected NAS tree back to the Ubuntu host, then
# applies packages, services, runtimes, quotas, database restore, and cron.
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$opts    = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-o','LogLevel=ERROR','-o','ConnectTimeout=20','-o','ServerAliveInterval=30','-o','ServerAliveCountMax=20')
$sshArgs = @() + $opts
$scpArgs = @('-r') + $opts
if (-not [string]::IsNullOrEmpty($env:AS_PORT)) { $sshArgs += @('-p', $env:AS_PORT); $scpArgs += @('-P', $env:AS_PORT) }
if (-not [string]::IsNullOrEmpty($env:AS_KEY))  { $sshArgs += @('-i', $env:AS_KEY);  $scpArgs += @('-i', $env:AS_KEY) }

$errCount = 0
$remoteScratch = '/opt/tmp/auto_sync_deploy_scratch'
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$platformDefaultCollectPaths = @()
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })
$excludePaths += @('/opt/usr/local/blog/data/uploads')

function New-FinalSshdConfig([string]$Port) {
    if ([string]::IsNullOrWhiteSpace($Port)) { $Port = '10022' }
    return @"
Include /etc/ssh/sshd_config.d/*.conf
Port $Port
ClientAliveInterval 360
ClientAliveCountMax 0
AllowUsers root tiger git

#AuthenticationMethods publickey password
AuthenticationMethods publickey
PubkeyAuthentication yes
PasswordAuthentication no
PermitEmptyPasswords no
PermitRootLogin yes
KbdInteractiveAuthentication no
StrictModes yes
MaxAuthTries 3

HostbasedAuthentication no
UsePAM no
X11Forwarding no
PrintMotd no
IgnoreRhosts yes
UseDNS no

AcceptEnv LANG LC_*
Subsystem sftp  /usr/lib/openssh/sftp-server
"@
}

function Copy-AwsSslIntoCollectedRoot {
    if ([string]::IsNullOrWhiteSpace($root)) { return }
    $shareRoot = Split-Path -Parent $root
    if ([string]::IsNullOrWhiteSpace($shareRoot)) { return }
    $awsSsl = Join-Path $shareRoot 'aws\etc\nginx\ssl'
    if (-not (Test-Path -LiteralPath $awsSsl)) {
        Write-Host "! missing AWS SSL source $awsSsl"
        $script:errCount++
        return
    }
    $target = Join-Path $root 'etc\nginx\ssl'
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Get-ChildItem -LiteralPath $awsSsl -File -Force | Copy-Item -Destination $target -Force
    Write-Host "stage AWS SSL certificates from $awsSsl"
}

function Normalize-RemotePath([string]$Path) {
    $p = ($Path -replace '\\','/').Trim()
    if ($p -eq '') { return '/' }
    if (-not $p.StartsWith('/')) { $p = '/' + $p }
    while ($p.Length -gt 1 -and $p.EndsWith('/')) { $p = $p.Substring(0, $p.Length - 1) }
    return $p
}

function Test-RemoteExcluded([string]$Path) {
    $p = Normalize-RemotePath $Path
    foreach ($ex in $excludePaths) {
        $e = Normalize-RemotePath $ex
        if ($p -eq $e -or $p.StartsWith($e + '/')) { return $true }
    }
    return $false
}

function Get-LocalCollectedPath([string]$RemotePath) {
    return Join-Path $root ((Normalize-RemotePath $RemotePath).TrimStart([char[]]"/") -replace '/','\')
}

function Join-RemotePath([string]$Base, [string]$Relative) {
    $b = Normalize-RemotePath $Base
    $r = ($Relative -replace '\\','/').Trim([char[]]"/")
    if ($r -eq '') { return $b }
    if ($b -eq '/') { return '/' + $r }
    return $b + '/' + $r
}

function Resolve-ReparseTarget([IO.FileSystemInfo]$Item) {
    if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) { return $Item.FullName }
    $target = @($Item.Target | Select-Object -First 1)
    if ([string]::IsNullOrWhiteSpace($target)) { return $Item.FullName }
    if (-not [IO.Path]::IsPathRooted($target)) {
        $target = Join-Path (Split-Path -Parent $Item.FullName) $target
    }
    return [IO.Path]::GetFullPath($target)
}

function Copy-ResolvedTree([string]$Source, [string]$Target, [string]$RemotePath, [hashtable]$SeenDirs) {
    if (Test-RemoteExcluded $RemotePath) {
        Write-Host "skip excluded $RemotePath"
        return
    }
    $item = Get-Item -LiteralPath $Source -Force
    $resolved = Resolve-ReparseTarget $item
    if ($resolved -ne $item.FullName) {
        if (-not (Test-Path -LiteralPath $resolved)) {
            Write-Host "! broken symlink $Source -> $resolved"
            return
        }
        $item = Get-Item -LiteralPath $resolved -Force
    }
    if (-not $item.PSIsContainer) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Target) | Out-Null
        Copy-Item -LiteralPath $item.FullName -Destination $Target -Force
        return
    }

    $resolvedDir = [IO.Path]::GetFullPath($item.FullName).TrimEnd([char[]]"\/")
    if ($SeenDirs.ContainsKey($resolvedDir)) {
        Write-Host "skip symlink cycle $RemotePath -> $resolvedDir"
        return
    }
    $SeenDirs[$resolvedDir] = $true
    New-Item -ItemType Directory -Force -Path $Target | Out-Null
    Get-ChildItem -LiteralPath $item.FullName -Force | ForEach-Object {
        $remoteChild = Join-RemotePath $RemotePath $_.Name
        $childTarget = Join-Path $Target $_.Name
        Copy-ResolvedTree $_.FullName $childTarget $remoteChild $SeenDirs
    }
    $SeenDirs.Remove($resolvedDir)
}

function Copy-CollectedPathToStage([string]$RemotePath, [string]$StageRoot) {
    $remote = Normalize-RemotePath $RemotePath
    if (Test-RemoteExcluded $remote) {
        Write-Host "skip excluded $remote"
        return
    }

    $local = Get-LocalCollectedPath $remote
    if (-not (Test-Path -LiteralPath $local)) {
        Write-Host "! missing local $local"
        return
    }

    $target = Join-Path $StageRoot ($remote.TrimStart([char[]]"/") -replace '/','\')
    Copy-ResolvedTree $local $target $remote @{}
    Write-Host "stage $remote"
}

function Invoke-Remote([string]$Script) {
    $Script = $Script -replace "`r`n", "`n"
    $localTmp = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_remote_" + [guid]::NewGuid().ToString('N') + ".sh")
    $remoteTmp = "$remoteScratch/" + [IO.Path]::GetFileName($localTmp)
    try {
        [IO.File]::WriteAllText($localTmp, $Script, [Text.UTF8Encoding]::new($false))
        & $ssh @sshArgs $dest "mkdir -p '$remoteScratch'"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! remote scratch setup failed"
            $script:errCount++
            return
        }
        & $scp @scpArgs $localTmp "$($dest):$remoteTmp"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! remote script upload failed"
            $script:errCount++
            return
        }
        & $ssh @sshArgs $dest "bash '$remoteTmp' </dev/null; rc=`$?; rm -f '$remoteTmp'; exit `$rc"
        if ($LASTEXITCODE -ne 0) {
            $exitCode = $LASTEXITCODE
            Write-Host "! remote step exit $exitCode"
            $script:errCount++
            throw "remote step failed with exit code $exitCode"
        }
    } finally {
        Remove-Item -LiteralPath $localTmp -Force -ErrorAction SilentlyContinue
    }
}

function Transfer-CollectedPathsToStage([string[]]$RequiredPaths, [string[]]$OptionalPaths, [string]$RemoteStage) {
    $tarPaths = New-Object System.Collections.Generic.List[string]
    foreach ($p in @($RequiredPaths + $OptionalPaths)) {
        $remote = Normalize-RemotePath $p
        if (Test-RemoteExcluded $remote) {
            Write-Host "skip excluded $remote"
            continue
        }

        $local = Get-LocalCollectedPath $remote
        if (-not (Test-Path -LiteralPath $local)) {
            if ($RequiredPaths -contains $p) {
                Write-Host "! missing local $local"
                $script:errCount++
            }
            continue
        }

        [void]$tarPaths.Add($remote.TrimStart([char[]]"/"))
        Write-Host "stage $remote"
    }

    if ($tarPaths.Count -eq 0) { return }

    $excludeArgs = @()
    foreach ($exclude in $excludePaths) {
        $rel = (Normalize-RemotePath $exclude).TrimStart([char[]]"/")
        if ($rel -ne '') { $excludeArgs += "--exclude=$rel" }
    }

    $localTar = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_stage_" + [guid]::NewGuid().ToString('N') + ".tar")
    $remoteTar = "$remoteScratch/" + [IO.Path]::GetFileName($localTar)
    try {
        $tarArgs = @('-C', $root, '-cf', $localTar) + $excludeArgs + @($tarPaths)
        & tar @tarArgs
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! local tar stage creation failed"
            $script:errCount++
            return
        }
        & $scp @scpArgs $localTar "$($dest):$remoteTar"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! tar stage upload failed"
            $script:errCount++
            return
        }
        Invoke-Remote "mkdir -p $RemoteStage; tar -C $RemoteStage -xf '$remoteTar'; rc=`$?; rm -f '$remoteTar'; exit `$rc"
    } finally {
        Remove-Item -LiteralPath $localTar -Force -ErrorAction SilentlyContinue
    }
}

function Quote-ShellArg([string]$Value) {
    return "'" + $Value.Replace("'", "'""'""'") + "'"
}

function Ensure-RemoteRootWritable {
    & $ssh @sshArgs $dest @'
set -eu
if ! touch /etc/.auto_sync_rw_test 2>/dev/null; then
    mount -o remount,rw /
fi
touch /etc/.auto_sync_rw_test
rm -f /etc/.auto_sync_rw_test
'@
    if ($LASTEXITCODE -ne 0) {
        Write-Host "! remote root filesystem is not writable"
        $script:errCount++
        throw "remote root filesystem is not writable"
    }
}

function Prepare-StagedSymlinks([string]$PermsFile, [string]$RemoteStage) {
    if ([string]::IsNullOrWhiteSpace($PermsFile) -or -not (Test-Path -LiteralPath $PermsFile)) { return }
    $commands = New-Object System.Collections.Generic.List[string]
    $commands.Add("stage=$(Quote-ShellArg $RemoteStage)")
    foreach ($line in [IO.File]::ReadAllLines($PermsFile)) {
        $t = $line.Trim()
        if (-not $t.StartsWith('symlink ')) { continue }
        $rest = $t.Substring(8)
        $sp = $rest.IndexOf(' ')
        if ($sp -lt 1) { continue }
        try { $target = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($rest.Substring(0, $sp))) } catch { continue }
        $path = $rest.Substring($sp + 1)
        if (-not $path.StartsWith('/')) { continue }
        $qPath = Quote-ShellArg $path
        $qTarget = Quote-ShellArg $target
        $commands.Add(@"
path=$qPath
target=$qTarget
case "`$target" in /*) resolved="`$target" ;; *) resolved="`$(dirname -- "`$path")/`$target" ;; esac
resolved="`$(realpath -m -- "`$resolved")"
src="`$stage`$path"
dst="`$stage`$resolved"
if [ -e "`$src" ] || [ -L "`$src" ]; then
    mkdir -p -- "`$(dirname -- "`$dst")" "`$(dirname -- "`$src")"
    if [ -d "`$src" ] && [ ! -L "`$src" ]; then
        mkdir -p -- "`$dst"
        cp -a "`$src"/. "`$dst"/
        rm -rf -- "`$src"
    else
        rm -rf -- "`$dst"
        mv -- "`$src" "`$dst"
    fi
    ln -s -- "`$target" "`$src"
fi
"@)
    }
    if ($commands.Count -gt 1) {
        Write-Host "preparing $($commands.Count - 1) staged symlink(s)"
        Invoke-Remote ($commands -join "`n")
    }
}

Copy-AwsSslIntoCollectedRoot
Ensure-RemoteRootWritable
$remoteStage = '/opt/tmp/auto_sync_deploy_stage'
Invoke-Remote "rm -rf $(Quote-ShellArg $remoteStage); mkdir -p $(Quote-ShellArg $remoteStage)"
$generatedOptionalPaths = @('/opt/immich/conf', '/root/auto_sync_db_dumps', '/tmp/auto_sync_db_dumps') + $platformDefaultCollectPaths
$requiredCollectPaths = @($collectPaths | Where-Object { (Normalize-RemotePath $_) -ne '/opt/immich/conf' })
Transfer-CollectedPathsToStage $requiredCollectPaths $generatedOptionalPaths $remoteStage
Prepare-StagedSymlinks $env:AS_PERMS_FILE $remoteStage

$sshPort = $env:AS_PORT
if ([string]::IsNullOrWhiteSpace($sshPort)) { $sshPort = '10022' }
$finalSshd = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_sshd_" + [guid]::NewGuid().ToString('N'))
try {
    [IO.File]::WriteAllText($finalSshd, (New-FinalSshdConfig $sshPort), [Text.ASCIIEncoding]::new())
    & $ssh @sshArgs $dest "mkdir -p '$remoteScratch'"
    if ($LASTEXITCODE -ne 0) { Write-Host "! remote scratch setup failed"; $errCount++ }
    & $scp @scpArgs $finalSshd "$($dest):$remoteScratch/auto_sync_sshd_config"
    if ($LASTEXITCODE -ne 0) { Write-Host "! sshd_config upload failed"; $errCount++ }
} finally {
    Remove-Item -LiteralPath $finalSshd -Force -ErrorAction SilentlyContinue
}
Invoke-Remote @"
set -e
mkdir -p /etc/ssh/sshd_config.d /root/.ssh
cp $remoteScratch/auto_sync_sshd_config /etc/ssh/sshd_config
rm -f $remoteScratch/auto_sync_sshd_config
rm -f /etc/ssh/sshd_config.d/50-cloud-init.conf /etc/ssh/sshd_config.d/98-auto-sync-gitlab.conf /etc/ssh/sshd_config.d/99-auto-sync-test.conf
if [ -f /tmp/auto_sync_root_key.pub ]; then
    cat /tmp/auto_sync_root_key.pub > /root/.ssh/authorized_keys
fi
chmod 700 /root /root/.ssh
chown -R root:root /root/.ssh 2>/dev/null || true
find /root/.ssh -type f -name "id_*" ! -name "*.pub" -exec chmod 600 {} \; 2>/dev/null || true
find /root/.ssh -type f -name "*.pub" -exec chmod 644 {} \; 2>/dev/null || true
chmod 600 /root/.ssh/authorized_keys 2>/dev/null || true
sshd -t
systemctl disable --now ssh.socket 2>/dev/null || true
systemctl restart ssh.service 2>/dev/null || systemctl restart sshd.service
"@

if (-not [string]::IsNullOrWhiteSpace($env:AS_PERMS_FILE) -and (Test-Path -LiteralPath $env:AS_PERMS_FILE)) {
    $links = New-Object System.Collections.Generic.List[string]
    $chmods = New-Object System.Collections.Generic.List[string]
    foreach ($line in [IO.File]::ReadAllLines($env:AS_PERMS_FILE)) {
        $t = $line.Trim(); if ($t -eq '') { continue }
        if ($t.StartsWith('symlink ')) {
            $rest = $t.Substring(8)
            $sp = $rest.IndexOf(' '); if ($sp -lt 1) { continue }
            $targetB64 = $rest.Substring(0, $sp); $path = $rest.Substring($sp + 1)
            if (-not ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/') -or $path.StartsWith('/home/'))) { continue }
            try { $target = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($targetB64)) } catch { continue }
            $qPath = Quote-ShellArg $path
            $qTarget = Quote-ShellArg $target
            $links.Add("mkdir -p -- `$(dirname -- $qPath) 2>/dev/null || true; rm -rf -- $qPath 2>/dev/null || true; ln -s -- $qTarget $qPath 2>/dev/null || true")
            continue
        }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        if ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/') -or $path.StartsWith('/home/')) {
            $chmods.Add("chmod $mode $(Quote-ShellArg $path) 2>/dev/null || true")
        }
    }
    if ($links.Count -gt 0) { Write-Host "restoring $($links.Count) recorded symlink(s)"; Invoke-Remote ($links -join "`n") }
    if ($chmods.Count -gt 0) { Write-Host "restoring $($chmods.Count) recorded permissions"; Invoke-Remote ($chmods -join "`n") }
}

$remote = @'
set -u
export DEBIAN_FRONTEND=noninteractive
GITLAB_CE_VERSION="17.11.7-ce.0"

log() { printf '[nas-deploy] %s\n' "$*"; }
try() { "$@" && return 0; log "WARN: command failed: $*"; return 0; }
unit_name() {
    unit="$1"
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q . || [ -e "/etc/systemd/system/$unit" ] || [ -e "/lib/systemd/system/$unit" ] || [ -e "/usr/lib/systemd/system/$unit" ]; then
        printf '%s\n' "$unit"
        return 0
    fi
    case "$unit" in
        *.service|*.timer|*.socket) return 1 ;;
        *) unit="${unit}.service" ;;
    esac
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q . || [ -e "/etc/systemd/system/$unit" ] || [ -e "/lib/systemd/system/$unit" ] || [ -e "/usr/lib/systemd/system/$unit" ]; then
        printf '%s\n' "$unit"
        return 0
    fi
    return 1
}
unit_exists() { unit_name "$1" >/dev/null; }
remove_bad_enable_links() {
    unit="$1"
    for wants_dir in /etc/systemd/system/*.target.wants; do
        [ -d "$wants_dir" ] || continue
        link="$wants_dir/$unit"
        if [ -e "$link" ] && [ ! -L "$link" ]; then
            rm -f "$link"
        fi
    done
}
enable_unit() {
    unit="$1"
    remove_bad_enable_links "$unit"
    if ! systemctl enable "$unit" >/dev/null 2>&1; then
        log "ERROR: enable $unit failed"
        systemctl enable "$unit"
        return 1
    fi
}
restart_if_exists() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    if unit_exists "$unit"; then
        enable_unit "$unit"
        systemctl restart "$unit" && { log "restarted $unit"; log_unit_processes "$unit"; } || log "WARN: restart $unit failed"
    fi
}
log_unit_processes() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    active="$(systemctl is-active "$unit" 2>/dev/null || true)"
    main_pid="$(systemctl show "$unit" -p MainPID --value 2>/dev/null || true)"
    pids="$(systemctl show "$unit" -p ControlPID -p MainPID --value 2>/dev/null | awk '$1 != "" && $1 != "0" {print}' | paste -sd, - 2>/dev/null || true)"
    [ -n "$pids" ] || pids="$main_pid"
    log "process $unit active=${active:-unknown} pid=${pids:-none}"
}
stop_if_exists() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    if unit_exists "$unit"; then
        systemctl stop "$unit" >/dev/null 2>&1 || true
        log "stopped $unit before installing collected paths"
    fi
}
stop_services_before_install() {
    log "stop services before installing collected paths"
    for s in mysql postgresql redis-server gitlab-runsvdir gitlab immich-ml auto_sync halo2 immich tbox_server tbox_client tbox-logrotate.timer rblog rblog-backup.timer nginx cron shadowsocks shadowsocks-rust waiwei-web waiwei-puller xray; do
        stop_if_exists "$s"
    done
}
disable_if_exists() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    if unit_exists "$unit"; then
        systemctl disable "$unit" >/dev/null 2>&1 || true
        systemctl stop "$unit" >/dev/null 2>&1 || true
        systemctl reset-failed "$unit" >/dev/null 2>&1 || true
        log "disabled+stopped $unit"
    fi
}

normalize_deploy_permissions() {
    log "normalize deployed file permissions"
    if [ -d /etc/nginx/ssl ]; then
        chown root:root /etc/nginx/ssl /etc/nginx/ssl/* 2>/dev/null || true
        chmod 755 /etc/nginx/ssl 2>/dev/null || true
        find /etc/nginx/ssl -maxdepth 1 -type f -name '*.key' -exec chmod 600 {} + 2>/dev/null || true
        find /etc/nginx/ssl -maxdepth 1 -type f ! -name '*.key' -exec chmod 644 {} + 2>/dev/null || true
    fi
    for d in /opt/usr/local/blog /opt/usr/local/tbox /opt/usr/local/waiwei /opt/usr/local/xray /opt/usr/local/shadowsocks /opt/usr/local/halo; do
        [ -e "$d" ] || continue
        chown -R root:root "$d" 2>/dev/null || true
        find "$d" -type d -exec chmod 755 {} + 2>/dev/null || true
        find "$d" -type f -exec chmod 644 {} + 2>/dev/null || true
    done
    for d in \
        /opt/usr/local/blog/bin \
        /opt/usr/local/blog/bin/admin \
        /opt/usr/local/tbox/bin \
        /opt/usr/local/waiwei/bin \
        /opt/usr/local/waiwei/scripts \
        /opt/usr/local/xray/bin \
        /opt/usr/local/shadowsocks/bin \
        /opt/usr/local/halo/bin \
        /opt/usr/local/bin
    do
        [ -d "$d" ] && find "$d" -type f -exec chmod 755 {} + 2>/dev/null || true
    done
    for d in /opt/usr/local /opt/immich /opt/src /opt/user; do
        [ -d "$d" ] && find "$d" -xdev \( -type d -o -type f \) \( -perm -0002 -o -perm -0020 \) -exec chmod go-w {} + 2>/dev/null || true
    done
}

install_staged_collected_paths() {
    stage="/opt/tmp/auto_sync_deploy_stage"
    [ -d "$stage" ] || return 0
    log "install collected paths after package installation"
    cp -a "$stage"/. /
    rc=$?
    if [ -f /tmp/auto_sync_root_key.pub ]; then
        mkdir -p /root/.ssh
        touch /root/.ssh/authorized_keys
        grep -qxF -f /tmp/auto_sync_root_key.pub /root/.ssh/authorized_keys 2>/dev/null || cat /tmp/auto_sync_root_key.pub >> /root/.ssh/authorized_keys
        rm -f /tmp/auto_sync_root_key.pub
    fi
    chmod 755 / /etc /usr /usr/bin 2>/dev/null || true
    chown -R root:root /root/.ssh 2>/dev/null || true
    chmod 700 /root/.ssh 2>/dev/null || true
    find /root/.ssh -type f -name "id_*" ! -name "*.pub" -exec chmod 600 {} \; 2>/dev/null || true
    find /root/.ssh -type f -name "*.pub" -exec chmod 644 {} \; 2>/dev/null || true
    chmod 600 /root/.ssh/config 2>/dev/null || true
    chmod 644 /root/.ssh/known_hosts /root/.ssh/known_hosts.old 2>/dev/null || true
    chmod 600 /root/.ssh/authorized_keys 2>/dev/null || true
    rm -rf "$stage"
    [ "$rc" -eq 0 ] && log "installed collected paths"
    return "$rc"
}

if [ -f /etc/apt/sources.list.d/gitlab_gitlab-ce.list ] && grep -q '/ubuntu/resolute\| resolute ' /etc/apt/sources.list.d/gitlab_gitlab-ce.list; then
    log "GitLab CE has no resolute repo yet; use noble package repo"
    sed -i 's#/ubuntu/resolute#/ubuntu/noble#g; s/ resolute / noble /g' /etc/apt/sources.list.d/gitlab_gitlab-ce.list
fi

if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
    sed -i 's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' /etc/apt/sources.list.d/ubuntu.sources
fi
apt-get update
policy_created=0
if [ ! -e /usr/sbin/policy-rc.d ]; then
    printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
    chmod 0755 /usr/sbin/policy-rc.d
    policy_created=1
fi
apt-get -o Dpkg::Options::=--force-confold purge -y vim || true
apt-get autoremove -y || true
curl_dev_version="$(apt-cache policy libcurl4-openssl-dev 2>/dev/null | awk '/Candidate:/ {print $2; exit}')"
if [ -n "$curl_dev_version" ] && [ "$curl_dev_version" != "(none)" ]; then
    apt-get install -y --allow-downgrades "curl=$curl_dev_version" "libcurl4t64=$curl_dev_version" "libcurl4-openssl-dev=$curl_dev_version" 2>/dev/null ||
        apt-get install -y --allow-downgrades "curl=$curl_dev_version" "libcurl4=$curl_dev_version" "libcurl4-openssl-dev=$curl_dev_version" 2>/dev/null ||
        log "WARN: libcurl dev version alignment failed"
fi
apt_result=0
apt-get -o Dpkg::Options::=--force-confold install -y \
    dialog zfsutils-linux openssh-server zsh net-tools curl aria2 iputils-ping iftop cron ca-certificates xfonts-utils dnsutils hdparm unzip \
    autoconf libtool libtool-bin cmake make gcc g++ texinfo bison flex gdb build-essential automake pkg-config help2man gettext \
    python3-pip ruby luajit netcat-openbsd gperf cscope exuberant-ctags vim-nox git git-lfs lcov graphviz \
    libgeoip-dev libxml2-dev libxslt1.1 libxslt1-dev libatomic-ops-dev libgd-dev libperl-dev libluajit-5.1-dev tcl-dev ruby-dev libncurses-dev \
    mysql-server postgresql postgresql-client libpq-dev postgresql-contrib sysstat nginx-full libnginx-mod-stream \
    ffmpeg imagemagick libheif-examples heif-gdk-pixbuf libimage-exiftool-perl libgl1 libvips-dev libvips-tools openjdk-21-jdk-headless postgresql-server-dev-all redis quota || apt_result=1
DEBIAN_FRONTEND=noninteractive dpkg --force-confold --configure -a || apt_result=1
[ "$policy_created" -eq 0 ] || rm -f /usr/sbin/policy-rc.d
[ "$apt_result" -eq 0 ] || { log "ERROR: package installation/configuration failed"; exit 1; }
restore_system_curl() {
    if dpkg-divert --list /usr/bin/curl 2>/dev/null | grep -q '/usr/bin/curl'; then
        rm -f /usr/bin/curl /bin/curl 2>/dev/null || true
        dpkg-divert --rename --remove /usr/bin/curl 2>/dev/null || true
    fi
    if [ ! -x /usr/bin/curl ] || [ -L /usr/bin/curl ]; then
        rm -f /usr/bin/curl /bin/curl 2>/dev/null || true
        apt-get install --reinstall -y curl >/dev/null 2>&1 || true
    fi
    if [ ! -x /usr/bin/curl ] && [ -x /usr/bin/curl.distrib ]; then
        cp -f /usr/bin/curl.distrib /usr/bin/curl
        chmod 0755 /usr/bin/curl
    fi
    hash -r 2>/dev/null || true
}
restore_system_curl
chmod 755 /usr /usr/local 2>/dev/null || true
[ ! -d /usr/local/bin ] || chmod 755 /usr/local/bin 2>/dev/null || true
[ ! -d /usr/local/sbin ] || chmod 755 /usr/local/sbin 2>/dev/null || true
apt-get remove -y apport || true
apt-get autoremove -y || true
rm -f /etc/nginx/sites-enabled/default /etc/nginx/conf.d/default.conf 2>/dev/null || true
stop_services_before_install
install_staged_collected_paths || { log "ERROR: collected path installation failed"; exit 1; }

id tiger >/dev/null 2>&1 || useradd -m -s /bin/bash tiger
ensure_software_src_layout() {
    new=/opt/src/software
    old=/opt/software/src
    mkdir -p /opt/src /opt/software
    if [ -e "$old" ] && [ ! -L "$old" ]; then
        if [ ! -e "$new" ]; then
            mv "$old" "$new"
        else
            cp -a "$old"/. "$new"/
            mv "$old" "${old}.migrated-$(date +%Y%m%d%H%M%S)"
        fi
    fi
    mkdir -p "$new"
    if [ -L "$old" ] && [ "$(readlink "$old")" != "$new" ]; then
        rm -f "$old"
    fi
}
ensure_software_src_layout
rewrite_software_src_paths() {
    replacement="$1"
    for root in /etc/systemd/system /etc/profile.d /opt/usr/local/bin; do
        [ -e "$root" ] || continue
        find "$root" -type f -exec grep -Il '/opt/software/src' {} + 2>/dev/null |
            xargs -r sed -i "s#/opt/software/src#$replacement#g"
    done
    [ ! -f /etc/profile ] || sed -i "s#/opt/software/src#$replacement#g" /etc/profile
}
rewrite_software_src_paths /opt/src/software
ensure_opt_link() {
    path="$1"
    target="$2"
    kind="$3"
    owner="$4"
    if [ -L "$path" ] && [ "$(readlink "$path")" = "$target" ]; then
        if [ "$kind" = dir ]; then mkdir -p "$target"; else touch "$target"; fi
        return 0
    fi
    mkdir -p "$(dirname "$path")" "$(dirname "$target")"
    if [ "$kind" = dir ]; then
        mkdir -p "$target"
        if [ -d "$path" ] && [ ! -L "$path" ]; then
            cp -a "$path"/. "$target"/
        elif [ -L "$path" ] && [ -d "$path" ]; then
            cp -aL "$path"/. "$target"/
        fi
    else
        if [ -f "$path" ] || { [ -L "$path" ] && [ -f "$path" ]; }; then
            cp -aL "$path" "$target"
        elif [ ! -e "$target" ]; then
            touch "$target"
        fi
    fi
    rm -rf "$path"
    ln -s "$target" "$path"
    chown -h "$owner" "$path" 2>/dev/null || true
    case "$target" in /opt/user/*) chown -h "$owner" "$target" 2>/dev/null || true ;; esac
}
while IFS='|' read -r path target kind owner; do
    [ -n "$path" ] && ensure_opt_link "$path" "$target" "$kind" "$owner"
done <<'EOF_OPT_LINKS'
/root/.bashrc|/opt/user/root/.bashrc|file|root:root
/root/.cache|/opt/user/root/.cache|dir|root:root
/root/.cargo|/opt/user/root/.cargo|dir|root:root
/root/.codex|/opt/user/root/.codex|dir|root:root
/root/.config|/opt/user/root/.config|dir|root:root
/root/.cscope.vim|/opt/user/root/.cscope.vim|file|root:root
/root/.dotnet|/opt/user/root/.dotnet|dir|root:root
/root/.launchpadlib|/opt/user/root/.launchpadlib|dir|root:root
/root/.local|/opt/user/root/.local|dir|root:root
/root/.npm|/opt/user/root/.npm|dir|root:root
/root/.npmrc|/opt/user/root/.npmrc|file|root:root
/root/.nvm|/opt/user/root/.nvm|dir|root:root
/root/.oh-my-zsh|/opt/user/root/.oh-my-zsh|dir|root:root
/root/.profile|/opt/user/root/.profile|file|root:root
/root/.rustup|/opt/user/root/.rustup|dir|root:root
/root/.vim|/opt/user/root/.vim|dir|root:root
/root/.vimbackup|/opt/user/root/.vimbackup|dir|root:root
/root/.vimswap|/opt/user/root/.vimswap|dir|root:root
/root/.vimundo|/opt/user/root/.vimundo|dir|root:root
/root/.vimviews|/opt/user/root/.vimviews|dir|root:root
/root/.vscode-server|/opt/user/root/.vscode-server|dir|root:root
/root/.halo2|/opt/user/root/.halo2|dir|root:root
/root/.zprofile|/opt/user/root/.zprofile|file|root:root
/root/.zshenv|/opt/user/root/.zshenv|file|root:root
/root/.zshrc|/opt/user/root/.zshrc|file|root:root
/root/src|/opt/user/root/src|dir|root:root
/home/tiger/.bashrc|/opt/user/tiger/.bashrc|file|tiger:tiger
/home/tiger/.oh-my-zsh|/opt/user/tiger/.oh-my-zsh|dir|tiger:tiger
/home/tiger/.profile|/opt/user/tiger/.profile|file|tiger:tiger
/home/tiger/.zprofile|/opt/user/tiger/.zprofile|file|tiger:tiger
/home/tiger/.zshenv|/opt/user/tiger/.zshenv|file|tiger:tiger
/home/tiger/.zshrc|/opt/user/tiger/.zshrc|file|tiger:tiger
EOF_OPT_LINKS

rm -rf /home/tiger/.nvm /home/tiger/.npm /home/tiger/.npmrc \
       /opt/user/tiger/.nvm /opt/user/tiger/.npm /opt/user/tiger/.npmrc 2>/dev/null || true

ensure_var_bind_mounts() {
    while IFS='|' read -r src dst owner mode; do
        [ -n "$src" ] || continue
        mkdir -p "$src" "$dst"
        if ! grep -Eq "^$src[[:space:]]+$dst[[:space:]]+none[[:space:]]+bind" /etc/fstab 2>/dev/null; then
            printf '%s %s none bind,x-systemd.requires=opt.mount,x-systemd.after=opt.mount 0 0\n' "$src" "$dst" >> /etc/fstab
        fi
        if ! mountpoint -q "$dst"; then
            mount "$dst" 2>/dev/null || mount --bind "$src" "$dst"
        fi
        chown "$owner" "$src" "$dst" 2>/dev/null || true
        chmod "$mode" "$src" "$dst" 2>/dev/null || true
    done <<'EOF_VAR_BINDS'
/opt/var/lib/postgresql|/var/lib/postgresql|postgres:postgres|755
/opt/var/lib/mysql|/var/lib/mysql|mysql:mysql|700
/opt/var/opt|/var/opt|root:root|755
/opt/var/cache|/var/cache|root:root|755
/opt/var/log|/var/log|root:root|755
/opt/var/lib/apt|/var/lib/apt|root:root|755
EOF_VAR_BINDS
    mkdir -p /var/log/postgresql
    chown root:postgres /var/log/postgresql 2>/dev/null || true
    chmod 2775 /var/log/postgresql 2>/dev/null || true
}
ensure_var_bind_mounts

if [ -f /usr/share/nginx/modules-available/mod-stream.conf ]; then
    mkdir -p /etc/nginx/modules-enabled
    ln -sfn /usr/share/nginx/modules-available/mod-stream.conf /etc/nginx/modules-enabled/50-mod-stream.conf
fi
if [ ! -f /usr/lib/nginx/modules/ngx_stream_module.so ] || [ ! -e /etc/nginx/modules-enabled/50-mod-stream.conf ]; then
    log "ERROR: nginx stream module is not installed or enabled"
    exit 1
fi

fix_postgresql_config_permissions() {
    pg_fix_major="${1:-}"
    if [ -z "$pg_fix_major" ]; then
        pg_fix_major="$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -V | tail -1)"
    fi
    [ -n "$pg_fix_major" ] || return 0
    [ -d "/etc/postgresql/$pg_fix_major" ] && chown -R postgres:postgres "/etc/postgresql/$pg_fix_major" 2>/dev/null || true
    [ -d "/var/lib/postgresql/$pg_fix_major" ] && chown -R postgres:postgres "/var/lib/postgresql/$pg_fix_major" 2>/dev/null || true
    find "/etc/postgresql/$pg_fix_major" -type d -exec chmod 755 {} + 2>/dev/null || true
    find "/etc/postgresql/$pg_fix_major" -type f \( -name 'pg_hba.conf' -o -name 'pg_ident.conf' \) -exec chmod 640 {} + 2>/dev/null || true
    find "/etc/postgresql/$pg_fix_major" -type f ! \( -name 'pg_hba.conf' -o -name 'pg_ident.conf' \) -exec chmod 644 {} + 2>/dev/null || true
}

ensure_postgresql_cluster() {
    pg_major="$(pg_config --version | sed -n 's/^PostgreSQL \([0-9][0-9]*\).*/\1/p')"
    [ -n "$pg_major" ] || { log "ERROR: cannot detect PostgreSQL major version"; return 1; }

    normalize_pg_config() {
        cfg_dir="/etc/postgresql/$pg_major/main"
        [ -f "$cfg_dir/postgresql.conf" ] || return 0
        sed -i -E \
            -e "s#^[[:space:]]*data_directory[[:space:]]*=.*#data_directory = '/var/lib/postgresql/$pg_major/main'#" \
            -e "s#^[[:space:]]*hba_file[[:space:]]*=.*#hba_file = '/etc/postgresql/$pg_major/main/pg_hba.conf'#" \
            -e "s#^[[:space:]]*ident_file[[:space:]]*=.*#ident_file = '/etc/postgresql/$pg_major/main/pg_ident.conf'#" \
            -e "s#^[[:space:]]*external_pid_file[[:space:]]*=.*#external_pid_file = '/var/run/postgresql/$pg_major-main.pid'#" \
            -e "s#^[[:space:]]*cluster_name[[:space:]]*=.*#cluster_name = '$pg_major/main'#" \
            "$cfg_dir/postgresql.conf" 2>/dev/null || true
    }

    restore_pg_config_if_missing() {
        cfg_dir="/etc/postgresql/$pg_major/main"
        [ -f "$cfg_dir/postgresql.conf" ] && return 0
        source_dir="$(find /etc/postgresql -path '*/main/postgresql.conf' -printf '%h\n' 2>/dev/null | grep -v "^$cfg_dir$" | sort -V | tail -1)"
        [ -n "$source_dir" ] || return 0
        mkdir -p "$cfg_dir"
        cp -a "$source_dir/." "$cfg_dir/"
        source_major="$(basename "$(dirname "$source_dir")")"
        if [ -n "$source_major" ] && [ "$source_major" != "$pg_major" ]; then
            sed -i "s#/postgresql/$source_major/#/postgresql/$pg_major/#g; s#postgresql-$source_major#postgresql-$pg_major#g" "$cfg_dir"/*.conf 2>/dev/null || true
        fi
        normalize_pg_config
    }

    if [ -s "/var/lib/postgresql/$pg_major/main/PG_VERSION" ]; then
        restore_pg_config_if_missing
        normalize_pg_config
        fix_postgresql_config_permissions "$pg_major"
        systemctl restart "postgresql@$pg_major-main.service" 2>/dev/null || systemctl restart postgresql 2>/dev/null || true
        pg_isready -q || { log "ERROR: existing PostgreSQL data directory is present but cluster $pg_major/main is not ready"; return 1; }
        return 0
    fi

    source_major="$(find /etc/postgresql -mindepth 1 -maxdepth 1 -type d -printf '%f\n' 2>/dev/null | grep -E '^[0-9]+$' | sort -V | tail -1)"
    source_copy="$(mktemp -d)"
    if [ -n "$source_major" ] && [ -d "/etc/postgresql/$source_major/main" ]; then
        cp -a "/etc/postgresql/$source_major/main/." "$source_copy/"
    fi
    for old_dir in /etc/postgresql/[0-9]*; do
        [ -d "$old_dir" ] || continue
        old_major="${old_dir##*/}"
        [ "$old_major" = "$pg_major" ] && continue
        mv "$old_dir" "/etc/postgresql/.auto_sync_saved_${old_major}_$(date +%Y%m%d%H%M%S)" 2>/dev/null || true
    done
    mkdir -p "/var/lib/postgresql/$pg_major"
    chown postgres:postgres /var/lib/postgresql "/var/lib/postgresql/$pg_major" 2>/dev/null || true
    chmod 755 /var/lib/postgresql "/var/lib/postgresql/$pg_major" 2>/dev/null || true
    rm -rf "/etc/postgresql/$pg_major/main"
    pg_createcluster --port 5432 "$pg_major" main --start
    if [ -f "$source_copy/postgresql.conf" ]; then
        for config in postgresql.conf pg_hba.conf pg_ident.conf pg_ctl.conf start.conf environment; do
            [ -f "$source_copy/$config" ] && cp -a "$source_copy/$config" "/etc/postgresql/$pg_major/main/$config"
        done
        if [ -n "$source_major" ] && [ "$source_major" != "$pg_major" ]; then
            sed -i "s#/postgresql/$source_major/#/postgresql/$pg_major/#g; s#postgresql-$source_major#postgresql-$pg_major#g" /etc/postgresql/$pg_major/main/*.conf 2>/dev/null || true
        fi
        normalize_pg_config
    fi
    rm -rf "$source_copy"
    fix_postgresql_config_permissions "$pg_major"
    systemctl restart "postgresql@$pg_major-main.service"
    pg_isready -q || { log "ERROR: PostgreSQL cluster $pg_major/main is not ready"; return 1; }
}
ensure_postgresql_cluster || exit 1

cp /usr/share/zoneinfo/Asia/Shanghai /etc/localtime

ensure_host_entry() {
    expected_ip="$1"
    host="$2"
    tmp="$(mktemp)"
    touch /etc/hosts
    awk -v expected_ip="$expected_ip" -v host="$host" '
        /^[[:space:]]*#/ || NF == 0 { print; next }
        {
            has_host = 0
            for (i = 2; i <= NF; i++) {
                if ($i == host) {
                    has_host = 1
                    break
                }
            }
            if (!has_host) {
                print
                next
            }
            if ($1 == expected_ip && found == 0) {
                print
                found = 1
                next
            }
            out = $1
            kept = 0
            for (i = 2; i <= NF; i++) {
                if ($i != host) {
                    out = out " " $i
                    kept = 1
                }
            }
            if (kept) {
                print out
            }
        }
        END {
            if (found == 0) {
                print expected_ip " " host
            }
        }
    ' /etc/hosts > "$tmp"
    cat "$tmp" > /etc/hosts
    rm -f "$tmp"
}
ensure_host_entry 127.0.0.1 code.xiedeacc.com
ensure_host_entry 127.0.0.1 unlock-music.xiedeacc.com
ensure_host_entry 127.0.0.1 immich.xiedeacc.com
ensure_host_entry 127.0.0.1 halo.xiedeacc.com
ensure_host_entry 127.0.0.1 blog.xiedeacc.com
ensure_host_entry 127.0.0.1 rblog.xiedeacc.com
ensure_host_entry 127.0.0.1 dev.xiedeacc.com
ensure_host_entry 127.0.0.1 coverage.xiedeacc.com

swapoff -a || true
sed -i '/^\/swap\.img[[:space:]]/s/^/#/' /etc/fstab 2>/dev/null || true
rm -f /swap.img

cat > /etc/profile.d/auto-sync-domestic-mirrors.sh <<'EOF_DOMESTIC_MIRRORS'
export GOPROXY=https://goproxy.cn,direct
export NVM_NODEJS_ORG_MIRROR=https://npmmirror.com/mirrors/node
export npm_config_registry=https://registry.npmmirror.com
export COREPACK_NPM_REGISTRY=https://registry.npmmirror.com
export PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
export UV_DEFAULT_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple
export UV_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
export PATH="$JAVA_HOME/bin:$PATH"
EOF_DOMESTIC_MIRRORS
. /etc/profile.d/auto-sync-domestic-mirrors.sh
clean_shell_startup_files() {
    for f in /root/.bashrc /root/.zshrc /root/.profile /root/.zprofile /root/.zshenv /home/tiger/.bashrc /home/tiger/.zshrc /home/tiger/.profile /home/tiger/.zprofile /home/tiger/.zshenv; do
        [ -f "$f" ] || continue
        sed -i -E \
            -e '/\/usr\/local\/java|\/usr\/local\/jdk-|\/opt\/software\/src\/tools\/nvm|\/usr\/local\/src\/software/d' \
            -e '/^[[:space:]]*#?[[:space:]]*export[[:space:]]+JAVA_HOME=/d' \
            -e '/^[[:space:]]*export[[:space:]]+CLASSPATH=/d' \
            -e '/JAVA_HOME\/bin|JAVA_HOME\/jre/d' \
            -e '/^[[:space:]]*export[[:space:]]+NVM_DIR="\$HOME\/\.nvm"/,+2d' \
            "$f"
    done
}
clean_shell_startup_files
cat > /etc/pip.conf <<'EOF_PIP_MIRROR'
[global]
index-url = https://pypi.tuna.tsinghua.edu.cn/simple
timeout = 120
EOF_PIP_MIRROR
mkdir -p /root/.cargo
cat > /root/.cargo/config.toml <<'EOF_CARGO_MIRROR'
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF_CARGO_MIRROR

if [ ! -d /root/.oh-my-zsh ]; then
    RUNZSH=no CHSH=no KEEP_ZSHRC=yes sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" || log "WARN: oh-my-zsh install failed"
fi

mkdir -p /opt/usr/local/bin
chmod 0755 /opt/usr /opt/usr/local /opt/usr/local/bin 2>/dev/null || true

if [ ! -x /opt/usr/local/go/go1.25.1/bin/go ]; then
    mkdir -p /opt/usr/local/go
    cd /opt/usr/local/go
    rm -rf go go1.25.1 go1.25.1.linux-amd64.tar.gz
    if curl --retry 5 --retry-all-errors -L -O https://mirrors.aliyun.com/golang/go1.25.1.linux-amd64.tar.gz || curl --retry 5 --retry-all-errors -L -O https://go.dev/dl/go1.25.1.linux-amd64.tar.gz; then
        tar zxf go1.25.1.linux-amd64.tar.gz
        mv go go1.25.1
        rm -f go1.25.1.linux-amd64.tar.gz
    else
        log "WARN: go download failed"
    fi
fi
export GOPATH=/root/src/go
export GOBIN=$GOPATH/bin
mkdir -p "$GOBIN" "$GOPATH/pkg"
export PATH="/opt/usr/local/go/go1.25.1/bin:$GOBIN:$PATH"
if command -v go >/dev/null 2>&1; then
    go install github.com/google/pprof@latest || log "WARN: go install pprof failed"
fi

if [ ! -x /root/.cargo/bin/rustup ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | timeout --kill-after=10s 600s sh -s -- -y || log "WARN: rustup install failed"
fi

mkdir -p /opt/src/software/tools
if [ -L /opt/src/software/tools/nvm ]; then
    rm -f /opt/src/software/tools/nvm
fi
export NVM_DIR=/opt/src/software/tools/nvm
mkdir -p "$NVM_DIR"
if [ ! -s "$NVM_DIR/nvm.sh" ]; then
    curl -fsSL https://gitee.com/mirrors/nvm/raw/v0.40.3/install.sh | NVM_SOURCE=https://gitee.com/mirrors/nvm.git bash || log "WARN: nvm install failed"
fi
if [ -s "$NVM_DIR/nvm.sh" ]; then
    . "$NVM_DIR/nvm.sh"
    nvm install 24.18.0 || nvm install 24 || log "WARN: nvm install 24 failed"
    nvm use 24.18.0 || nvm use 24 || true
    hash -r 2>/dev/null || true
    if ! command -v npm >/dev/null 2>&1; then
        log "npm missing after nvm use; reinstall Node v24.18.0"
        rm -rf "$NVM_DIR/versions/node/v24.18.0"
        nvm install 24.18.0 || nvm install 24 || log "WARN: nvm reinstall 24 failed"
        nvm use 24.18.0 || nvm use 24 || true
        hash -r 2>/dev/null || true
    fi
    node_bin_dir="$(dirname "$(command -v node 2>/dev/null || true)")"
    if [ -n "$node_bin_dir" ] && [ -x "$node_bin_dir/node" ]; then
        export PATH="$node_bin_dir:$PATH"
        npm config set registry "$npm_config_registry" || log "WARN: npm mirror setup failed"
        if command -v corepack >/dev/null 2>&1; then
            corepack enable || log "WARN: corepack enable failed"
            corepack prepare pnpm@latest --activate || log "WARN: corepack prepare pnpm failed"
        fi
        if ! command -v pnpm >/dev/null 2>&1; then
            npm install -g pnpm || log "WARN: npm install pnpm failed"
        fi
        command -v pnpm >/dev/null 2>&1 && pnpm config set registry "$npm_config_registry" || true
    fi
fi
if [ -d "$NVM_DIR" ]; then
    chmod -R a+rX "$NVM_DIR" || true
fi

if [ ! -x /opt/usr/local/bin/buildifier ]; then
    curl -L -o /opt/usr/local/bin/buildifier https://github.com/bazelbuild/buildtools/releases/download/v7.1.2/buildifier-linux-amd64
    chmod +x /opt/usr/local/bin/buildifier
fi

[ -x "$JAVA_HOME/bin/java" ] || { log "ERROR: OpenJDK 21 is not installed at $JAVA_HOME"; exit 1; }

cat > /opt/src/software/tools/auto_sync_install_vim_tools.sh <<'EOF_VIM_TOOLS'
#!/bin/bash
set -u
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
log() { printf '[vim-tools] %s\n' "$*"; }
if [ -f /root/src/share/ubuntu/conf/.vimrc ]; then
    cp /root/src/share/ubuntu/conf/.vimrc /root/.vimrc
fi
mkdir -p /root/.vim/bundle
if [ ! -d /root/.vim/bundle/Vundle.vim ]; then
    git clone https://github.com/VundleVim/Vundle.vim.git /root/.vim/bundle/Vundle.vim || true
fi
if command -v vim >/dev/null 2>&1 && [ -f /root/.vimrc ]; then
    vimrc_hash="$(sha256sum /root/.vimrc | awk '{print $1}')"
    if [ "$(cat /root/.vim/.auto_sync_vundle_hash 2>/dev/null || true)" != "$vimrc_hash" ]; then
        vundle_rc="$(mktemp)"
        sed -n '1,2p' /root/.vimrc > "$vundle_rc"
        sed -n '/^filetype off$/,/^call vundle#end()/p' /root/.vimrc >> "$vundle_rc"
        vundle_exit=0
        timeout --kill-after=10s 600s vim -Nu "$vundle_rc" -n -es -i NONE '+set nomore' '+PluginInstall' '+qall' </dev/null || vundle_exit=$?
        declared_plugins="$(grep -Ec "^[[:space:]]*Plugin[[:space:]]+'" /root/.vimrc || true)"
        installed_plugins="$(find /root/.vim/bundle -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
        if [ "$installed_plugins" -ge "$declared_plugins" ] &&
           [ -d /root/.vim/bundle/Vundle.vim ] &&
           [ -d /root/.vim/bundle/YouCompleteMe ] &&
           [ -d /root/.vim/bundle/vim-glaive ]; then
            printf '%s\n' "$vimrc_hash" > /root/.vim/.auto_sync_vundle_hash
            [ "$vundle_exit" -eq 0 ] || log "PluginInstall returned $vundle_exit; all $declared_plugins declared plugins are present"
        else
            log "WARN: vim plugins incomplete (declared=$declared_plugins installed=$installed_plugins exit=$vundle_exit)"
        fi
        rm -f "$vundle_rc"
    fi
fi
if [ ! -d /root/.vim/bundle/vim-airline-fonts ]; then
    git clone https://github.com/powerline/fonts /root/.vim/bundle/vim-airline-fonts || true
fi
if [ -x /root/.vim/bundle/vim-airline-fonts/install.sh ]; then
    (cd /root/.vim/bundle/vim-airline-fonts && ./install.sh) || true
fi
if [ -d /root/.vim/bundle/YouCompleteMe ]; then
    ycm_commit="$(git -C /root/.vim/bundle/YouCompleteMe rev-parse HEAD 2>/dev/null || true)"
    if [ -n "$ycm_commit" ] && [ "$(cat /root/.vim/bundle/YouCompleteMe/.auto_sync_installed 2>/dev/null || true)" != "$ycm_commit" ]; then
        ycm_path="$JAVA_HOME/bin:/opt/usr/local/go/go1.25.1/bin:/root/src/go/bin:/root/.cargo/bin:/opt/src/software/tools/nvm/versions/node/v24.18.0/bin:$PATH"
        ycmd_build=/root/.vim/bundle/YouCompleteMe/third_party/ycmd/build.py
        jdt_milestone="$(sed -n "s/^JDTLS_MILESTONE = '\([^']*\)'.*/\1/p" "$ycmd_build" | head -1)"
        jdt_stamp="$(sed -n "s/^JDTLS_BUILD_STAMP = '\([^']*\)'.*/\1/p" "$ycmd_build" | head -1)"
        if command -v aria2c >/dev/null 2>&1 && [ -n "$jdt_milestone" ] && [ -n "$jdt_stamp" ]; then
            jdt_package="jdt-language-server-$jdt_milestone-$jdt_stamp.tar.gz"
            jdt_cache=/root/.vim/bundle/YouCompleteMe/third_party/ycmd/third_party/eclipse.jdt.ls/target/cache
            mkdir -p "$jdt_cache"
            timeout --kill-after=10s 300s aria2c --allow-overwrite=true --auto-file-renaming=false --continue=true -x 16 -s 16 -k 1M \
                -d "$jdt_cache" -o "$jdt_package" \
                "https://download.eclipse.org/jdtls/milestones/$jdt_milestone/$jdt_package" || log "WARN: JDT.LS cache prefetch failed"
        fi
        if (cd /root/.vim/bundle/YouCompleteMe && git submodule update --init --recursive && PATH="$ycm_path" timeout --kill-after=10s 900s python3 install.py --all --force-sudo); then
            printf '%s\n' "$ycm_commit" > /root/.vim/bundle/YouCompleteMe/.auto_sync_installed
        else
            log "WARN: YouCompleteMe install failed"
        fi
    fi
fi
EOF_VIM_TOOLS
chmod 0755 /opt/src/software/tools/auto_sync_install_vim_tools.sh
rm -f /usr/local/sbin/auto_sync_install_vim_tools.sh 2>/dev/null || true
rmdir /usr/local/sbin 2>/dev/null || true
if ! pgrep -f '/opt/src/software/tools/auto_sync_install_vim_tools.sh' >/dev/null 2>&1; then
    log "start Vim plugin/YouCompleteMe installation in background"
    nohup /opt/src/software/tools/auto_sync_install_vim_tools.sh >> /var/log/auto_sync_vim_tools.log 2>&1 &
fi

grep -q '^\*[[:space:]]\+soft[[:space:]]\+core[[:space:]]\+unlimited' /etc/security/limits.conf || cat >> /etc/security/limits.conf <<'EOF_LIMITS'
*       soft    core    unlimited
*       hard    core    unlimited
EOF_LIMITS
grep -q '^kernel.core_pattern=/var/crash/core.%e.%p.%t' /etc/sysctl.conf || echo 'kernel.core_pattern=/var/crash/core.%e.%p.%t' >> /etc/sysctl.conf
mkdir -p /var/crash /etc/systemd
sysctl -p /etc/sysctl.conf || true
cat > /etc/systemd/coredump.conf <<'EOF_COREDUMP'
[Coredump]
Storage=external
ProcessSizeMax=unlimited
ExternalSizeMax=unlimited
EOF_COREDUMP

mkdir -p /opt/src/software
if [ -d /root/src/software/pgvector ]; then
    if [ ! -e /opt/src/software/pgvector ]; then
        mv /root/src/software/pgvector /opt/src/software/pgvector
    else
        cp -a /root/src/software/pgvector/. /opt/src/software/pgvector/
        rm -rf /root/src/software/pgvector
    fi
fi
if [ ! -d /opt/src/software/pgvector ]; then
    git clone https://github.com/pgvector/pgvector.git /opt/src/software/pgvector || true
fi
if [ -d /opt/src/software/pgvector ]; then
    (cd /opt/src/software/pgvector && make && make install) || log "WARN: pgvector build/install failed"
fi

deploy_immich_from_git() {
    mkdir -p /opt/src/software
    mkdir -p /opt/immich/server /opt/immich/web /opt/immich/upload /opt/immich/machine-learning /opt/immich/conf
    if [ ! -s /opt/immich/conf/immich-ml.env ]; then
        cat > /opt/immich/conf/immich-ml.env <<'EOF_IMMICH_ML_ENV'
IMMICH_HOST=0.0.0.0
IMMICH_PORT=3003
IMMICH_LOG_LEVEL=log
MACHINE_LEARNING_CACHE_FOLDER=/opt/immich/machine-learning/.cache
TRANSFORMERS_CACHE=/opt/immich/machine-learning/.cache
EOF_IMMICH_ML_ENV
        chmod 0640 /opt/immich/conf/immich-ml.env
    fi
    mkdir -p /root/.ssh
    ssh-keyscan -T 10 github.com >> /root/.ssh/known_hosts 2>/dev/null || true
    export GIT_SSH_COMMAND='ssh -o StrictHostKeyChecking=accept-new'
    repo=/opt/src/software/immich
    if [ -e "$repo" ] && [ ! -d "$repo/.git" ]; then
        mv "$repo" "${repo}.before-git-$(date +%Y%m%d%H%M%S)" || return 1
    fi
    if [ ! -d "$repo/.git" ]; then
        git clone --branch deploy git@github.com:xiedeacc/immich.git "$repo" || return 1
    fi
    (
        cd "$repo" &&
        git remote set-url origin git@github.com:xiedeacc/immich.git &&
        (git checkout -- deploy.sh 2>/dev/null || true) &&
        (git checkout -- scripts/deploy.sh 2>/dev/null || true) &&
        (git checkout -- install.sh 2>/dev/null || true) &&
        git fetch origin deploy &&
        git checkout deploy &&
        git pull --ff-only origin deploy
    ) || return 1
    immich_commit="$(git -C "$repo" rev-parse HEAD 2>/dev/null || true)"
    immich_marker=/opt/immich/.auto_sync_deploy_commit
    immich_installed=0
    if [ -f /opt/immich/server/dist/main.js ] &&
       [ -f /opt/immich/web/build/index.html ] &&
       [ -x /opt/immich/machine-learning/.venv/bin/python ]; then
        immich_installed=1
    fi
    if [ -n "$immich_commit" ] && [ "$immich_installed" -eq 1 ]; then
        if [ "$(cat "$immich_marker" 2>/dev/null || true)" = "$immich_commit" ]; then
            log "immich deploy branch unchanged; skip rebuild"
            return 0
        fi
        if [ ! -f "$immich_marker" ]; then
            printf '%s\n' "$immich_commit" > "$immich_marker"
            log "immich already installed; record current deploy commit and skip rebuild"
            return 0
        fi
    fi
    for script in deploy.sh scripts/deploy.sh install.sh; do
        if [ -f "$repo/$script" ]; then
            chmod +x "$repo/$script" 2>/dev/null || true
            immich_node_home="${IMMICH_NODE_HOME:-/opt/src/software/tools/nvm/versions/node/v24.18.0}"
            (cd "$repo" && VERSION="${IMMICH_VERSION:-deploy}" UV_DEFAULT_INDEX="${UV_DEFAULT_INDEX:-https://pypi.tuna.tsinghua.edu.cn/simple}" UV_INDEX_URL="${UV_INDEX_URL:-https://pypi.tuna.tsinghua.edu.cn/simple}" PIP_INDEX_URL="${PIP_INDEX_URL:-https://pypi.tuna.tsinghua.edu.cn/simple}" npm_config_node_gyp="$immich_node_home/lib/node_modules/npm/node_modules/node-gyp/bin/node-gyp.js" bash "$script") || return 1
            [ -z "$immich_commit" ] || printf '%s\n' "$immich_commit" > "$immich_marker"
            return 0
        fi
    done
    log "WARN: immich deploy script not found in $repo"
    return 1
}
setquota -u root 20000000 20000000 0 0 /dev/mmcblk0p2 2>/dev/null || true
for u in tiger git gitlab immich; do
    id "$u" >/dev/null 2>&1 && setquota -u "$u" 1000000 1000000 0 0 / 2>/dev/null || true
done

ensure_zfs_for_gitlab() {
    if ! command -v zpool >/dev/null 2>&1; then
        log "WARN: zpool not installed; cannot ensure GitLab /zfs storage"
        return 0
    fi
    log "ensure /zfs is imported and mounted before GitLab setup"
    for pool in zfs zfs_pool ssd; do
        if ! zpool list -H "$pool" >/dev/null 2>&1; then
            zpool import "$pool" >/dev/null 2>&1 || true
        fi
    done
    zfs mount zfs >/dev/null 2>&1 || zfs mount -a >/dev/null 2>&1 || true
    zpool status zfs >/dev/null 2>&1 || log "WARN: zfs pool is not online"
    if ! findmnt -T /zfs >/dev/null 2>&1; then
        log "WARN: /zfs is not mounted"
    fi
    ls -ald /zfs >/dev/null 2>&1 || true
}

ensure_gitlab_zfs_config() {
    [ -f /etc/gitlab/gitlab.rb ] || return 0
    if ! grep -q '/zfs/gitlab_data' /etc/gitlab/gitlab.rb 2>/dev/null; then
        log "add GitLab /zfs data storage config"
        cat >> /etc/gitlab/gitlab.rb <<'EOF_GITLAB_ZFS'

# Managed by auto_sync NAS deployment: keep GitLab repository/LFS data on /zfs.
gitlab_rails['lfs_enabled'] = true
gitlab_rails['lfs_storage_path'] = "/zfs/gitlab_data/lfs-objects"
git_data_dirs({
   "default" => {"path" => "/zfs/gitlab_data"},
})
gitlab_rails['repository_storages'] = {
  'default' => {
    'path' => '/zfs/gitlab_data',
    'gitaly_address' => 'unix:/var/opt/gitlab/gitaly/gitaly.socket'
  }
}
EOF_GITLAB_ZFS
    fi
    if ! grep -q 'auto_sync GitLab frontend ports' /etc/gitlab/gitlab.rb 2>/dev/null; then
        cat >> /etc/gitlab/gitlab.rb <<'EOF_GITLAB_FRONTEND'

# Managed by auto_sync GitLab frontend ports.
# Workhorse serves nginx on 10080. Puma must not bind TCP 8080 because waiwei-web uses it.
gitlab_rails['gitlab_shell_ssh_port'] = 10022
gitlab_workhorse['listen_network'] = "tcp"
gitlab_workhorse['listen_addr'] = "127.0.0.1:10080"
puma['listen'] = ""
puma['port'] = nil
EOF_GITLAB_FRONTEND
    fi
    if grep -q "gitlab_rails\['gitlab_shell_ssh_port'\]" /etc/gitlab/gitlab.rb 2>/dev/null; then
        sed -i -E "s/^#?[[:space:]]*gitlab_rails\['gitlab_shell_ssh_port'\].*/gitlab_rails['gitlab_shell_ssh_port'] = 10022/" /etc/gitlab/gitlab.rb
    else
        cat >> /etc/gitlab/gitlab.rb <<'EOF_GITLAB_SSH_PORT'

# Managed by auto_sync GitLab SSH clone port.
gitlab_rails['gitlab_shell_ssh_port'] = 10022
EOF_GITLAB_SSH_PORT
    fi
}

ensure_gitlab_repo_compatible() {
    list="/etc/apt/sources.list.d/gitlab_gitlab-ce.list"
    key=/etc/apt/keyrings/gitlab_gitlab-ce-archive-keyring.asc
    mkdir -p /etc/apt/keyrings
    for attempt in 1 2 3 4 5; do
        if curl -fsSL --connect-timeout 30 --retry 5 --retry-delay 5 --retry-all-errors \
            https://packages.gitlab.com/gitlab/gitlab-ce/gpgkey -o "$key"; then
            break
        fi
        log "WARN: GitLab repo key download failed, attempt $attempt"
        sleep $((attempt * 5))
    done
    [ -s "$key" ] || { log "ERROR: GitLab repo key download failed"; return 1; }
    printf 'deb [signed-by=%s] https://packages.gitlab.com/gitlab/gitlab-ce/ubuntu/ noble main\n' "$key" > "$list"
}

ensure_zfs_for_gitlab
installed_gitlab_version="$(dpkg-query -W -f='${Version}' gitlab-ce 2>/dev/null || true)"
if [ "$installed_gitlab_version" != "$GITLAB_CE_VERSION" ]; then
    ensure_gitlab_repo_compatible || exit 1
    apt-get update || exit 1
    apt-get install -y --allow-downgrades "gitlab-ce=$GITLAB_CE_VERSION" || exit 1
fi
ensure_zfs_for_gitlab
mkdir -p /zfs/gitlab_data /zfs/gitlab_data/lfs-objects /zfs/gitlab_data/repositories 2>/dev/null || true
passwd -S git 2>/dev/null || true
if id git >/dev/null 2>&1; then
    git_pw_hash="$(openssl passwd -6 "auto-sync-git-$(date +%s)-$RANDOM" 2>/dev/null || true)"
    [ -z "$git_pw_hash" ] || usermod -p "$git_pw_hash" git 2>/dev/null || true
    rm -f /etc/ssh/sshd_config.d/98-auto-sync-gitlab.conf 2>/dev/null || true
fi
[ -d /zfs/gitlab_data ] && chown git:git /zfs/gitlab_data 2>/dev/null || true
[ -d /zfs/gitlab_data/lfs-objects ] && chown git:git /zfs/gitlab_data/lfs-objects 2>/dev/null || true
[ -d /zfs/gitlab_data/repositories ] && chown git:git /zfs/gitlab_data/repositories 2>/dev/null || true
[ -d /zfs/gitlab_data ] && chmod 2770 /zfs/gitlab_data /zfs/gitlab_data/lfs-objects /zfs/gitlab_data/repositories 2>/dev/null || true
[ -d /etc/gitlab ] && chown -R git:git /etc/gitlab 2>/dev/null || true
[ -d /etc/gitlab ] && chmod 700 /etc/gitlab 2>/dev/null || true
ensure_gitlab_zfs_config
if command -v gitlab-ctl >/dev/null 2>&1; then
    gitlab-ctl reconfigure || log "WARN: gitlab reconfigure failed"
    gitlab-ctl restart || log "WARN: gitlab restart failed"
    gitlab-ctl status || true
fi

mkdir -p /root/src/share/ubuntu
if [ ! -f /root/src/share/ubuntu/backup_pg.sh ]; then
    cat > /root/src/share/ubuntu/backup_pg.sh <<'EOF_BACKUP_PG'
#!/bin/bash
set -u
BACKUP_DIR="/zfs/backup/pg_backup"
DATE=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="$BACKUP_DIR/pg_full_backup_$DATE.sql"
LOG_FILE="/var/log/pg_backup.log"

log_message() {
    echo "$(date '+%Y-%m-%d %H:%M:%S'): $1" | tee -a "$LOG_FILE"
}

mkdir -p "$BACKUP_DIR"
chown -R postgres:postgres "$BACKUP_DIR" 2>/dev/null || true
log_message "Starting PostgreSQL backup..."

if su - postgres -c "/usr/bin/pg_dumpall > '$BACKUP_FILE'"; then
    if [[ -s "$BACKUP_FILE" ]]; then
        log_message "Backup successful: $BACKUP_FILE ($(du -h "$BACKUP_FILE" | cut -f1))"
        find "$BACKUP_DIR" -name "pg_full_backup_*.sql" -mtime +7 -delete
        log_message "Cleaned up old backups (kept last 7 days)"
        exit 0
    fi
    log_message "ERROR: Backup file is empty or was not created"
    rm -f "$BACKUP_FILE"
    exit 1
fi
log_message "ERROR: pg_dumpall command failed"
rm -f "$BACKUP_FILE"
exit 1
EOF_BACKUP_PG
fi
if [ ! -f /root/src/share/ubuntu/backup_mysql.sh ]; then
    cat > /root/src/share/ubuntu/backup_mysql.sh <<'EOF_BACKUP_MYSQL'
#!/bin/bash
set -u
BACKUP_DIR="/zfs/backup/mysql_backup"
DATE=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="$BACKUP_DIR/mysql_full_backup_$DATE.sql"
LOG_FILE="/var/log/mysql_backup.log"

log_message() {
    echo "$(date '+%Y-%m-%d %H:%M:%S'): $1" | tee -a "$LOG_FILE"
}

mkdir -p "$BACKUP_DIR"
log_message "Starting MySQL backup..."

if /usr/bin/mysqldump -uroot --default-character-set=utf8mb4 -q --lock-all-tables --flush-logs -E -R --triggers --all-databases > "$BACKUP_FILE"; then
    if [[ -s "$BACKUP_FILE" ]]; then
        log_message "Backup successful: $BACKUP_FILE ($(du -h "$BACKUP_FILE" | cut -f1))"
        find "$BACKUP_DIR" -name "mysql_full_backup_*.sql" -mtime +7 -delete
        log_message "Cleaned up old backups (kept last 7 days)"
        exit 0
    fi
    log_message "ERROR: Backup file is empty or was not created"
    rm -f "$BACKUP_FILE"
    exit 1
fi
log_message "ERROR: mysqldump command failed"
rm -f "$BACKUP_FILE"
exit 1
EOF_BACKUP_MYSQL
fi
chmod +x /root/src/share/ubuntu/backup_pg.sh /root/src/share/ubuntu/backup_mysql.sh

wake_zfs() {
    if [ ! -d /zfs ]; then
        log "skip /zfs wake: /zfs not found"
        return 1
    fi
    log "wake /zfs before database restore"
    ls -ald /zfs >/dev/null 2>&1 || true
    find /zfs/backup -maxdepth 2 -type d -print -quit >/dev/null 2>&1 || true
    return 0
}

zfs_pool_for_mount() {
    src="$(findmnt -T /zfs -no SOURCE 2>/dev/null | head -1 || true)"
    if [ -n "$src" ] && printf '%s' "$src" | grep -qv '^/dev/'; then
        printf '%s\n' "$src" | cut -d/ -f1
        return 0
    fi
    src="$(df -P /zfs 2>/dev/null | awk 'NR==2 {print $1}' || true)"
    if [ -n "$src" ] && printf '%s' "$src" | grep -qv '^/dev/'; then
        printf '%s\n' "$src" | cut -d/ -f1
    fi
}

normalize_block_device() {
    dev="$1"
    [ -b "$dev" ] || return 0
    parent="$(lsblk -no PKNAME "$dev" 2>/dev/null | head -1 || true)"
    if [ -n "$parent" ]; then
        printf '/dev/%s\n' "$parent"
        return 0
    fi
    readlink -f "$dev" 2>/dev/null || printf '%s\n' "$dev"
}

zfs_block_devices() {
    pool="$(zfs_pool_for_mount || true)"
    if [ -n "$pool" ] && command -v zpool >/dev/null 2>&1; then
        zpool status -P "$pool" 2>/dev/null | awk '/\/dev\// {print $1}' | while IFS= read -r dev; do normalize_block_device "$dev"; done | sort -u
        return 0
    fi
    findmnt -T /zfs -no SOURCE 2>/dev/null | awk '/^\/dev\// {print $1}' | while IFS= read -r dev; do normalize_block_device "$dev"; done | sort -u
}

standby_zfs() {
    if [ ! -d /zfs ]; then
        return 0
    fi
    if ! command -v hdparm >/dev/null 2>&1; then
        log "skip /zfs standby: hdparm not installed"
        return 0
    fi
    devices="$(zfs_block_devices | sort -u || true)"
    if [ -z "$devices" ]; then
        log "skip /zfs standby: no block devices found"
        return 0
    fi
    log "set /zfs devices to standby"
    printf '%s\n' "$devices" | while IFS= read -r dev; do
        [ -n "$dev" ] || continue
        if [ -b "$dev" ]; then
            hdparm -y "$dev" >/dev/null 2>&1 && log "standby $dev" || log "WARN: standby $dev failed"
        fi
    done
}

newest_dump() {
    kind="$1"
    shift
    [ "$#" -gt 0 ] || return 0
    tmp="$(mktemp)"
    for root in "$@"; do
        [ -d "$root" ] || continue
        maxdepth_args=()
        case "$root" in
            /zfs/backup|/zfs/backup/*) maxdepth_args=(-maxdepth 4) ;;
        esac
        case "$kind" in
            mysql)
                find "$root" "${maxdepth_args[@]}" -type f \( \
                    -name 'mysql_full_backup_*.sql' -o \
                    -name 'mysql_full_backup_*.sql.gz' -o \
                    -name 'mysql-all.sql' -o \
                    -name 'mysql-all.sql.gz' -o \
                    -name '*mysql*.sql' -o \
                    -name '*mysql*.sql.gz' \
                \) -print0 2>/dev/null >> "$tmp"
                ;;
            postgres)
                find "$root" "${maxdepth_args[@]}" -type f \( \
                    -name 'pg_full_backup_*.sql' -o \
                    -name 'pg_full_backup_*.sql.gz' -o \
                    -name 'postgres-all.sql' -o \
                    -name 'postgres-all.sql.gz' -o \
                    -name '*postgres*.sql' -o \
                    -name '*postgres*.sql.gz' -o \
                    -name '*pg*.sql' -o \
                    -name '*pg*.sql.gz' \
                \) -print0 2>/dev/null >> "$tmp"
                ;;
            *)
                rm -f "$tmp"
                return 1
                ;;
        esac
    done
    score_dumps < "$tmp" | sort -nr | head -1 | cut -f2-
    rm -f "$tmp"
}

score_dumps() {
    while IFS= read -r -d '' path; do
        base="$(basename "$path")"
        stamp="$(printf '%s\n' "$base" | sed -n 's/.*_\([0-9]\{8\}_[0-9]\{6\}\)\.sql\(\.gz\)\?$/\1/p')"
        score=""
        if [ -n "$stamp" ]; then
            score="$(date -d "${stamp:0:4}-${stamp:4:2}-${stamp:6:2} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s 2>/dev/null || true)"
        fi
        [ -n "$score" ] || score="$(stat -c %Y "$path" 2>/dev/null || echo 0)"
        printf '%s\t%s\n' "$score" "$path"
    done
}

dump_search_roots() {
    for dir in \
        /zfs/backup \
        /root/auto_sync_db_dumps \
        /tmp/auto_sync_db_dumps \
        /root/src/share/ubuntu \
        /root/src/share/nas \
        /root/src/share
    do
        [ -d "$dir" ] && printf '%s\n' "$dir"
    done
}

ensure_postgresql_ready() {
    if command -v pg_lsclusters >/dev/null 2>&1; then
        pg_lsclusters -h 2>/dev/null | awk '$4 ~ /binaries_missing/ { print $1 }' | while read -r old_version; do
            [ -n "$old_version" ] || continue
            [ -d "/etc/postgresql/$old_version" ] && mv "/etc/postgresql/$old_version" "/etc/postgresql/${old_version}.disabled-by-auto-sync-$(date +%Y%m%d%H%M%S)" 2>/dev/null || true
        done
    fi
    version="$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -V | tail -1)"
    [ -z "$version" ] || fix_postgresql_config_permissions "$version"
    systemctl restart postgresql 2>/dev/null || true
    if command -v pg_lsclusters >/dev/null 2>&1; then
        if ! pg_lsclusters 2>/dev/null | awk '$4 == "online" { found = 1 } END { exit found ? 0 : 1 }'; then
            if [ -n "$version" ]; then
                [ -d "/etc/postgresql/$version/main" ] || pg_createcluster "$version" main --start || true
                fix_postgresql_config_permissions "$version"
                pg_ctlcluster "$version" main start 2>/dev/null || true
            fi
        fi
        if ! pg_lsclusters 2>/dev/null | awk '$3 == "5432" && $4 == "online" { found = 1 } END { exit found ? 0 : 1 }'; then
            if [ -n "$version" ] && [ -f "/etc/postgresql/$version/main/postgresql.conf" ]; then
                pg_ctlcluster "$version" main stop 2>/dev/null || true
                sed -i -E "s/^#?[[:space:]]*port[[:space:]]*=.*/port = 5432/" "/etc/postgresql/$version/main/postgresql.conf"
                fix_postgresql_config_permissions "$version"
                pg_ctlcluster "$version" main start 2>/dev/null || true
            fi
        fi
    fi
    [ -z "$version" ] || fix_postgresql_config_permissions "$version"
    systemctl restart postgresql 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
        pg_isready -q 2>/dev/null && return 0
        sleep 1
    done
    return 1
}

configure_postgresql_peer_maps() {
    for hba in /etc/postgresql/[0-9]*/main/pg_hba.conf; do
        [ -f "$hba" ] || continue
        dir="$(dirname "$hba")"
        ident="$dir/pg_ident.conf"
        sed -i -E 's/^(local[[:space:]]+all[[:space:]]+all[[:space:]]+peer)([[:space:]].*)?$/\1 map=gitlab/' "$hba"
        touch "$ident"
        grep -Eq '^[[:space:]]*gitlab[[:space:]]+git[[:space:]]+gitlab([[:space:]]|$)' "$ident" || printf '\ngitlab  git  gitlab\n' >> "$ident"
        grep -Eq '^[[:space:]]*gitlab[[:space:]]+tiger[[:space:]]+immich([[:space:]]|$)' "$ident" || printf '\ngitlab  tiger  immich\n' >> "$ident"
    done
    systemctl reload postgresql 2>/dev/null || true
}

prepare_immich_database_extensions() {
    ensure_postgresql_ready >/dev/null 2>&1 || { log "WARN: PostgreSQL is not ready for immich extension check"; return 0; }
    if ! sudo -u postgres psql -Atqc "SELECT 1 FROM pg_database WHERE datname = 'immich'" 2>/dev/null | grep -qx 1; then
        return 0
    fi
    sudo -u postgres psql -d immich -v ON_ERROR_STOP=0 \
        -c "CREATE EXTENSION IF NOT EXISTS vector;" \
        -c "ALTER EXTENSION vector UPDATE;" >/dev/null 2>&1 ||
        log "WARN: immich pgvector extension preparation failed"
}

repair_immich_media_derivatives() {
    ensure_postgresql_ready >/dev/null 2>&1 || { log "WARN: PostgreSQL is not ready for immich media derivative repair"; return 0; }
    if ! sudo -u postgres psql -Atqc "SELECT 1 FROM pg_database WHERE datname = 'immich'" 2>/dev/null | grep -qx 1; then
        return 0
    fi
    command -v ffmpeg >/dev/null 2>&1 || { log "WARN: ffmpeg is missing; skip immich media derivative repair"; return 0; }
    command -v ffprobe >/dev/null 2>&1 || { log "WARN: ffprobe is missing; skip immich media derivative repair"; return 0; }
    command -v python3 >/dev/null 2>&1 || { log "WARN: python3 is missing; skip immich media derivative repair"; return 0; }
    log "repair immich media derivatives"
    IMMICH_DERIVATIVE_REPAIR_LIMIT="${IMMICH_DERIVATIVE_REPAIR_LIMIT:-200}" python3 - <<'PY_IMMICH_DERIVATIVE_REPAIR' || log "WARN: immich media derivative repair failed"
import json
import os
import re
import shutil
import struct
import subprocess
import tempfile
from pathlib import Path

UPLOAD_ROOT = Path("/opt/immich/upload")
LIMIT = int(os.environ.get("IMMICH_DERIVATIVE_REPAIR_LIMIT", "200") or "200")
REPAIR_ALL = os.environ.get("IMMICH_DERIVATIVE_REPAIR_ALL", "").lower() in ("1", "true", "yes", "all")
SHARD_INDEX = int(os.environ.get("IMMICH_DERIVATIVE_REPAIR_SHARD_INDEX", "0") or "0")
SHARD_COUNT = int(os.environ.get("IMMICH_DERIVATIVE_REPAIR_SHARD_COUNT", "1") or "1")
ASSET_IDS = [value.strip() for value in os.environ.get("IMMICH_DERIVATIVE_REPAIR_ASSET_IDS", "").split(",") if value.strip()]
RESUME_ENABLED = (
    REPAIR_ALL
    and not ASSET_IDS
    and os.environ.get("IMMICH_DERIVATIVE_REPAIR_RESUME", "1").lower() not in ("0", "false", "no", "off")
)
STATE_DIR = Path(os.environ.get("IMMICH_DERIVATIVE_REPAIR_STATE_DIR", "/var/lib/immich_derivative_repair"))
CHECKPOINT_PATH = STATE_DIR / f"repair_all_shard_{SHARD_INDEX}_of_{SHARD_COUNT}.ok"
STRATEGY_CACHE_PATH = STATE_DIR / "strategy-cache.json"


def run(cmd, *, check=True, timeout=None):
    try:
        return subprocess.run(cmd, check=check, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        return subprocess.CompletedProcess(cmd, 124, exc.stdout or "", exc.stderr or "timeout")


def load_checkpoint():
    if not RESUME_ENABLED or not CHECKPOINT_PATH.exists():
        return set()
    result = set()
    for line in CHECKPOINT_PATH.read_text(errors="ignore").splitlines():
        asset_id = line.split("\t", 1)[0].strip()
        if asset_id:
            result.add(asset_id)
    return result


def mark_checkpoint(asset_id, status):
    if not RESUME_ENABLED:
        return
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    with CHECKPOINT_PATH.open("a", encoding="utf-8") as handle:
        handle.write(f"{asset_id}\t{status}\n")


def load_strategy_cache():
    if not STRATEGY_CACHE_PATH.exists():
        return {}
    try:
        return json.loads(STRATEGY_CACHE_PATH.read_text(errors="ignore") or "{}")
    except Exception:
        return {}


STRATEGY_CACHE = load_strategy_cache()


def save_strategy_cache():
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    tmp = STRATEGY_CACHE_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(STRATEGY_CACHE, sort_keys=True), encoding="utf-8")
    tmp.replace(STRATEGY_CACHE_PATH)


def file_fingerprint(path):
    stat = Path(path).stat()
    return {
        "path": str(path),
        "size": int(stat.st_size),
        "mtime_ns": int(stat.st_mtime_ns),
    }


def cached_strategy(asset_id, fingerprint):
    entry = STRATEGY_CACHE.get(asset_id)
    if not entry or entry.get("fingerprint") != fingerprint:
        return None
    return entry


def remember_strategy(asset_id, fingerprint, status, strategy=None):
    STRATEGY_CACHE[asset_id] = {
        "fingerprint": fingerprint,
        "status": status,
        "strategy": strategy or "",
    }
    save_strategy_cache()


def psql(sql):
    return run(["sudo", "-u", "postgres", "psql", "-d", "immich", "-At", "-F", "\t", "-c", sql]).stdout


def q(value):
    return "'" + str(value).replace("'", "''") + "'"


def first_color_stream(path):
    result = probe_json(path)
    if result is None:
        return None
    streams = result.get("streams", [])
    first_video = None
    for stream in streams:
        if stream.get("codec_type") != "video":
            continue
        idx = stream.get("index")
        if first_video is None:
            first_video = idx
        if not str(stream.get("pix_fmt", "")).startswith("gray"):
            return idx
    return first_video


def probe_json(path):
    result = run(["ffprobe", "-v", "quiet", "-print_format", "json", "-show_streams", str(path)], check=False)
    if result.returncode != 0:
        return None
    return json.loads(result.stdout or "{}")


def tile_grid_info(path):
    result = run(["ffmpeg", "-hide_banner", "-i", str(path)], check=False)
    lines = ((result.stdout or "") + (result.stderr or "")).splitlines()
    line = next((entry for entry in lines if "Tile Grid:" in entry and "(default)" in entry), None)
    if line is None:
        line = next((entry for entry in lines if "Tile Grid:" in entry), None)
    if line is None:
        return None
    dim_match = re.search(r"(\d+)x(\d+)(?: \(default\))?$", line.strip())
    pix_match = re.search(r"\),\s*([^,\s(]+)\(", line)
    if not dim_match or not pix_match:
        return None
    width, height = int(dim_match.group(1)), int(dim_match.group(2))
    pix_fmt = pix_match.group(1)
    info = probe_json(path)
    if info is None:
        return None
    color_streams = [
        stream for stream in info.get("streams", [])
        if stream.get("codec_type") == "video" and stream.get("pix_fmt") == pix_fmt and not str(stream.get("pix_fmt", "")).startswith("gray")
    ]
    if not color_streams:
        return None
    groups = {}
    for stream in color_streams:
        key = (int(stream.get("width") or 0), int(stream.get("height") or 0))
        groups.setdefault(key, []).append(int(stream["index"]))
    candidates = []
    for (tile_w, tile_h), indexes in groups.items():
        if tile_w <= 0 or tile_h <= 0:
            continue
        cols = (width + tile_w - 1) // tile_w
        rows = (height + tile_h - 1) // tile_h
        needed = cols * rows
        if len(indexes) >= needed:
            candidates.append((tile_w * tile_h, tile_w, tile_h, cols, rows, sorted(indexes)[:needed]))
    if not candidates:
        return None
    _, tile_w, tile_h, cols, rows, tiles = max(candidates)
    return width, height, tile_w, tile_h, cols, rows, tiles


def scale_filter(size):
    return f"scale='if(gt(iw,ih),-2,{size})':'if(gt(iw,ih),{size},-2)'"


def video_filter(size):
    return (
        "zscale=t=linear:npl=100,format=gbrpf32le,"
        f"zscale=p=bt709,tonemap=hable,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,{scale_filter(size)}"
    )


def output_quality_args(kind):
    if kind == "thumbnail":
        return ["-quality", "95"]
    return ["-q:v", "1"]


def make_image(src, dst, size, stream, kind):
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(src)]
    if stream is not None:
        cmd += ["-map", f"0:{stream}"]
    cmd += ["-frames:v", "1", "-vf", scale_filter(size), *output_quality_args(kind), str(dst)]
    return run(cmd, check=False).returncode == 0 and dst.exists() and dst.stat().st_size > 0


def make_image_from_source(src, dst, size, kind):
    if make_image_fast_source(src, dst, size, kind):
        return True
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(src), "-frames:v", "1", "-vf", scale_filter(size), *output_quality_args(kind), str(dst)]
    return run(cmd, check=False).returncode == 0 and dst.exists() and dst.stat().st_size > 0


def make_image_fast_source(src, dst, size, kind):
    tool = shutil.which("magick") or shutil.which("convert")
    if not tool:
        return False
    resize = f"{size}x{size}>"
    quality = "95" if kind == "thumbnail" else "92"
    cmd = [tool, str(src), "-auto-orient", "-resize", resize, "-quality", quality, str(dst)]
    return run(cmd, check=False, timeout=120).returncode == 0 and dst.exists() and dst.stat().st_size > 0


def make_normal_image_source(src, work_dir):
    dst = Path(work_dir) / "normal.jpg"
    if shutil.which("convert"):
        cmd = ["convert", str(src), "-auto-orient", str(dst)]
        if run(cmd, check=False).returncode == 0 and dst.exists() and dst.stat().st_size > 0:
            return dst
    if shutil.which("heif-convert"):
        cmd = ["heif-convert", str(src), str(dst)]
        if run(cmd, check=False).returncode == 0 and dst.exists() and dst.stat().st_size > 0:
            return dst
    return None


def make_heif_box_grid_source(src, work_dir):
    data = Path(src).read_bytes()

    def u8(offset):
        return data[offset]

    def u16(offset):
        return struct.unpack_from(">H", data, offset)[0]

    def u32(offset):
        return struct.unpack_from(">I", data, offset)[0]

    def u64(offset):
        return struct.unpack_from(">Q", data, offset)[0]

    def read_int(offset, size):
        return 0 if size == 0 else int.from_bytes(data[offset:offset + size], "big")

    def children(start, end):
        offset = start
        while offset + 8 <= end:
            size = u32(offset)
            box_type = data[offset + 4:offset + 8].decode("latin1")
            header = 8
            if size == 1:
                size = u64(offset + 8)
                header = 16
            elif size == 0:
                size = end - offset
            if size < header or offset + size > end:
                break
            yield box_type, offset, header, offset + size
            offset += size

    meta = None
    for box_type, offset, header, end in children(0, len(data)):
        if box_type == "meta":
            meta = {t: (o, h, e) for t, o, h, e in children(offset + header + 4, end)}
            break
    if not meta or not all(key in meta for key in ("pitm", "iinf", "iloc", "iref", "iprp", "idat")):
        return None

    def parse_pitm():
        offset, header, _ = meta["pitm"]
        pos = offset + header
        version = u8(pos)
        pos += 4
        return u16(pos) if version == 0 else u32(pos)

    def parse_iinf():
        items = {}
        offset, header, end = meta["iinf"]
        pos = offset + header
        version = u8(pos)
        pos += 4
        count = u16(pos) if version == 0 else u32(pos)
        pos += 2 if version == 0 else 4
        for _ in range(count):
            size = u32(pos)
            box_type = data[pos + 4:pos + 8].decode("latin1")
            box_end = pos + size
            p = pos + 8
            item_version = u8(p)
            p += 4
            if box_type == "infe" and item_version >= 2:
                item_id = u16(p) if item_version < 3 else u32(p)
                p += 2 if item_version < 3 else 4
                p += 2
                items[item_id] = data[p:p + 4].decode("latin1")
            pos = box_end
        return items

    def parse_iloc():
        locs = {}
        offset, header, _ = meta["iloc"]
        pos = offset + header
        version = u8(pos)
        pos += 4
        sizes = u8(pos)
        pos += 1
        offset_size, length_size = sizes >> 4, sizes & 15
        sizes = u8(pos)
        pos += 1
        base_size = sizes >> 4
        index_size = (sizes & 15) if version in (1, 2) else 0
        count = u16(pos) if version < 2 else u32(pos)
        pos += 2 if version < 2 else 4
        for _ in range(count):
            item_id = u16(pos) if version < 2 else u32(pos)
            pos += 2 if version < 2 else 4
            construction = 0
            if version in (1, 2):
                construction = u16(pos) & 15
                pos += 2
            pos += 2
            base = read_int(pos, base_size)
            pos += base_size
            extent_count = u16(pos)
            pos += 2
            extents = []
            for _ in range(extent_count):
                if version in (1, 2) and index_size:
                    pos += index_size
                extent_offset = read_int(pos, offset_size)
                pos += offset_size
                extent_length = read_int(pos, length_size)
                pos += length_size
                extents.append((base + extent_offset, extent_length))
            locs[item_id] = (construction, extents)
        return locs

    def parse_iref():
        refs = {}
        offset, header, end = meta["iref"]
        pos = offset + header
        version = u8(pos)
        pos += 4
        while pos + 8 <= end:
            size = u32(pos)
            ref_type = data[pos + 4:pos + 8].decode("latin1")
            p = pos + 8
            box_end = pos + size
            from_id = u16(p) if version == 0 else u32(p)
            p += 2 if version == 0 else 4
            count = u16(p)
            p += 2
            targets = []
            for _ in range(count):
                targets.append(u16(p) if version == 0 else u32(p))
                p += 2 if version == 0 else 4
            refs.setdefault(ref_type, {})[from_id] = targets
            pos = box_end
        return refs

    def parse_iprp():
        offset, header, end = meta["iprp"]
        ipco = ipma = None
        for box_type, child_offset, child_header, child_end in children(offset + header, end):
            if box_type == "ipco":
                ipco = (child_offset, child_header, child_end)
            elif box_type == "ipma":
                ipma = (child_offset, child_header, child_end)
        if not ipco or not ipma:
            return [], {}
        props = [(t, data[o + h:e]) for t, o, h, e in children(ipco[0] + ipco[1], ipco[2])]
        assoc = {}
        offset, header, _ = ipma
        pos = offset + header
        version = u8(pos)
        flags = int.from_bytes(data[pos + 1:pos + 4], "big")
        pos += 4
        count = u32(pos)
        pos += 4
        for _ in range(count):
            item_id = u16(pos) if version < 1 else u32(pos)
            pos += 2 if version < 1 else 4
            assoc_count = u8(pos)
            pos += 1
            indexes = []
            for _ in range(assoc_count):
                if flags & 1:
                    value = u16(pos)
                    pos += 2
                    indexes.append(value & 0x7fff)
                else:
                    value = u8(pos)
                    pos += 1
                    indexes.append(value & 0x7f)
            assoc[item_id] = indexes
        return props, assoc

    def prop(props, assoc, item_id, prop_type):
        for index in assoc.get(item_id, []):
            if 1 <= index <= len(props) and props[index - 1][0] == prop_type:
                return props[index - 1][1]
        return None

    def ispe_size(payload):
        return struct.unpack_from(">II", payload, 4)

    def hvcc_to_annexb(hvcc):
        output = bytearray()
        pos = 23
        for _ in range(hvcc[22]):
            pos += 1
            count = struct.unpack_from(">H", hvcc, pos)[0]
            pos += 2
            for _ in range(count):
                length = struct.unpack_from(">H", hvcc, pos)[0]
                pos += 2
                output += b"\x00\x00\x00\x01" + hvcc[pos:pos + length]
                pos += length
        return bytes(output)

    def sample_to_annexb(sample, nal_length_size):
        output = bytearray()
        pos = 0
        while pos + nal_length_size <= len(sample):
            length = int.from_bytes(sample[pos:pos + nal_length_size], "big")
            pos += nal_length_size
            if length <= 0 or pos + length > len(sample):
                return b"\x00\x00\x00\x01" + sample
            output += b"\x00\x00\x00\x01" + sample[pos:pos + length]
            pos += length
        return bytes(output)

    try:
        primary = parse_pitm()
        items = parse_iinf()
        if items.get(primary) != "grid":
            return None
        locs = parse_iloc()
        refs = parse_iref()
        props, assoc = parse_iprp()
        tile_ids = refs.get("dimg", {}).get(primary)
        if not tile_ids:
            return None
        idat_offset = meta["idat"][0] + meta["idat"][1]
        construction, extents = locs[primary]
        if construction != 1:
            return None
        grid_payload = b"".join(data[idat_offset + offset:idat_offset + offset + length] for offset, length in extents)
        rows = grid_payload[2] + 1
        cols = grid_payload[3] + 1
        if len(grid_payload) >= 8:
            width = int.from_bytes(grid_payload[4:6], "big")
            height = int.from_bytes(grid_payload[6:8], "big")
        else:
            width, height = ispe_size(prop(props, assoc, primary, "ispe"))
        tile_ids = tile_ids[:rows * cols]
        if len(tile_ids) != rows * cols:
            return None
        irot = prop(props, assoc, primary, "irot")
        rotation = (irot[0] & 3) if irot else 0
    except Exception:
        return None

    tile_paths = []
    for position, item_id in enumerate(tile_ids):
        try:
            hvcc = prop(props, assoc, item_id, "hvcC")
            nal_length_size = (hvcc[21] & 3) + 1
            _, item_extents = locs[item_id]
            sample = b"".join(data[offset:offset + length] for offset, length in item_extents)
        except Exception:
            return None
        hevc_path = Path(work_dir) / f"box_tile_{position:03d}.hevc"
        tile_path = Path(work_dir) / f"box_tile_{position:03d}.jpg"
        hevc_path.write_bytes(hvcc_to_annexb(hvcc) + sample_to_annexb(sample, nal_length_size))
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "hevc", "-i", str(hevc_path), "-frames:v", "1", "-q:v", "1", str(tile_path)]
        if run(cmd, check=False, timeout=20).returncode != 0 or not tile_path.exists() or tile_path.stat().st_size == 0:
            return None
        tile_paths.append(tile_path)

    first = run(["identify", "-format", "%w %h", str(tile_paths[0])], check=False)
    if first.returncode != 0:
        return None
    tile_w, tile_h = (int(value) for value in first.stdout.split())
    inputs = []
    for tile_path in tile_paths:
        inputs.extend(["-i", str(tile_path)])
    layout = "|".join(f"{(idx % cols) * tile_w}_{(idx // cols) * tile_h}" for idx in range(len(tile_paths)))
    filters = [f"xstack=inputs={len(tile_paths)}:layout={layout}", f"crop={width}:{height}:0:0"]
    if rotation == 1:
        filters.append("transpose=2")
    elif rotation == 2:
        filters.append("transpose=1,transpose=1")
    elif rotation == 3:
        filters.append("transpose=1")
    full_path = Path(work_dir) / "box_grid_full.jpg"
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        *inputs,
        "-filter_complex", ",".join(filters),
        "-frames:v", "1", "-q:v", "1", str(full_path),
    ]
    if run(cmd, check=False, timeout=180).returncode != 0 or not full_path.exists() or full_path.stat().st_size == 0:
        return None
    return full_path


def make_tile_grid_source(src, work_dir, orientation):
    info = tile_grid_info(src)
    if info is None:
        return None
    width, height, tile_w, tile_h, cols, rows, tiles = info
    tile_paths = []
    for position, stream_index in enumerate(tiles):
        tile_path = Path(work_dir) / f"tile_{position:03d}.jpg"
        tile_paths.append(tile_path)
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(src)]
    for stream_index, tile_path in zip(tiles, tile_paths):
        cmd += ["-map", f"0:{stream_index}", "-frames:v", "1", "-q:v", "1", str(tile_path)]
    timeout = max(60, min(900, len(tile_paths) * 2))
    if run(cmd, check=False, timeout=timeout).returncode != 0:
        return None
    if any(not tile_path.exists() or tile_path.stat().st_size == 0 for tile_path in tile_paths):
        return None
    inputs = []
    for tile_path in tile_paths:
        inputs.extend(["-i", str(tile_path)])
    layout = "|".join(f"{(idx % cols) * tile_w}_{(idx // cols) * tile_h}" for idx in range(len(tile_paths)))
    full_path = Path(work_dir) / "tile_grid_full.jpg"
    filters = [f"xstack=inputs={len(tile_paths)}:layout={layout}", f"crop={width}:{height}:0:0"]
    if str(orientation) == "6":
        filters.append("transpose=1")
    elif str(orientation) == "8":
        filters.append("transpose=2")
    elif str(orientation) == "3":
        filters.append("transpose=1,transpose=1")
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        *inputs,
        "-filter_complex", ",".join(filters),
        "-frames:v", "1", "-q:v", "1", str(full_path),
    ]
    if run(cmd, check=False, timeout=120).returncode != 0 or not full_path.exists() or full_path.stat().st_size == 0:
        return None
    return full_path


def make_heif_source_by_strategy(src, work_dir, orientation, strategy):
    if strategy == "box-grid":
        return make_heif_box_grid_source(src, work_dir)
    if strategy == "normal":
        return make_normal_image_source(src, work_dir)
    if strategy == "primary-stream":
        stream = first_color_stream(src)
        if stream is None:
            return None
        dst = Path(work_dir) / "primary_stream.jpg"
        if make_image(src, dst, 4096, stream, "fullsize"):
            return dst
        return None
    if strategy == "tile-grid":
        return make_tile_grid_source(src, work_dir, orientation)
    return None


def make_outputs_from_source(source, outputs):
    made = [(kind, path) for kind, path, size in outputs if make_image_from_source(source, path, size, kind)]
    if len(made) == len(outputs):
        return made
    for _, path, _ in outputs:
        try:
            if path.exists():
                path.unlink()
        except Exception:
            pass
    return []


def repair_heif_outputs(asset_id, src, outputs, orientation, work_dir):
    fingerprint = file_fingerprint(src)
    cached = cached_strategy(asset_id, fingerprint)
    if cached and cached.get("status") in ("empty-original", "known-failed"):
        return [], cached["status"]
    cached_strategy_name = cached.get("strategy") if cached and cached.get("status") == "ok" else None
    strategies = []
    if cached_strategy_name:
        strategies.append(cached_strategy_name)
    # Cheap, targeted paths first. The expensive full tile-grid fallback is last
    # and is cached per original file once it proves necessary.
    strategies.extend(["box-grid", "normal", "primary-stream", "tile-grid"])
    seen = set()
    for strategy in strategies:
        if strategy in seen:
            continue
        seen.add(strategy)
        source = make_heif_source_by_strategy(src, work_dir, orientation, strategy)
        if source is None:
            continue
        made = make_outputs_from_source(source, outputs)
        if made:
            remember_strategy(asset_id, fingerprint, "ok", strategy)
            return made, None
    remember_strategy(asset_id, fingerprint, "known-failed")
    return [], None


def make_video(src, dst, size):
    for second in video_seek_times(src):
        base = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-ss", f"{second:.3f}", "-i", str(src), "-map", "0:v:0", "-frames:v", "1"]
        cmd = base + ["-vf", video_filter(size), "-q:v", "2", str(dst)]
        if run(cmd, check=False, timeout=20).returncode == 0 and dst.exists() and dst.stat().st_size > 0:
            return True
        cmd = base + ["-vf", scale_filter(size), "-q:v", "2", str(dst)]
        if run(cmd, check=False, timeout=20).returncode == 0 and dst.exists() and dst.stat().st_size > 0:
            return True
    return False


def image_mean(path):
    if not shutil.which("identify"):
        return 1.0
    result = run(["identify", "-format", "%[mean]", str(path)], check=False)
    if result.returncode != 0:
        return 0.0
    try:
        return float(result.stdout.strip())
    except ValueError:
        return 0.0


def video_duration(path):
    info = probe_json(path)
    if info is None:
        return None
    candidates = [info.get("format", {}).get("duration")]
    candidates.extend(stream.get("duration") for stream in info.get("streams", []) if stream.get("codec_type") == "video")
    for value in candidates:
        try:
            duration = float(value)
        except (TypeError, ValueError):
            continue
        if duration > 0:
            return duration
    return None


def has_video_stream(path):
    info = probe_json(path)
    if info is None:
        return False
    return any(stream.get("codec_type") == "video" for stream in info.get("streams", []))


def video_seek_times(path):
    duration = video_duration(path)
    if duration is None:
        return (0.0, 1.0, 3.0, 5.0, 8.0)
    candidates = [0.0, min(0.1, duration / 3), duration / 2, duration * 0.8, 1.0, 3.0, 5.0, 8.0]
    result = []
    for value in candidates:
        value = max(0.0, min(float(value), max(0.0, duration - 0.001)))
        rounded = round(value, 3)
        if rounded not in result:
            result.append(rounded)
    return tuple(result)


def representative_video_frame(src, work_dir):
    best = None
    best_mean = -1.0
    for index, second in enumerate(video_seek_times(src)):
        frame = Path(work_dir) / f"frame_{index}.jpg"
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-ss", f"{second:.3f}", "-i", str(src), "-map", "0:v:0", "-frames:v", "1", "-vf", "scale=1920:-2", "-q:v", "2", str(frame)]
        if run(cmd, check=False, timeout=20).returncode != 0 or not frame.exists() or frame.stat().st_size == 0:
            continue
        mean = image_mean(frame)
        if mean > best_mean:
            best = frame
            best_mean = mean
        if mean > 3000:
            break
    return best


def upsert(asset_id, kind, path):
    sql = (
        'INSERT INTO asset_file ("assetId", type, path, "createdAt", "updatedAt", "isEdited") VALUES '
        f"({q(asset_id)}, {q(kind)}, {q(str(path))}, now(), now(), false) "
        'ON CONFLICT ("assetId", type, "isEdited") DO UPDATE SET '
        'path = EXCLUDED.path, "updatedAt" = now()'
    )
    psql(sql)


def node_bin():
    candidates = [
        os.environ.get("NODE_BIN"),
        "/opt/src/software/tools/nvm/versions/node/v24.18.0/bin/node",
        shutil.which("node"),
    ]
    return next((str(candidate) for candidate in candidates if candidate and Path(str(candidate)).exists()), None)


def refresh_asset_cache_key(asset_id, image_path):
    node = node_bin()
    if not node or not Path("/opt/immich/server/node_modules/sharp").exists():
        psql(f'UPDATE asset SET "updatedAt" = now(), "updateId" = immich_uuid_v7() WHERE id = {q(asset_id)}')
        return
    script = r"""
const sharp = require('/opt/immich/server/node_modules/sharp');
const { rgbaToThumbHash } = require('/opt/immich/server/node_modules/thumbhash');
const input = process.argv[1];
sharp(input)
  .resize(100, 100, { fit: 'inside' })
  .ensureAlpha()
  .raw()
  .toBuffer({ resolveWithObject: true })
  .then(({ data, info }) => {
    process.stdout.write(Buffer.from(rgbaToThumbHash(info.width, info.height, data)).toString('base64'));
  })
  .catch((error) => {
    console.error(error && error.stack ? error.stack : error);
    process.exit(1);
  });
"""
    result = run([node, "-e", script, str(image_path)], check=False, timeout=30)
    thumbhash = (result.stdout or "").strip()
    if result.returncode != 0 or not thumbhash:
        psql(f'UPDATE asset SET "updatedAt" = now(), "updateId" = immich_uuid_v7() WHERE id = {q(asset_id)}')
        return
    psql(
        f'UPDATE asset SET thumbhash = decode({q(thumbhash)}, '
        f'\'base64\'), "updatedAt" = now(), "updateId" = immich_uuid_v7() WHERE id = {q(asset_id)}'
    )


def asset_dir(owner_id, asset_id):
    return UPLOAD_ROOT / "thumbs" / owner_id / asset_id[:2] / asset_id[2:4]


where_clause = """
    type IN ('IMAGE', 'VIDEO')
""" if REPAIR_ALL else (
    "id IN (" + ", ".join(q(asset_id) for asset_id in ASSET_IDS) + ")"
    if ASSET_IDS else """
    (
        type = 'VIDEO'
        AND (thumbnail IS NULL OR preview IS NULL OR coalesce(thumbnail_bytes, 0) < 1000)
      )
      OR (
        type = 'IMAGE'
        AND lower("originalPath") ~ '\\.(heic|heif)$'
        AND (
            thumbnail IS NULL OR preview IS NULL OR fullsize IS NULL
            OR coalesce(thumbnail_bytes, 0) < 1000
            OR coalesce(preview_bytes, 0) < 50000
            OR coalesce(fullsize_bytes, 0) < 50000
        )
      )
      OR "originalPath" IN (
        '/opt/immich/upload/library/admin/2026/2026-07-12/6390.heic',
        '/opt/immich/upload/library/admin/2026/2026-07-11/IMG_1229.mov',
        '/opt/immich/upload/library/admin/2026/2026-07-11/IMG_1228.mov',
        '/opt/immich/upload/library/admin/2026/2026-06-07/IMG_1101.heic'
      )
"""
)
limit_clause = "" if LIMIT <= 0 else f"LIMIT {LIMIT}"
shard_clause = ""
if SHARD_COUNT > 1 and not ASSET_IDS:
    shard_clause = f"AND mod(abs(hashtext(id::text)), {SHARD_COUNT}) = {SHARD_INDEX}"

query = f"""
WITH files AS (
    SELECT a.id, a.type, a."ownerId", a."originalPath", a."fileModifiedAt", a."createdAt",
           max(e.orientation) AS orientation,
           max(CASE WHEN af.type = 'thumbnail' THEN af.path END) AS thumbnail,
           max(CASE WHEN af.type = 'preview' THEN af.path END) AS preview,
           max(CASE WHEN af.type = 'fullsize' THEN af.path END) AS fullsize,
           max(CASE WHEN af.type = 'thumbnail' THEN (pg_stat_file(af.path, true)).size END) AS thumbnail_bytes,
           max(CASE WHEN af.type = 'preview' THEN (pg_stat_file(af.path, true)).size END) AS preview_bytes,
           max(CASE WHEN af.type = 'fullsize' THEN (pg_stat_file(af.path, true)).size END) AS fullsize_bytes
    FROM asset a
    LEFT JOIN asset_file af ON af."assetId" = a.id AND af."isEdited" = false
    LEFT JOIN asset_exif e ON e."assetId" = a.id
    WHERE a."deletedAt" IS NULL
      AND a."originalPath" IS NOT NULL
    GROUP BY a.id, a.type, a."ownerId", a."originalPath", a."fileModifiedAt", a."createdAt"
),
missing AS (
    SELECT id, type, "ownerId", "originalPath", orientation, "fileModifiedAt", "createdAt",
           thumbnail, preview, fullsize, thumbnail_bytes, preview_bytes, fullsize_bytes
    FROM files
    WHERE ({where_clause})
    {shard_clause}
    ORDER BY "fileModifiedAt" DESC NULLS LAST, "createdAt" DESC
    {limit_clause}
)
SELECT id, type, "ownerId", "originalPath", coalesce(orientation, ''),
       coalesce(thumbnail, ''), coalesce(preview, ''), coalesce(fullsize, ''),
       coalesce(thumbnail_bytes, 0), coalesce(preview_bytes, 0), coalesce(fullsize_bytes, 0)
FROM missing
"""

rows = [line.split("\t", 10) for line in psql(query).splitlines() if line.strip()]
completed_ids = load_checkpoint()
ok = 0
failed = 0
resumed = 0
for asset_id, asset_type, owner_id, original_path, orientation, thumbnail, preview, fullsize, thumbnail_bytes, preview_bytes, fullsize_bytes in rows:
    if asset_id in completed_ids:
        resumed += 1
        if (ok + failed + resumed) % 100 == 0:
            print(f"immich derivative repair progress: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {ok + failed + resumed}/{len(rows)} checked", flush=True)
        continue
    src = Path(original_path)
    if not src.exists():
        failed += 1
        print(f"missing original: {asset_id} {src}")
        continue
    if src.stat().st_size == 0:
        ok += 1
        mark_checkpoint(asset_id, "empty-original")
        print(f"skip empty original: {asset_id} {src}")
        if (ok + failed + resumed) % 100 == 0:
            print(f"immich derivative repair progress: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {ok + failed + resumed}/{len(rows)} checked", flush=True)
        continue
    out_dir = asset_dir(owner_id, asset_id)
    out_dir.mkdir(parents=True, exist_ok=True)
    try:
        if asset_type == "IMAGE":
            outputs = []
            if not thumbnail or int(thumbnail_bytes or 0) < 1000 or not Path(thumbnail).exists():
                outputs.append(("thumbnail", out_dir / f"{asset_id}_thumbnail.webp", 512))
            if not preview or int(preview_bytes or 0) < 50000 or not Path(preview).exists():
                outputs.append(("preview", out_dir / f"{asset_id}_preview.jpeg", 2048))
            if not fullsize or int(fullsize_bytes or 0) < 50000 or not Path(fullsize).exists():
                outputs.append(("fullsize", out_dir / f"{asset_id}_fullsize.jpeg", 4096))
            suffix = src.suffix.lower()
            if not outputs:
                made = []
            elif suffix in (".heic", ".heif"):
                with tempfile.TemporaryDirectory(prefix="immich-derivative-") as temp_dir:
                    made, skip_status = repair_heif_outputs(asset_id, src, outputs, orientation, temp_dir)
                    if skip_status:
                        ok += 1
                        mark_checkpoint(asset_id, skip_status)
                        print(f"skip cached HEIC {skip_status}: {asset_id} {src}")
                        if (ok + failed + resumed) % 100 == 0:
                            print(f"immich derivative repair progress: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {ok + failed + resumed}/{len(rows)} checked", flush=True)
                        continue
            else:
                made = [(kind, path) for kind, path, size in outputs if make_image_from_source(src, path, size, kind)]
        else:
            if not has_video_stream(src):
                ok += 1
                mark_checkpoint(asset_id, "no-video-stream")
                print(f"skip video without video stream: {asset_id} {src}")
                if (ok + failed + resumed) % 100 == 0:
                    print(f"immich derivative repair progress: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {ok + failed + resumed}/{len(rows)} checked", flush=True)
                continue
            outputs = [
                ("thumbnail", out_dir / f"{asset_id}_thumbnail.webp", 512),
                ("preview", out_dir / f"{asset_id}_preview.jpeg", 2048),
            ]
            with tempfile.TemporaryDirectory(prefix="immich-derivative-") as temp_dir:
                frame = representative_video_frame(src, temp_dir)
                if frame is None:
                    ok += 1
                    mark_checkpoint(asset_id, "no-decodable-video-frame")
                    print(f"skip video without decodable frame: {asset_id} {src}")
                    if (ok + failed + resumed) % 100 == 0:
                        print(f"immich derivative repair progress: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {ok + failed + resumed}/{len(rows)} checked", flush=True)
                    continue
                made = [(kind, path) for kind, path, size in outputs if make_image_from_source(frame, path, size, kind)]
        if not made:
            if asset_type == "IMAGE" and not outputs:
                ok += 1
                mark_checkpoint(asset_id, "already-good")
                continue
            failed += 1
            print(f"ffmpeg failed: {asset_id} {src}")
            continue
        for kind, path in made:
            shutil.chown(path, user="tiger", group="tiger")
            path.chmod(0o644)
            upsert(asset_id, kind, path)
        cache_source = next((path for kind, path in made if kind == "preview"), made[0][1])
        refresh_asset_cache_key(asset_id, cache_source)
        ok += 1
        mark_checkpoint(asset_id, "ok")
        if (ok + failed + resumed) % 100 == 0:
            print(f"immich derivative repair progress: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {ok + failed + resumed}/{len(rows)} checked", flush=True)
    except Exception as exc:
        failed += 1
        print(f"repair failed: {asset_id} {src}: {exc}")

if RESUME_ENABLED and failed == 0 and CHECKPOINT_PATH.exists():
    CHECKPOINT_PATH.unlink()
print(f"immich derivative repair summary: shard {SHARD_INDEX}/{SHARD_COUNT}, {ok} ok, {failed} failed, {resumed} resumed, {len(rows)} checked")
PY_IMMICH_DERIVATIVE_REPAIR
}

mysql_has_user_data() {
    command -v mysql >/dev/null 2>&1 || return 1
    count="$(mysql -NBe "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema NOT IN ('mysql','information_schema','performance_schema','sys')" 2>/dev/null || echo 0)"
    [ "${count:-0}" -gt 0 ] 2>/dev/null
}

postgres_has_user_data() {
    ensure_postgresql_ready >/dev/null 2>&1 || return 1
    count="$(sudo -u postgres psql -Atqc "SELECT COUNT(*) FROM pg_database WHERE datistemplate = false AND datname NOT IN ('postgres')" 2>/dev/null || echo 0)"
    [ "${count:-0}" -gt 0 ] 2>/dev/null
}

restore_once() {
    kind="$1"
    latest="$2"
    marker="/var/lib/auto_sync/${kind}_restore_marker"
    [ -n "$latest" ] || { log "skip $kind restore: no dump found"; return 0; }
    mkdir -p /var/lib/auto_sync
    if [ -f "$marker" ] && [ "$(cat "$marker" 2>/dev/null)" = "$latest" ]; then
        log "skip $kind restore: already restored $latest"
        return 0
    fi
    case "$kind" in
        mysql)
            if mysql_has_user_data; then
                log "skip mysql restore: existing user data found"
                return 0
            fi
            ;;
        postgres)
            if postgres_has_user_data; then
                log "skip postgres restore: existing user databases found"
                return 0
            fi
            ;;
    esac
    log "restore $kind from $latest"
    case "$kind" in
        mysql)
            systemctl restart mysql 2>/dev/null || systemctl restart mariadb 2>/dev/null || true
            if printf '%s' "$latest" | grep -q '\.gz$'; then zcat "$latest" | mysql; else mysql < "$latest"; fi
            ;;
        postgres)
            ensure_postgresql_ready || log "WARN: postgresql is not ready before restore"
            configure_postgresql_peer_maps
            if printf '%s' "$latest" | grep -q '\.gz$'; then zcat "$latest" | sudo -u postgres psql -v ON_ERROR_STOP=0; else sudo -u postgres psql -v ON_ERROR_STOP=0 -f "$latest"; fi
            ;;
    esac && printf '%s' "$latest" > "$marker" || log "WARN: $kind restore failed"
}
zfs_woken=0
wake_zfs && zfs_woken=1 || true
dump_roots="$(dump_search_roots)"
mysql_dump="$(newest_dump mysql $dump_roots 2>/dev/null || true)"
pg_dump="$(newest_dump postgres $dump_roots 2>/dev/null || true)"
[ -n "$mysql_dump" ] && log "selected mysql dump: $mysql_dump"
[ -n "$pg_dump" ] && log "selected postgres dump: $pg_dump"
restore_once mysql "$mysql_dump"
restore_once postgres "$pg_dump"
prepare_immich_database_extensions
id tiger >/dev/null 2>&1 || useradd -m -s /bin/bash tiger
mkdir -p /opt/immich/upload
for d in backups encoded-video library profile thumbs upload; do
    mkdir -p "/opt/immich/upload/$d"
    touch "/opt/immich/upload/$d/.immich"
done
chown -R tiger:tiger /opt/immich/upload 2>/dev/null || true
deploy_immich_from_git || log "WARN: immich deploy from git failed"
repair_immich_media_derivatives
rm -rf /root/auto_sync_db_dumps /tmp/auto_sync_db_dumps 2>/dev/null || true
if [ "$zfs_woken" = "1" ]; then
    standby_zfs
fi

migrate_opt_usr_local_layout() {
    mkdir -p /opt/usr/local
    chmod 0755 /opt/usr /opt/usr/local 2>/dev/null || true
    if grep -Eq '^/opt/usr/local[[:space:]]+/usr/local[[:space:]]' /etc/fstab 2>/dev/null; then
        cp -a /etc/fstab "/etc/fstab.auto_sync_backup.$(date +%Y%m%d%H%M%S)"
        sed -i '\#^/opt/usr/local[[:space:]]\+/usr/local[[:space:]]#d' /etc/fstab
        systemctl daemon-reload 2>/dev/null || true
        umount -l /usr/local 2>/dev/null || true
        mkdir -p /usr/local
        chmod 0755 /usr/local 2>/dev/null || true
        systemctl reset-failed usr-local.mount 2>/dev/null || true
    fi
    if [ -d /opt/auto_sync ]; then
        mkdir -p /opt/usr/local/auto_sync
        cp -a /opt/auto_sync/. /opt/usr/local/auto_sync/
        rm -rf /opt/auto_sync
    fi
    for name in blog go halo shadowsocks tbox waiwei xray bin; do
        src="/usr/local/$name"
        dst="/opt/usr/local/$name"
        [ -e "$src" ] || [ -L "$src" ] || continue
        if [ -e "$dst" ] && [ "$src" -ef "$dst" ]; then
            continue
        fi
        mkdir -p "$dst"
        if [ -d "$src" ]; then
            cp -a "$src"/. "$dst"/
        elif [ -f "$src" ]; then
            cp -a "$src" "$dst"
        fi
        [ "$src" -ef "$dst" ] 2>/dev/null || rm -rf "$src"
    done
    for root in /etc/systemd/system /etc/profile.d /etc/logrotate.d /opt/usr/local /etc/immich; do
        [ -e "$root" ] || continue
        find "$root" -type f -exec grep -IlE '/opt/auto_sync|/usr/local/(blog|go|halo|shadowsocks|tbox|waiwei|xray|bin)' {} + 2>/dev/null |
            xargs -r sed -i \
                -e 's#/opt/auto_sync#/opt/usr/local/auto_sync#g' \
                -e 's#/usr/local/blog#/opt/usr/local/blog#g' \
                -e 's#/usr/local/go#/opt/usr/local/go#g' \
                -e 's#/usr/local/halo#/opt/usr/local/halo#g' \
                -e 's#/usr/local/shadowsocks#/opt/usr/local/shadowsocks#g' \
                -e 's#/usr/local/tbox#/opt/usr/local/tbox#g' \
                -e 's#/usr/local/waiwei#/opt/usr/local/waiwei#g' \
                -e 's#/usr/local/xray#/opt/usr/local/xray#g' \
                -e 's#/usr/local/bin#/opt/usr/local/bin#g'
        find "$root" -type f -exec grep -Il '/opt/opt' {} + 2>/dev/null |
            xargs -r sed -i \
                -e 's#/opt/opt/opt/usr/local#/opt/usr/local#g' \
                -e 's#/opt/opt/usr/local#/opt/usr/local#g'
    done
    if [ -f /opt/usr/local/blog/conf/rblog.toml ]; then
        sed -i 's#/usr/local/blog#/opt/usr/local/blog#g' /opt/usr/local/blog/conf/rblog.toml
    fi
    cat > /etc/profile.d/opt-usr-local-path.sh <<'EOF_OPT_USR_LOCAL_PATH'
# Managed by auto_sync NAS deployment.
case ":$PATH:" in
  *:/opt/usr/local/bin:*) ;;
  *) export PATH="/opt/usr/local/bin:$PATH" ;;
esac
case ":$PATH:" in
  *:/opt/usr/local/go/go1.25.1/bin:*) ;;
  *) [ ! -d /opt/usr/local/go/go1.25.1/bin ] || export PATH="/opt/usr/local/go/go1.25.1/bin:$PATH" ;;
esac
EOF_OPT_USR_LOCAL_PATH
    chmod 0644 /etc/profile.d/opt-usr-local-path.sh
}
migrate_opt_usr_local_layout

mkdir -p /opt/usr/local/auto_sync/logs /opt/usr/local/blog/logs /opt/usr/local/tbox/log /opt/usr/local/waiwei/logs /opt/usr/local/xray/logs /opt/usr/local/shadowsocks/logs /opt/usr/local/shadowsocks/conf /opt/usr/local/shadowsocks/data /opt/immich/server /opt/immich/upload /opt/immich/machine-learning /opt/immich/conf /opt/user/root/.halo2 /opt/user/tiger /home/tiger
for d in backups encoded-video library profile thumbs upload; do
    mkdir -p "/opt/immich/upload/$d"
    touch "/opt/immich/upload/$d/.immich"
done
migrate_root_home_to_opt() {
    mkdir -p /opt/user/root

    # Historical Halo state lived under tiger; merge it into root before the
    # generic /root spillover pass creates /root/.halo2.
    for src in /home/tiger/.halo2 /opt/user/tiger/.halo2; do
        [ -e "$src" ] || [ -L "$src" ] || continue
        mkdir -p /opt/user/root/.halo2
        if [ -d "$src" ]; then
            cp -aL "$src"/. /opt/user/root/.halo2/ 2>/dev/null || true
        fi
        rm -rf "$src"
    done

    shopt -s dotglob nullglob
    for src in /root/*; do
        name="${src##*/}"
        [ "$name" = ".ssh" ] && continue
        dest="/opt/user/root/$name"
        if [ -L "$src" ] && [ "$(readlink "$src")" = "$dest" ]; then
            continue
        fi
        if [ -d "$src" ]; then
            mkdir -p "$dest"
            cp -aL "$src"/. "$dest"/ 2>/dev/null || true
        elif [ -f "$src" ] || [ -L "$src" ]; then
            mkdir -p "$(dirname "$dest")"
            cp -aL "$src" "$dest" 2>/dev/null || true
        else
            continue
        fi
        rm -rf "$src"
        ln -s "$dest" "$src"
    done
    shopt -u dotglob nullglob

    chown -R root:root /opt/user/root 2>/dev/null || true
    find /root -mindepth 1 -maxdepth 1 ! -name .ssh -exec chown -h root:root {} + 2>/dev/null || true
    chmod 700 /root /root/.ssh 2>/dev/null || true
}
migrate_root_home_to_opt
python3 - <<'PY_TIGER_LINK_OWNERS'
import os
import pwd
import grp
from pathlib import Path

uid = pwd.getpwnam('tiger').pw_uid
gid = grp.getgrnam('tiger').gr_gid
for entry in Path('/home/tiger').iterdir():
    if entry.is_symlink():
        os.lchown(entry, uid, gid)
PY_TIGER_LINK_OWNERS
for d in /opt/immich /opt/user/tiger; do
    [ -e "$d" ] && chown -R tiger:tiger "$d" 2>/dev/null || true
done
for d in /opt/usr/local/auto_sync /opt/usr/local/halo /opt/usr/local/tbox /opt/usr/local/shadowsocks /opt/user/root/.halo2; do
    [ -e "$d" ] && chown -R root:root "$d" 2>/dev/null || true
done
for f in \
    /opt/usr/local/auto_sync/bin/auto_sync \
    /opt/usr/local/auto_sync/bin/auto_syncd \
    /opt/usr/local/auto_sync/bin/auto_sync_gui \
    /opt/usr/local/tbox/bin/tbox_client \
    /opt/usr/local/xray/bin/xray \
    /opt/usr/local/xray/bin/update-geo.sh \
    /opt/usr/local/waiwei/bin/waiwei_web \
    /opt/usr/local/waiwei/bin/waiwei_puller \
    /opt/usr/local/blog/bin/rblog \
    /opt/usr/local/blog/bin/rblog-backup \
    /opt/usr/local/blog/bin/admin/* \
    /opt/usr/local/halo/bin/* \
    /opt/usr/local/shadowsocks/bin/* \
    /opt/usr/local/bin/vlmcsd
do
    [ -e "$f" ] && chmod a+rx "$f" 2>/dev/null || true
done
find /opt/immich/server/bin /opt/immich/machine-learning/.venv/bin -type f -exec chmod a+rx {} + 2>/dev/null || true
chmod a+rx /opt/src/software/tools/nvm/versions/node/v24.18.0/bin/node 2>/dev/null || true
normalize_deploy_permissions
systemctl daemon-reload
systemctl reset-failed 2>/dev/null || true
rm -f /etc/nginx/sites-enabled/default /etc/nginx/conf.d/default.conf 2>/dev/null || true
for f in /etc/systemd/system/auto_sync.service /etc/systemd/system/halo2.service /etc/systemd/system/tbox_client.service; do
    [ -f "$f" ] || continue
    sed -i -E 's/^User=.*/User=root/; s/^Group=.*/Group=root/' "$f"
    grep -q '^User=' "$f" || sed -i '/^\[Service\]/a User=root' "$f"
    grep -q '^Group=' "$f" || sed -i '/^User=root/a Group=root' "$f"
done
if [ -f /etc/systemd/system/halo2.service ]; then
    grep -q '^Environment="HOME=/root"' /etc/systemd/system/halo2.service || sed -i '/^Group=root/a Environment="HOME=/root"' /etc/systemd/system/halo2.service
    if grep -q '^WorkingDirectory=' /etc/systemd/system/halo2.service; then
        sed -i 's#^WorkingDirectory=.*#WorkingDirectory=/root#' /etc/systemd/system/halo2.service
    else
        sed -i '/^Environment="HOME=\/root"/a WorkingDirectory=/root' /etc/systemd/system/halo2.service
    fi
fi
systemctl daemon-reload
if [ -f /etc/systemd/system/redis.service ] && [ -f /usr/lib/systemd/system/redis-server.service ] &&
   cmp -s /etc/systemd/system/redis.service /usr/lib/systemd/system/redis-server.service; then
    rm -f /etc/systemd/system/redis.service
    systemctl daemon-reload
fi
for s in tbox_server shadowsocks shadowsocks-rust waiwei-web waiwei-puller xray; do
    disable_if_exists "$s"
done
for s in mysql postgresql redis-server gitlab-runsvdir gitlab immich-ml auto_sync halo2 immich tbox_client tbox-logrotate.timer rblog rblog-backup.timer nginx cron; do
    restart_if_exists "$s"
done

(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_pg.sh'; echo '0 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_pg.sh > /dev/null 2>&1') | crontab -
(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_mysql.sh'; echo '5 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_mysql.sh > /dev/null 2>&1') | crontab -

echo '--- final states ---'
for s in auto_sync halo2 immich immich-ml tbox_server tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server rblog rblog-backup.timer gitlab-runsvdir gitlab shadowsocks shadowsocks-rust waiwei-web waiwei-puller xray; do
    resolved="$(unit_name "$s" 2>/dev/null || true)"
    if [ -n "$resolved" ]; then
        printf '  %s: ' "$s"; systemctl is-enabled "$resolved" 2>/dev/null | tr -d '\n'; printf ' / '; systemctl is-active "$resolved" 2>/dev/null | tr -d '\n'; echo
    elif [ "$s" = "gitlab" ] && command -v gitlab-ctl >/dev/null 2>&1 && gitlab-ctl status >/dev/null 2>&1; then
        printf '  %s: enabled / active\n' "$s"
    else
        printf '  %s: not-found / inactive\n' "$s"
    fi
done

wait_for_unit_active() {
    name="$1"
    resolved="$(unit_name "$name" 2>/dev/null || true)"
    [ -n "$resolved" ] || return 1
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45; do
        systemctl is-active --quiet "$resolved" && return 0
        state="$(systemctl is-active "$resolved" 2>/dev/null || true)"
        [ "$state" = "activating" ] || [ "$state" = "reloading" ] || break
        sleep 2
    done
    systemctl is-active --quiet "$resolved"
}
wait_for_unit_enabled() {
    name="$1"
    resolved="$(unit_name "$name" 2>/dev/null || true)"
    [ -n "$resolved" ] || return 1
    systemctl is-enabled --quiet "$resolved"
}
wait_for_https_200() {
    url="$1"
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
        code="$(curl -k -L --max-time 45 -o /dev/null -s -w '%{http_code}' "$url" || true)"
        [ "$code" = "200" ] && return 0
        sleep 10
    done
    log "ERROR: $url did not return HTTP 200; last status: $code"
    return 1
}

required_failed=0
for s in auto_sync halo2 immich immich-ml tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server rblog rblog-backup.timer; do
    if ! wait_for_unit_active "$s"; then
        log "ERROR: required service $s is not active"
        required_failed=1
    fi
    if ! wait_for_unit_enabled "$s"; then
        log "ERROR: required service $s is not enabled"
        required_failed=1
    fi
done
if ! pg_isready -q; then
    log "ERROR: PostgreSQL is not accepting connections"
    required_failed=1
fi
if ! command -v gitlab-ctl >/dev/null 2>&1 || ! gitlab-ctl status >/dev/null 2>&1; then
    log "ERROR: required service gitlab is not active"
    required_failed=1
fi
for url in https://code.xiedeacc.com https://unlock-music.xiedeacc.com https://halo.xiedeacc.com https://immich.xiedeacc.com https://blog.xiedeacc.com https://rblog.xiedeacc.com; do
    if ! wait_for_https_200 "$url"; then
        required_failed=1
    fi
done
exit "$required_failed"
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
