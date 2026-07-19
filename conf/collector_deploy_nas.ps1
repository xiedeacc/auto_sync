$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected NAS tree back to the Ubuntu host, then
# applies packages, services, runtimes, quotas, ZFS standby handling, and cron.
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
$remoteScratch = '/tmp/auto_sync_deploy_scratch'
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$platformDefaultCollectPaths = @()
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })

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
    $nameManifest = Join-Path $root '.auto_sync_name_manifest.json'
    if (Test-Path -LiteralPath $nameManifest) {
        [void]$tarPaths.Add('.auto_sync_name_manifest.json')
        Write-Host "stage /.auto_sync_name_manifest.json"
    }

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

function Invoke-NameManifestRehydrate([string]$RemoteStage) {
    $qStage = Quote-ShellArg $RemoteStage
    Invoke-Remote @"
set -e
stage=$qStage
export AUTO_SYNC_STAGE="`$stage"
python3 - <<'PY'
import json, os, shutil
stage = os.environ.get("AUTO_SYNC_STAGE", "")
if not stage:
    raise SystemExit("AUTO_SYNC_STAGE is empty")
manifest_path = os.path.join(stage, ".auto_sync_name_manifest.json")
if not os.path.exists(manifest_path):
    raise SystemExit(0)
with open(manifest_path, "r", encoding="utf-8") as f:
    manifest = json.load(f)
entries = manifest.get("entries", [])
def safe_rel(rel):
    parts = [p for p in rel.split("/") if p]
    if any(p in (".", "..") for p in parts):
        raise RuntimeError(f"unsafe stored_rel: {rel}")
    return parts
for entry in sorted(entries, key=lambda e: e["stored_rel"].count("/"), reverse=True):
    stored_parts = safe_rel(entry["stored_rel"])
    stored = os.path.join(stage.encode(), *[p.encode("utf-8") for p in stored_parts])
    original = stage.encode() + b"/" + bytes.fromhex(entry["original_rel_hex"])
    if not os.path.lexists(stored):
        continue
    os.makedirs(os.path.dirname(original), exist_ok=True)
    if os.path.lexists(original):
        if os.path.isdir(original) and not os.path.islink(original):
            shutil.rmtree(original)
        else:
            os.unlink(original)
    os.rename(stored, original)
for root, dirs, files in os.walk(stage.encode(), topdown=False):
    if os.path.basename(root) == b".auto_sync_name":
        try:
            os.rmdir(root)
        except OSError:
            pass
os.unlink(manifest_path)
print(f"rehydrated {len(entries)} Linux byte-name path(s)")
PY
"@
}

