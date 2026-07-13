$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected OpenWrt tree back to an already prepared
# router at 192.168.2.1, fixes permissions, points shadowsocks at the aws server,
# makes the config take effect and starts shadowsocks (not shadowsocks-rust).
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$usingPassword = -not [string]::IsNullOrEmpty($env:AS_PASSWORD)
$opts    = @('-o','StrictHostKeyChecking=accept-new','-o','ConnectTimeout=15','-o','NumberOfPasswordPrompts=1')
if ($usingPassword) {
    $opts += @('-o','BatchMode=no','-o','PreferredAuthentications=publickey,password,keyboard-interactive')
} else {
    $opts += @('-o','BatchMode=yes')
}
$sshArgs = @() + $opts
$scpArgs = @('-r') + $opts
if (-not [string]::IsNullOrEmpty($env:AS_PORT)) { $sshArgs += @('-p', $env:AS_PORT); $scpArgs += @('-P', $env:AS_PORT) }
if (-not [string]::IsNullOrEmpty($env:AS_KEY))  { $sshArgs += @('-i', $env:AS_KEY);  $scpArgs += @('-i', $env:AS_KEY) }

$askpassDir = $null
if ($usingPassword) {
    $askpassDir = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_ssh_askpass_" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $askpassDir | Out-Null
    $askpassPs1 = Join-Path $askpassDir 'askpass.ps1'
    $askpassCmd = Join-Path $askpassDir 'askpass.cmd'
    $escapedPassword = $env:AS_PASSWORD.Replace("'", "''")
    [IO.File]::WriteAllText($askpassPs1, "Write-Output '$escapedPassword'`r`n", [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($askpassCmd, "@echo off`r`npowershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$askpassPs1`"`r`n", [Text.ASCIIEncoding]::new())
    $env:SSH_ASKPASS = $askpassCmd
    $env:SSH_ASKPASS_REQUIRE = 'force'
    if ([string]::IsNullOrEmpty($env:DISPLAY)) { $env:DISPLAY = 'auto_sync:0' }
}

function Clear-Askpass {
    if ($script:askpassDir) {
        Remove-Item -LiteralPath $script:askpassDir -Recurse -Force -ErrorAction SilentlyContinue
        $script:askpassDir = $null
    }
}

$errCount = 0
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })
$deployExcludePaths = @(
    '/usr/local/shadowsocks/bin/sslocal-master',
    '/usr/local/shadowsocks/bin/sslocal-master-redir-nft.sh',
    '/usr/local/shadowsocks/data/source',
    '/usr/local/shadowsocks/data/temp/dns_cache.jsonl',
    '/usr/local/shadowsocks/data/temp/domain_conflicts.jsonl',
    '/usr/local/shadowsocks/data/temp/ip_conflicts.jsonl',
    '/usr/local/shadowsocks/logs',
    '/usr/local/shadowsocks/data/record.txt'
)

function Require-LocalValue([string]$Name, [string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) {
        throw "$Name is required"
    }
}

Require-LocalValue 'AS_SSH' $ssh
Require-LocalValue 'AS_SCP' $scp
Require-LocalValue 'AS_DEST' $dest
Require-LocalValue 'AS_ROOT' $root
if ($dest -notmatch '(^|@)192\.168\.2\.1$') { throw "AS_DEST must target 192.168.2.1; got: $dest" }
if (-not (Get-Command $ssh -ErrorAction SilentlyContinue)) { throw "AS_SSH not found: $ssh" }
if (-not (Get-Command $scp -ErrorAction SilentlyContinue)) { throw "AS_SCP not found: $scp" }
if (-not (Test-Path -LiteralPath $root)) { throw "AS_ROOT not found: $root" }
if ($collectPaths.Count -eq 0) { throw 'AS_COLLECT_PATHS is empty' }

function Stop-IfErrors([string]$Phase) {
    if ($script:errCount -gt 0) {
        throw "$Phase failed with $script:errCount error(s); stopping before service/network changes"
    }
}

function Test-RemoteConnection([string]$Target) {
    & $ssh @sshArgs $Target "true" *> $null
    return $LASTEXITCODE -eq 0
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
    foreach ($ex in $deployExcludePaths) {
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
        $script:errCount++
        return
    }

    $target = Join-Path $StageRoot ($remote.TrimStart([char[]]"/") -replace '/','\')
    Copy-ResolvedTree $local $target $remote @{}
    Write-Host "stage $remote"
}

# Send the remote script as a single shell-quoted argv and feed it to `sh` via
# the POSIX `printf` builtin, avoiding stdin encoding and argv-quoting issues.
function Quote-RemoteShellArg([string]$Value) {
    return "'" + $Value.Replace("'", "'""'""'") + "'"
}

function Invoke-Remote([string]$Script) {
    $quoted = Quote-RemoteShellArg $Script
    & $ssh @sshArgs $dest "printf '%s' $quoted | sh"
    if ($LASTEXITCODE -ne 0) {
        $script:errCount++
        throw "remote step exit $LASTEXITCODE"
    }
}

if (-not (Test-RemoteConnection $dest)) {
    throw "cannot connect to $dest; prepare OpenWrt on 192.168.2.1 before running this deploy script"
}

# Fail before touching services or files if the prepared OpenWrt image is still
# missing tooling that this Windows deploy path relies on.
Write-Host 'checking OpenWrt deploy prerequisites'
Invoke-Remote @'
set -u
missing=0

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "!! missing command: $1" >&2
        missing=1
    }
}

