$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected OpenWrt tree back to the router with the
# bundled ssh/scp, fixes permissions, points shadowsocks at the aws server, makes
# the config take effect and (re)starts shadowsocks (not shadowsocks-rust).
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$opts    = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-o','ConnectTimeout=15')
$sshArgs = @() + $opts
$scpArgs = @('-r','-p') + $opts
if (-not [string]::IsNullOrEmpty($env:AS_PORT)) { $sshArgs += @('-p', $env:AS_PORT); $scpArgs += @('-P', $env:AS_PORT) }
if (-not [string]::IsNullOrEmpty($env:AS_KEY))  { $sshArgs += @('-i', $env:AS_KEY);  $scpArgs += @('-i', $env:AS_KEY) }

$errCount = 0
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })

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
    $item = Get-Item -LiteralPath $local -Force
    if (-not $item.PSIsContainer) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        Copy-Item -LiteralPath $local -Destination $target -Force
        Write-Host "stage $remote"
        return
    }

    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Write-Host "stage $remote"
    $baseLen = $item.FullName.TrimEnd([char[]]"\/").Length
    Get-ChildItem -LiteralPath $local -Force -Recurse | ForEach-Object {
        $rel = $_.FullName.Substring($baseLen).TrimStart([char[]]"\/")
        $remoteChild = Join-RemotePath $remote $rel
        if (-not (Test-RemoteExcluded $remoteChild)) {
            $childTarget = Join-Path $target ($rel -replace '/','\')
            if ($_.PSIsContainer) {
                New-Item -ItemType Directory -Force -Path $childTarget | Out-Null
            } else {
                New-Item -ItemType Directory -Force -Path (Split-Path -Parent $childTarget) | Out-Null
                Copy-Item -LiteralPath $_.FullName -Destination $childTarget -Force
            }
        }
    }
}

# Send the remote script base64-encoded as a plain ASCII argv and decode it on
# the router. This avoids Windows PowerShell prepending a UTF-8 BOM to native
# stdin (which made the remote `sh` choke on the first token) and sidesteps all
# argv-quoting issues for multi-line scripts.
function Invoke-Remote([string]$Script) {
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Script))
    & $ssh @sshArgs $dest "echo $b64 | base64 -d | sh"
    if ($LASTEXITCODE -ne 0) { Write-Host "! remote step exit $LASTEXITCODE"; $script:errCount++ }
}

# 0. Provision the router before any local files are transferred: repoint apk
#    at the TUNA mirror, refresh indexes, swap busybox dnsmasq for dnsmasq-full
#    without dropping DNS, and ensure every required package is installed.
$provision = @'
set -u
TUNA=http://mirrors.tuna.tsinghua.edu.cn/openwrt
FEEDS=/etc/apk/repositories.d/distfeeds.list

# apk feeds -> TUNA mirror (only rewrites the official downloads.openwrt.org host)
if [ -f "$FEEDS" ] && grep -Eq "https?://downloads\.openwrt\.org" "$FEEDS"; then
    sed -i -E "s#https?://downloads\.openwrt\.org#$TUNA#g" "$FEEDS"
    echo "feeds repointed -> $TUNA"
else
    echo "feeds already on tuna mirror"
fi

apk update || echo "!! apk update failed"

# busybox dnsmasq -> dnsmasq-full. Removing dnsmasq kills the router's own
# DNS (resolv.conf -> 127.0.0.1), so point the resolver at a public server
# for the swap, then let netifd restore it.
if apk list -I 2>/dev/null | grep -q "^dnsmasq-full-[0-9]"; then
    echo "dnsmasq-full already installed"
else
    echo "swapping busybox dnsmasq -> dnsmasq-full (keeping DNS up)"
    RESOLV=$(readlink -f /etc/resolv.conf 2>/dev/null); [ -n "$RESOLV" ] || RESOLV=/tmp/resolv.conf
    printf "nameserver 223.5.5.5\nnameserver 119.29.29.29\n" > "$RESOLV"
    apk del dnsmasq 2>/dev/null || true
    apk add dnsmasq-full || echo "!! dnsmasq-full install FAILED"
    /etc/init.d/dnsmasq restart 2>/dev/null || true
    /etc/init.d/network reload 2>/dev/null || true
fi

# busybox `vi` is a builtin applet (cannot be apk-removed); install vim instead.
apk add vim-full 2>/dev/null || echo "!! vim-full failed"