function Ensure-RemoteRootWritable {
    Invoke-Remote @'
set -eu
if ! touch /etc/.auto_sync_rw_test 2>/dev/null; then
    mount -o remount,rw /
fi
touch /etc/.auto_sync_rw_test
rm -f /etc/.auto_sync_rw_test
'@
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
$remoteStage = '/tmp/auto_sync_deploy_stage'
Invoke-Remote "rm -rf $(Quote-ShellArg $remoteStage); mkdir -p $(Quote-ShellArg $remoteStage)"
$generatedOptionalPaths = $platformDefaultCollectPaths
$requiredCollectPaths = $collectPaths
Transfer-CollectedPathsToStage $requiredCollectPaths $generatedOptionalPaths $remoteStage
Invoke-NameManifestRehydrate $remoteStage
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
rm -f /etc/ssh/sshd_config.d/50-cloud-init.conf /etc/ssh/sshd_config.d/99-auto-sync-test.conf
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
        log "enabled $unit"
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
    [ "$pids" = "0" ] && pids=none
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
    for s in mysql postgresql redis-server auto_sync tbox_server tbox_client tbox-logrotate.timer rgit rgit-backup.service rgit-backup.timer rgit-ocsp.service rgit-ocsp.timer domus domus-backup.service domus-backup.timer nginx cron shadowsocks shadowsocks-rust waiwei-web waiwei-puller xray; do
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
        log_unit_processes "$unit"
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
    for d in /opt/usr/local/domus /opt/usr/local/tbox /opt/usr/local/waiwei /opt/usr/local/xray /opt/usr/local/shadowsocks; do
        [ -e "$d" ] || continue
        chown -R root:root "$d" 2>/dev/null || true
        find "$d" -type d -exec chmod 755 {} + 2>/dev/null || true
        find "$d" -type f -exec chmod 644 {} + 2>/dev/null || true
    done
    if [ -d /opt/usr/local/rgit ]; then
        chown root:root /opt/usr/local/rgit /opt/usr/local/rgit/bin 2>/dev/null || true
        find /opt/usr/local/rgit/bin -type f -exec chown root:root {} + -exec chmod 755 {} + 2>/dev/null || true
        for d in /opt/usr/local/rgit/conf /opt/usr/local/rgit/data /opt/usr/local/rgit/logs /opt/usr/local/rgit/.ssh /opt/usr/local/rgit/.backup-worktree; do
            [ -e "$d" ] && chown -R git:git "$d" 2>/dev/null || true
        done
        [ -d /opt/usr/local/rgit/conf ] && chmod 750 /opt/usr/local/rgit/conf 2>/dev/null || true
        [ -d /opt/usr/local/rgit/data ] && chmod 700 /opt/usr/local/rgit/data 2>/dev/null || true
        [ -d /opt/usr/local/rgit/logs ] && chmod 750 /opt/usr/local/rgit/logs 2>/dev/null || true
        [ -d /opt/usr/local/rgit/.ssh ] && chmod 700 /opt/usr/local/rgit/.ssh 2>/dev/null || true
    fi
    if [ -d /opt/usr/local/domus ]; then
        chown root:root /opt/usr/local/domus /opt/usr/local/domus/bin 2>/dev/null || true
        find /opt/usr/local/domus/bin -type f -exec chown root:root {} + -exec chmod 755 {} + 2>/dev/null || true
        for d in /opt/usr/local/domus/conf /opt/usr/local/domus/data /opt/usr/local/domus/logs /opt/usr/local/domus/.backup-worktree; do
            [ -e "$d" ] && chown -R root:root "$d" 2>/dev/null || true
        done
        [ -d /opt/usr/local/domus/conf ] && chmod 750 /opt/usr/local/domus/conf 2>/dev/null || true
        [ -d /opt/usr/local/domus/data ] && chmod 700 /opt/usr/local/domus/data 2>/dev/null || true
        [ -d /opt/usr/local/domus/logs ] && chmod 750 /opt/usr/local/domus/logs 2>/dev/null || true
    fi
    for d in \
        /opt/usr/local/domus/bin \
        /opt/usr/local/rgit/bin \
        /opt/usr/local/tbox/bin \
        /opt/usr/local/waiwei/bin \
        /opt/usr/local/waiwei/scripts \
        /opt/usr/local/xray/bin \
        /opt/usr/local/shadowsocks/bin \
        /opt/usr/local/bin
    do
        [ -d "$d" ] && find "$d" -type f -exec chmod 755 {} + 2>/dev/null || true
    done
    for d in /opt/usr/local /opt/src /opt/user; do
        [ -d "$d" ] && find "$d" -xdev \( -type d -o -type f \) \( -perm -0002 -o -perm -0020 \) -exec chmod go-w {} + 2>/dev/null || true
    done
    for f in \
        /etc/systemd/system/auto_sync.service \
        /etc/systemd/system/domus.service \
        /etc/systemd/system/domus-backup.service \
        /etc/systemd/system/domus-backup.timer \
        /etc/systemd/system/rgit.service \
        /etc/systemd/system/rgit-backup.service \
        /etc/systemd/system/rgit-backup.timer \
        /etc/systemd/system/rgit-backup.service.d/ssh.conf \
        /etc/systemd/system/rgit-ocsp.service \
        /etc/systemd/system/rgit-ocsp.timer \
        /etc/systemd/system/tbox_client.service \
        /etc/systemd/system/tbox-logrotate.service \
        /etc/systemd/system/tbox-logrotate.timer
    do
        [ -e "$f" ] && chown root:root "$f" 2>/dev/null || true
        [ -e "$f" ] && chmod 0644 "$f" 2>/dev/null || true
    done
}