need_exec() {
    [ -x "$1" ] || {
        echo "!! missing executable: $1 ($2)" >&2
        missing=1
    }
}

need_init() {
    [ -x "/etc/init.d/$1" ] || {
        echo "!! missing init script: /etc/init.d/$1" >&2
        missing=1
    }
}

need_exec /usr/libexec/sftp-server "install openssh-sftp-server for Windows scp"
need_cmd ubus
need_cmd uci
need_cmd ip
need_cmd nft
need_cmd netstat
need_cmd pgrep
need_cmd grep
need_cmd start-stop-daemon
need_init dnsmasq
need_init dropbear
need_init network

if ! ip rule show >/dev/null 2>&1; then
    echo "!! ip command cannot show policy rules; install ip-full" >&2
    missing=1
fi

if ! nft list tables >/dev/null 2>&1; then
    echo "!! nft command cannot list tables; install nftables and nft kernel modules" >&2
    missing=1
fi

if ! ubus list >/dev/null 2>&1; then
    echo "!! ubus is unavailable" >&2
    missing=1
elif ! ubus list service >/dev/null 2>&1; then
    echo "!! procd service registry is unavailable on ubus" >&2
    missing=1
fi

[ "$missing" -eq 0 ] || exit 1
echo "OpenWrt deploy prerequisites OK"
'@

# 1. Prepare parent directories before file transfer. The router must already
#    have the required OpenWrt packages and filesystem layout.
Invoke-Remote @'
mkdir -p /etc/init.d /etc/sysctl.d /etc/config /usr/local
/etc/init.d/shadowsocks stop 2>/dev/null || true
/etc/init.d/shadowsocks-rust disable 2>/dev/null || true
/etc/init.d/shadowsocks-rust stop 2>/dev/null || true
for svc in waiwei-web waiwei-puller xray; do
  /etc/init.d/$svc disable 2>/dev/null || true
  /etc/init.d/$svc stop 2>/dev/null || true
done
killall sslocal sslocal-master xray waiwei-web waiwei-puller 2>/dev/null || true
rm -rf \
  /usr/local/shadowsocks/bin/sslocal-master \
  /usr/local/shadowsocks/bin/sslocal-master-redir-nft.sh \
  /usr/local/shadowsocks/data/source \
  /usr/local/shadowsocks/data/temp/dns_cache.jsonl \
  /usr/local/shadowsocks/data/temp/domain_conflicts.jsonl \
  /usr/local/shadowsocks/data/temp/ip_conflicts.jsonl \
  /usr/local/shadowsocks/logs \
  /usr/local/shadowsocks/data/record.txt