apk add losetup resize2fs usbutils block-mount fdisk e2fsprogs blkid hdparm ipset libnettle8 libnetfilter-conntrack3 || echo "!! pkg set 1 partial"
apk add kmod-fs-ext4 kmod-fs-vfat kmod-usb-storage kmod-usb-storage-uas grep diffutils dnsmasq-full coreutils-stat kmod-tcp-bbr || echo "!! pkg set 2 partial"
apk add coreutils-base64 ca-certificates ca-bundle ip-full curl wget grep nftables kmod-nft-core kmod-nft-nat kmod-nft-tproxy kmod-nft-socket vim-full || echo "!! pkg set 3 partial"

echo "provisioning complete"
'@
Write-Host 'provisioning router (apk mirror + packages)'
Invoke-Remote $provision

# 1. Prepare parent directories before file transfer. Do not stop services here:
#    keep the router running while collected files are copied back.
Invoke-Remote @'
mkdir -p /etc/init.d /etc/sysctl.d /etc/config /usr/local
'@

# 2. Push every path in the Collector list, excluding anything in Ignore.
$localStage = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_openwrt_deploy_stage_" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $localStage | Out-Null
try {
    foreach ($p in $collectPaths) {
        Copy-CollectedPathToStage $p $localStage
    }

    foreach ($entry in Get-ChildItem -LiteralPath $localStage -Force) {
        Write-Host "scp staged $($entry.Name)"
        & $scp @scpArgs -- $entry.FullName ('{0}:/' -f $dest)
        if ($LASTEXITCODE -ne 0) { Write-Host "! scp failed for staged $($entry.Name)"; $errCount++ }
    }
} finally {
    Remove-Item -LiteralPath $localStage -Recurse -Force -ErrorAction SilentlyContinue
}

# 3. Overwrite the remote client config with the server-substituted copy that
#    the engine prepared in Rust (family-matched to aws's hostname).
$confRemote = '/usr/local/shadowsocks/conf/shadowsocks-client.json'
if (-not [string]::IsNullOrWhiteSpace($env:AS_SS_CLIENT_CONF) -and (Test-Path -LiteralPath $env:AS_SS_CLIENT_CONF)) {
    Write-Host 'scp shadowsocks-client.json (server substituted by engine)'
    & $scp @scpArgs -- $env:AS_SS_CLIENT_CONF ('{0}:{1}' -f $dest, $confRemote)
    if ($LASTEXITCODE -ne 0) { Write-Host '! scp substituted conf failed'; $errCount++ }
} else {
    Write-Host '! AS_SS_CLIENT_CONF not provided; remote keeps the pushed conf (server not substituted)'
}

# 4. Restore the Unix permissions recorded at collect time (Windows dropped
#    them). The engine hands us the per-host cache via AS_PERMS_FILE.
if (-not [string]::IsNullOrWhiteSpace($env:AS_PERMS_FILE) -and (Test-Path -LiteralPath $env:AS_PERMS_FILE)) {
    $chmods = New-Object System.Collections.Generic.List[string]
    foreach ($line in [IO.File]::ReadAllLines($env:AS_PERMS_FILE)) {
        $t = $line.Trim(); if ($t -eq '') { continue }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        $chmods.Add("chmod $mode '$path'")
    }
    if ($chmods.Count -gt 0) {
        Write-Host "restoring $($chmods.Count) recorded permissions"
        Invoke-Remote ($chmods -join "`n")
    }
} else {
    Write-Host '! AS_PERMS_FILE not provided; skipping permission restore'
}

# 5. Apply the config and (re)start shadowsocks. Network reload is last because
#    it can briefly drop this ssh session.
$remote = @'
# apply sysctl
sysctl -p /etc/sysctl.conf >/dev/null 2>&1 || true
for f in /etc/sysctl.d/*.conf; do [ -f $f ] && sysctl -p $f >/dev/null 2>&1 || true; done

# apply dhcp config
/etc/init.d/dnsmasq restart 2>/dev/null || true
/etc/init.d/odhcpd restart 2>/dev/null || true

# shadowsocks (sslocal) on; shadowsocks-rust (sslocal-master) off to avoid a
# procd name clash (both init scripts use NAME=shadowsocks-rust).
/etc/init.d/shadowsocks-rust disable 2>/dev/null || true
/etc/init.d/shadowsocks-rust stop 2>/dev/null || true
/etc/init.d/shadowsocks enable
/etc/init.d/shadowsocks restart
# procd spawns sslocal asynchronously; give it a few seconds before checking.
i=0; while [ $i -lt 10 ]; do pgrep -f /usr/local/shadowsocks/bin/sslocal >/dev/null && break; sleep 1; i=$((i+1)); done
echo "shadowsocks pgrep:"; pgrep -f /usr/local/shadowsocks/bin/sslocal || echo "  (not running)"

# apply network config last (may briefly drop this session)
/etc/init.d/network reload 2>/dev/null || true
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