install_staged_collected_paths() {
    stage="/tmp/auto_sync_deploy_stage"
    [ -d "$stage" ] || return 0
    log "install collected paths after package installation"
    install_staged_file() {
        rel="$1"
        target="/$rel"
        [ -f "$stage/$rel" ] || return 0
        mkdir -p "$(dirname "$target")"
        cp -a "$stage/$rel" "$target"
        chmod "${2:-644}" "$target" 2>/dev/null || true
    }
    rm -rf "$stage/root/src/share" 2>/dev/null || true
    rmdir "$stage/root/src" "$stage/root" 2>/dev/null || true
    for rel in \
        opt/usr/local/auto_sync/bin/auto_sync \
        opt/usr/local/auto_sync/bin/auto_syncd \
        opt/usr/local/auto_sync/bin/auto_sync_gui \
        opt/usr/local/tbox/bin/tbox_client \
        opt/usr/local/xray/bin/xray \
        opt/usr/local/xray/bin/update-geo.sh \
        opt/usr/local/waiwei/bin/waiwei_web \
        opt/usr/local/waiwei/bin/waiwei_puller \
        opt/usr/local/domus/bin/domus \
        opt/usr/local/domus/bin/domus-* \
        opt/usr/local/shadowsocks/bin/sslocal \
        opt/usr/local/shadowsocks/bin/ssserver \
        opt/usr/local/shadowsocks/bin/xray-plugin
    do
        [ -e "$stage/$rel" ] || continue
        rm -f "/$rel" 2>/dev/null || true
    done
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

if [ -f /etc/apt/sources.list ]; then
    sed -i 's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#https://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#https://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' /etc/apt/sources.list
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
mkdir -p /opt/src/software
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
/root/.cscope.vim|/opt/user/root/.cscope.vim|dir|root:root
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
cmp -s /usr/share/zoneinfo/Asia/Shanghai /etc/localtime 2>/dev/null || cp /usr/share/zoneinfo/Asia/Shanghai /etc/localtime

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
ensure_host_entry 127.0.0.1 unlock-music.xiedeacc.com
ensure_host_entry 127.0.0.1 halo.xiedeacc.com
ensure_host_entry 127.0.0.1 dev.xiedeacc.com
ensure_host_entry 127.0.0.1 coverage.xiedeacc.com

swapoff -a || true
sed -i '/^\/swap\.img[[:space:]]/s/^/#/' /etc/fstab 2>/dev/null || true
rm -f /swap.img

ensure_auto_sync_profile_block() {
    touch /etc/profile
    tmp_profile="$(mktemp)"
    awk '
        /^# BEGIN auto_sync domestic mirrors$/ { skip = 1; next }
        /^# END auto_sync domestic mirrors$/ { skip = 0; next }
        /^# BEGIN auto_sync java environment$/ { skip = 1; next }
        /^# END auto_sync java environment$/ { skip = 0; next }
        skip == 0 { print }
    ' /etc/profile > "$tmp_profile"
    cat >> "$tmp_profile" <<'EOF_DOMESTIC_MIRRORS'

# BEGIN auto_sync java environment
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
export PATH="$JAVA_HOME/bin:$PATH"
# END auto_sync java environment
EOF_DOMESTIC_MIRRORS
    cat "$tmp_profile" > /etc/profile
    rm -f "$tmp_profile"
    chmod 0644 /etc/profile
}
ensure_auto_sync_profile_block
export GOPROXY=https://goproxy.cn,direct
export NVM_NODEJS_ORG_MIRROR=https://npmmirror.com/mirrors/node
export npm_config_registry=https://registry.npmmirror.com
export COREPACK_NPM_REGISTRY=https://registry.npmmirror.com
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
export PATH="$JAVA_HOME/bin:/opt/usr/local/bin:/opt/usr/local/go/go1.25.1/bin:$PATH"
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
cat > /root/.npmrc <<'EOF_NPM_MIRROR'
registry=https://registry.npmmirror.com
cache=/opt/user/root/.npm
EOF_NPM_MIRROR
chmod 0600 /root/.npmrc
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
    go env -w GOPROXY=https://goproxy.cn,direct || log "WARN: go mirror setup failed"
    go install github.com/google/pprof@latest || log "WARN: go install pprof failed"
fi

if [ ! -x /root/.cargo/bin/rustup ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | timeout --kill-after=10s 600s sh -s -- -y || log "WARN: rustup install failed"
fi

mkdir -p /opt/src/software/tools
export NVM_DIR=/root/.nvm
mkdir -p "$NVM_DIR"
if [ ! -s "$NVM_DIR/nvm.sh" ]; then
    curl -fsSL https://gitee.com/mirrors/nvm/raw/v0.40.3/install.sh | NVM_SOURCE=https://gitee.com/mirrors/nvm.git bash || log "WARN: nvm install failed"
fi
if [ -s "$NVM_DIR/nvm.sh" ]; then
    . "$NVM_DIR/nvm.sh"
    log "ensure node.js v24.18.0 via nvm"
    if [ -x "$NVM_DIR/versions/node/v24.18.0/bin/node" ]; then
        log "node.js v24.18.0 already installed"
    else
        log "install node.js v24.18.0"
        nvm install 24.18.0 >/dev/null 2>&1 || nvm install 24 >/dev/null 2>&1 || log "WARN: nvm install 24 failed"
    fi
    nvm use 24.18.0 --silent >/dev/null || nvm use 24 --silent >/dev/null || true
    hash -r 2>/dev/null || true
    if ! command -v npm >/dev/null 2>&1; then
        log "npm missing after nvm use; reinstall Node v24.18.0"
        rm -rf "$NVM_DIR/versions/node/v24.18.0"
        nvm install 24.18.0 >/dev/null 2>&1 || nvm install 24 >/dev/null 2>&1 || log "WARN: nvm reinstall 24 failed"
        nvm use 24.18.0 --silent >/dev/null || nvm use 24 --silent >/dev/null || true
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
        ycm_path="$JAVA_HOME/bin:/opt/usr/local/go/go1.25.1/bin:/root/src/go/bin:/root/.cargo/bin:/root/.nvm/versions/node/v24.18.0/bin:$PATH"
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
if [ ! -d /opt/src/software/pgvector ]; then
    git clone https://github.com/pgvector/pgvector.git /opt/src/software/pgvector || true
fi
if [ -d /opt/src/software/pgvector ]; then
    (cd /opt/src/software/pgvector && make && make install) || log "WARN: pgvector build/install failed"
fi

setquota -u root 20000000 20000000 0 0 /dev/mmcblk0p2 2>/dev/null || true
for u in tiger git; do
    id "$u" >/dev/null 2>&1 && setquota -u "$u" 1000000 1000000 0 0 / 2>/dev/null || true
done

wake_zfs() {
    if [ ! -d /zfs ]; then
        log "skip /zfs wake: /zfs not found"
        return 1
    fi
    log "wake /zfs before standby handling"
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

ensure_postgresql_ready() {
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

zfs_woken=0
wake_zfs && zfs_woken=1 || true
id tiger >/dev/null 2>&1 || useradd -m -s /bin/bash tiger
if [ "$zfs_woken" = "1" ]; then
    standby_zfs
fi

mkdir -p /opt/usr/local
chmod 0755 /opt/usr /opt/usr/local 2>/dev/null || true

mkdir -p /opt/usr/local/auto_sync/logs /opt/usr/local/domus/logs /opt/usr/local/rgit/logs /opt/usr/local/tbox/log /opt/usr/local/waiwei/logs /opt/usr/local/xray/logs /opt/usr/local/shadowsocks/logs /opt/usr/local/shadowsocks/conf /opt/usr/local/shadowsocks/data /opt/user/tiger /home/tiger
migrate_root_home_to_opt() {
    mkdir -p /opt/user/root

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
[ -e /opt/user/tiger ] && chown -R tiger:tiger /opt/user/tiger 2>/dev/null || true
for d in /opt/usr/local/auto_sync /opt/usr/local/tbox /opt/usr/local/shadowsocks; do
    [ -e "$d" ] && chown -R root:root "$d" 2>/dev/null || true
done
if [ -d /opt/usr/local/domus ]; then
    chown root:root /opt/usr/local/domus /opt/usr/local/domus/bin 2>/dev/null || true
    find /opt/usr/local/domus/bin -type f -exec chown root:root {} + -exec chmod 755 {} + 2>/dev/null || true
    for d in /opt/usr/local/domus/conf /opt/usr/local/domus/data /opt/usr/local/domus/logs /opt/usr/local/domus/.backup-worktree; do
        [ -e "$d" ] && chown -R root:root "$d" 2>/dev/null || true
    done
fi
if [ -d /opt/usr/local/rgit ]; then
    chown root:root /opt/usr/local/rgit /opt/usr/local/rgit/bin 2>/dev/null || true
    find /opt/usr/local/rgit/bin -type f -exec chown root:root {} + -exec chmod 755 {} + 2>/dev/null || true
    for d in /opt/usr/local/rgit/conf /opt/usr/local/rgit/data /opt/usr/local/rgit/logs /opt/usr/local/rgit/.ssh /opt/usr/local/rgit/.backup-worktree; do
        [ -e "$d" ] && chown -R git:git "$d" 2>/dev/null || true
    done
fi
for f in \
    /opt/usr/local/auto_sync/bin/auto_sync \
    /opt/usr/local/auto_sync/bin/auto_syncd \
    /opt/usr/local/auto_sync/bin/auto_sync_gui \
    /opt/usr/local/tbox/bin/tbox_client \
    /opt/usr/local/xray/bin/xray \
    /opt/usr/local/xray/bin/update-geo.sh \
    /opt/usr/local/waiwei/bin/waiwei_web \
    /opt/usr/local/waiwei/bin/waiwei_puller \
    /opt/usr/local/domus/bin/* \
    /opt/usr/local/rgit/bin/* \
    /opt/usr/local/shadowsocks/bin/* \
    /opt/usr/local/bin/vlmcsd
do
    [ -e "$f" ] && chmod a+rx "$f" 2>/dev/null || true
done
chmod a+rx /root/.nvm/versions/node/v24.18.0/bin/node 2>/dev/null || true
normalize_deploy_permissions
ensure_domus_backup() {
    [ -d /opt/usr/local/domus ] || return 0
    mkdir -p /opt/usr/local/domus/bin /opt/usr/local/domus/conf /opt/usr/local/domus/data /opt/usr/local/domus/logs
    mkdir -p /root/.ssh
    ssh-keyscan -T 10 github.com >> /root/.ssh/known_hosts 2>/dev/null || true
    if [ ! -d /opt/usr/local/domus/.backup-worktree/.git ]; then
        rm -rf /opt/usr/local/domus/.backup-worktree
        git clone git@github.com:xiedeacc/domus_data.git /opt/usr/local/domus/.backup-worktree || log "WARN: domus_data backup clone failed"
    fi
    if [ ! -f /etc/systemd/system/domus.service ]; then
        cat > /etc/systemd/system/domus.service <<'EOF_DOMUS_SERVICE'
[Unit]
Description=Domus service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=/opt/usr/local/domus
Environment=RUST_LOG=info
ExecStart=/opt/usr/local/domus/bin/domus
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF_DOMUS_SERVICE
    fi
    cat > /opt/usr/local/domus/bin/domus-backup <<'EOF_DOMUS_BACKUP'
#!/usr/bin/env bash
set -euo pipefail
app=/opt/usr/local/domus
worktree="$app/.backup-worktree"
mkdir -p "$app/bin" "$app/conf" "$app/data" "$app/logs" /root/.ssh
ssh-keyscan -T 10 github.com >> /root/.ssh/known_hosts 2>/dev/null || true
export GIT_SSH_COMMAND='ssh -o StrictHostKeyChecking=accept-new'
if [ ! -d "$worktree/.git" ]; then
    rm -rf "$worktree"
    git clone git@github.com:xiedeacc/domus_data.git "$worktree"
fi
git -C "$worktree" config user.name auto_sync
git -C "$worktree" config user.email auto_sync@nas.local
git -C "$worktree" pull --ff-only || true
rm -rf "$worktree/bin" "$worktree/conf" "$worktree/data"
[ ! -e "$app/bin" ] || cp -a "$app/bin" "$worktree/bin"
[ ! -e "$app/conf" ] || cp -a "$app/conf" "$worktree/conf"
mkdir -p "$worktree/data"
if [ -d "$app/data" ]; then
    shopt -s dotglob nullglob
    for item in "$app/data"/* "$app/data"/.[!.]* "$app/data"/..?*; do
        name="${item##*/}"
        case "$name" in
            upload|backups) continue ;;
        esac
        cp -a "$item" "$worktree/data/$name"
    done
    shopt -u dotglob nullglob
fi
cd "$worktree"
git add -A -- bin data conf
if ! git diff --cached --quiet; then
    git commit -m "Backup domus data $(date -Is)"
fi
git pull --rebase --autostash || true
git push
EOF_DOMUS_BACKUP
    chmod 755 /opt/usr/local/domus/bin/domus-backup
    cat > /etc/systemd/system/domus-backup.service <<'EOF_DOMUS_BACKUP_SERVICE'
[Unit]
Description=Back up Domus runtime data
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=root
Group=root
ExecStart=/opt/usr/local/domus/bin/domus-backup
EOF_DOMUS_BACKUP_SERVICE
    cat > /etc/systemd/system/domus-backup.timer <<'EOF_DOMUS_BACKUP_TIMER'
[Unit]
Description=Run Domus runtime data backup periodically

[Timer]
OnCalendar=*-*-* 03:20:00
Persistent=true
Unit=domus-backup.service

[Install]
WantedBy=timers.target
EOF_DOMUS_BACKUP_TIMER
}
ensure_domus_backup
normalize_deploy_permissions
systemctl daemon-reload
systemctl reset-failed 2>/dev/null || true
rm -f /etc/nginx/sites-enabled/default /etc/nginx/conf.d/default.conf 2>/dev/null || true
for f in /etc/systemd/system/auto_sync.service /etc/systemd/system/domus.service /etc/systemd/system/tbox_client.service; do
    [ -f "$f" ] || continue
    sed -i -E 's/^User=.*/User=root/; s/^Group=.*/Group=root/' "$f"
    grep -q '^User=' "$f" || sed -i '/^\[Service\]/a User=root' "$f"
    grep -q '^Group=' "$f" || sed -i '/^User=root/a Group=root' "$f"
done
systemctl daemon-reload
if [ -f /etc/systemd/system/redis.service ] && [ -f /usr/lib/systemd/system/redis-server.service ] &&
   cmp -s /etc/systemd/system/redis.service /usr/lib/systemd/system/redis-server.service; then
    rm -f /etc/systemd/system/redis.service
    systemctl daemon-reload
fi
for s in tbox_server mysql postgresql redis-server shadowsocks shadowsocks-rust waiwei-web waiwei-puller xray; do
    disable_if_exists "$s"
done
for s in auto_sync tbox_client tbox-logrotate.timer rgit rgit-backup.timer rgit-ocsp.timer domus domus-backup.timer nginx cron; do
    restart_if_exists "$s"
done

crontab -l 2>/dev/null | grep -v -E '/root/src/share/(ubuntu/backup_pg|nas/backup_pg|ubuntu/backup_mysql|nas/backup_mysql)\.sh' | crontab -

print_final_states() {
    echo '--- final states ---'
    for s in auto_sync tbox_server tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server rgit rgit-backup.timer rgit-ocsp.timer domus domus-backup.timer shadowsocks shadowsocks-rust waiwei-web waiwei-puller xray; do
        resolved="$(unit_name "$s" 2>/dev/null || true)"
        if [ -n "$resolved" ]; then
            enabled="$(systemctl is-enabled "$resolved" 2>/dev/null || true)"
            active="$(systemctl is-active "$resolved" 2>/dev/null || true)"
            pid="$(systemctl show "$resolved" -p MainPID --value 2>/dev/null || true)"
            [ -n "$pid" ] && [ "$pid" != "0" ] || pid=none
            printf '  %s: %s / %s / pid=%s\n' "$s" "${enabled:-unknown}" "${active:-unknown}" "$pid"
        else
            printf '  %s: not-found / inactive / pid=none\n' "$s"
        fi
    done
}

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
for s in auto_sync tbox_client tbox-logrotate.timer nginx cron rgit rgit-backup.timer rgit-ocsp.timer domus domus-backup.timer; do
    if ! wait_for_unit_active "$s"; then
        log "ERROR: required service $s is not active"
        required_failed=1
    fi
    if ! wait_for_unit_enabled "$s"; then
        log "ERROR: required service $s is not enabled"
        required_failed=1
    fi
done
for url in https://unlock-music.xiedeacc.com; do
    if ! wait_for_https_200 "$url"; then
        required_failed=1
    fi
done
print_final_states
exit "$required_failed"
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