'@

# 2. Push every path in the Collector list, excluding anything in Ignore.
$localStage = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_openwrt_deploy_stage_" + [guid]::NewGuid().ToString('N'))
$remoteStage = '/tmp/auto_sync_openwrt_stage'
New-Item -ItemType Directory -Force -Path $localStage | Out-Null
try {
    foreach ($p in $collectPaths) {
        Copy-CollectedPathToStage $p $localStage
    }

    Invoke-Remote "rm -rf $remoteStage; mkdir -p $remoteStage"
    foreach ($entry in Get-ChildItem -LiteralPath $localStage -Force) {
        Write-Host "scp staged $($entry.Name)"
        & $scp @scpArgs -- $entry.FullName ('{0}:{1}/' -f $dest, $remoteStage)
        if ($LASTEXITCODE -ne 0) { Write-Host "! scp failed for staged $($entry.Name)"; $errCount++ }
    }
} finally {
    Remove-Item -LiteralPath $localStage -Recurse -Force -ErrorAction SilentlyContinue
}
Stop-IfErrors 'file transfer'

# Windows staging directories do not carry meaningful Unix modes. If scp/SFTP
# applies those modes to existing top-level OpenWrt directories, daemons running
# as non-root users can lose access to /etc and /usr. Install from a remote
# stage so scp never writes attributes onto those live top-level directories.
Invoke-Remote @'
set -e
stage=/tmp/auto_sync_openwrt_stage
chmod 755 /etc /etc/config /etc/init.d /etc/sysctl.d /usr /usr/local 2>/dev/null || true
if [ -d "$stage" ]; then
    for entry in "$stage"/* "$stage"/.[!.]* "$stage"/..?*; do
        [ -e "$entry" ] || [ -L "$entry" ] || continue
        name=${entry##*/}
        case "$name" in .|..) continue ;; esac
        dst="/$name"
        if [ -d "$entry" ] && [ ! -L "$entry" ]; then
            mkdir -p "$dst"
            cp -R "$entry"/. "$dst"/
        else
            cp -f "$entry" "$dst"
        fi
    done
    rm -rf "$stage"
fi
chmod 755 /etc /etc/config /etc/init.d /etc/sysctl.d /usr /usr/local 2>/dev/null || true
'@

# 3. Overwrite the remote client config with the server-substituted copy that
#    the engine prepared in Rust (family-matched to aws's hostname).
$confRemote = '/usr/local/shadowsocks/conf/shadowsocks-client.json'
if (-not [string]::IsNullOrWhiteSpace($env:AS_SS_CLIENT_CONF) -and (Test-Path -LiteralPath $env:AS_SS_CLIENT_CONF)) {
    Write-Host 'scp shadowsocks-client.json (server substituted by engine)'
    & $scp @scpArgs -- $env:AS_SS_CLIENT_CONF ('{0}:{1}' -f $dest, $confRemote)
    if ($LASTEXITCODE -ne 0) { Write-Host '! scp substituted conf failed'; $errCount++ }
} else {
    Write-Host '! AS_SS_CLIENT_CONF not provided; refusing to restart with a possibly stale server config'
    $errCount++
}
Stop-IfErrors 'shadowsocks-client.json transfer'

# 4. Restore the Unix permissions recorded at collect time (Windows dropped
#    them). The engine hands us the per-host cache via AS_PERMS_FILE.
function Quote-ShellArg([string]$Value) {
    return "'" + $Value.Replace("'", "'""'""'") + "'"
}

function Test-ProtectedModePath([string]$Path) {
    $p = Normalize-RemotePath $Path
    return @(
        '/etc',
        '/etc/config',
        '/etc/init.d',
        '/etc/sysctl.d',
        '/usr',
        '/usr/local',
        '/usr/local/shadowsocks',
        '/usr/local/shadowsocks/bin',
        '/usr/local/shadowsocks/conf',
        '/usr/local/shadowsocks/data',
        '/usr/local/shadowsocks/data/temp'
    ) -contains $p
}

if (-not [string]::IsNullOrWhiteSpace($env:AS_PERMS_FILE) -and (Test-Path -LiteralPath $env:AS_PERMS_FILE)) {
    $links = New-Object System.Collections.Generic.List[string]
    $chmods = New-Object System.Collections.Generic.List[string]
    foreach ($line in [IO.File]::ReadAllLines($env:AS_PERMS_FILE)) {
        $t = $line.Trim(); if ($t -eq '') { continue }
        if ($t.StartsWith('symlink ')) {
            $rest = $t.Substring(8)
            $sp = $rest.IndexOf(' '); if ($sp -lt 1) { continue }
            $targetB64 = $rest.Substring(0, $sp); $path = $rest.Substring($sp + 1)
            if (-not ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/'))) { continue }
            try { $target = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($targetB64)) } catch { continue }
            $qPath = Quote-ShellArg $path
            $qTarget = Quote-ShellArg $target
            $links.Add("mkdir -p -- `$(dirname -- $qPath) 2>/dev/null || true; rm -rf -- $qPath 2>/dev/null || true; ln -s -- $qTarget $qPath 2>/dev/null || true")
            continue
        }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        if (-not ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/'))) { continue }
        if (Test-ProtectedModePath $path) { continue }
        $qPath = Quote-ShellArg $path
        $chmods.Add("[ -e $qPath ] && chmod $mode $qPath || true")
    }
    if ($links.Count -gt 0) {
        Write-Host "restoring $($links.Count) recorded symlink(s)"
        Invoke-Remote ($links -join "`n")
    }
    if ($chmods.Count -gt 0) {
        Write-Host "restoring $($chmods.Count) recorded permissions"
        Invoke-Remote ($chmods -join "`n")
    }
} else {
    Write-Host '! AS_PERMS_FILE not provided; skipping permission restore'
}
Stop-IfErrors 'permission restore'

Invoke-Remote @'
chmod 755 /etc /etc/config /etc/init.d /etc/sysctl.d /usr /usr/local /usr/local/shadowsocks /usr/local/shadowsocks/bin /usr/local/shadowsocks/conf /usr/local/shadowsocks/data /usr/local/shadowsocks/data/temp 2>/dev/null || true
'@

# 5. Apply the config and (re)start shadowsocks. Network reload is last because
#    it can briefly drop this ssh session.
$remote = @'
set -u

wait_ubus() {
    i=0
    while [ $i -lt 20 ]; do
        if ubus list >/dev/null 2>&1 && ubus list service >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        i=$((i+1))
    done
    echo "!! ubus/procd service registry is not ready" >&2
    return 1
}

ensure_ubus() {
    wait_ubus || {
        echo "!! ubus is unavailable; refusing to restart ubusd from deploy script" >&2
        echo "!! fix OpenWrt ubus/rpcd/uhttpd first, then rerun deploy" >&2
        return 1
    }
}

sslocal_running() {
    pgrep -f '/usr/local/shadowsocks/bin/sslocal([[:space:]]|$)' >/dev/null 2>&1 ||
        ps w 2>/dev/null | grep -q '[s]slocal -c /usr/local/shadowsocks/conf/shadowsocks-client.json'
}

dns_listener_ready() {
    netstat -ln 2>/dev/null | grep -Eq '(^|[.:])1053[[:space:]]'
}

validate_lan_dhcp_config() {
    iface=$(uci -q get dhcp.lan.interface 2>/dev/null || true)
    ignore=$(uci -q get dhcp.lan.ignore 2>/dev/null || true)
    start=$(uci -q get dhcp.lan.start 2>/dev/null || true)
    limit=$(uci -q get dhcp.lan.limit 2>/dev/null || true)
    if [ "$iface" != lan ]; then
        echo "!! dhcp.lan.interface is '$iface', expected 'lan'" >&2
        return 1
    fi
    if [ "$ignore" = 1 ]; then
        echo "!! dhcp.lan.ignore=1, LAN DHCP is disabled" >&2
        return 1
    fi
    if [ -z "$start" ] || [ -z "$limit" ]; then
        echo "!! dhcp.lan start/limit is missing" >&2
        return 1
    fi
}

schedule_network_reload() {
    # Network reload can reset this SSH connection. Detach it so the deploy
    # script can report success after all preflight checks have passed.
    rm -f /var/run/auto_sync_network_reload.pid /tmp/auto_sync_network_reload.log
    start-stop-daemon -S -b -m -p /var/run/auto_sync_network_reload.pid -x /bin/sh -- -c '
        exec >/tmp/auto_sync_network_reload.log 2>&1 </dev/null
        sleep 1
        /etc/init.d/network reload || true
        sleep 6
        /etc/init.d/dnsmasq restart || true
        /etc/init.d/dropbear restart || true
    ' || {
        echo "!! failed to schedule network reload" >&2
        return 1
    }
}

ensure_ubus || exit 1

# apply sysctl
sysctl -p /etc/sysctl.conf >/dev/null 2>&1 || true
for f in /etc/sysctl.d/*.conf; do [ -f $f ] && sysctl -p $f >/dev/null 2>&1 || true; done

# shadowsocks (sslocal) on; legacy shadowsocks-rust (sslocal-master) present
# for collection/round-trip but disabled and stopped.
/etc/init.d/shadowsocks-rust disable 2>/dev/null || true
/etc/init.d/shadowsocks-rust stop 2>/dev/null || true
for svc in waiwei-web waiwei-puller xray; do
  /etc/init.d/$svc disable 2>/dev/null || true
  /etc/init.d/$svc stop 2>/dev/null || true
done
killall sslocal-master xray waiwei-web waiwei-puller 2>/dev/null || true
ensure_ubus || exit 1

[ -x /etc/init.d/shadowsocks ] || { echo "!! missing /etc/init.d/shadowsocks" >&2; exit 1; }
[ -x /usr/local/shadowsocks/bin/sslocal ] || { echo "!! missing /usr/local/shadowsocks/bin/sslocal" >&2; exit 1; }
[ -s /usr/local/shadowsocks/conf/shadowsocks-client.json ] || { echo "!! missing shadowsocks-client.json" >&2; exit 1; }

/etc/init.d/shadowsocks enable || exit 1
/etc/init.d/shadowsocks start || exit 1

# procd spawns sslocal asynchronously; verify both the process and DNS listener
# before switching dnsmasq to 127.0.0.1#1053.
i=0
while [ $i -lt 15 ]; do
    if sslocal_running && dns_listener_ready; then
        break
    fi
    sleep 1
    i=$((i+1))
done

echo "shadowsocks pgrep:"
pgrep -f /usr/local/shadowsocks/bin/sslocal || echo "  (not running)"
if ! sslocal_running || ! dns_listener_ready; then
    echo "!! shadowsocks did not start cleanly; leaving dnsmasq/network untouched" >&2
    logread 2>/dev/null | grep -iE 'shadowsocks|sslocal|procd|ubus' | tail -n 80 >&2 || true
    exit 1
fi

# Only now apply dhcp DNS forwarding to sslocal's DNS listener.
validate_lan_dhcp_config || exit 1
/etc/init.d/dnsmasq restart || exit 1
/etc/init.d/odhcpd restart 2>/dev/null || true

# apply network config last (may briefly drop this session)
schedule_network_reload || exit 1
echo "network reload scheduled in background"
'@
Invoke-Remote $remote

if ($errCount -gt 0) {
    Write-Host "deploy completed with $errCount error(s)"
    Clear-Askpass
    exit 1
}
Clear-Askpass
Write-Host 'deploy completed cleanly'
