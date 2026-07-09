$ErrorActionPreference = 'Stop'
# Runs on Windows. Brings aws to its desired state. aws logs in as `ubuntu`
# (non-root) with passwordless sudo, so root files are staged in the home dir
# and installed with `sudo rsync`; acme.sh runs as the ubuntu user (it owns
# /etc/nginx/ssl). Modes come from AS_PERMS_FILE, so scp is used without -p.
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$opts    = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-o','ConnectTimeout=20')
$sshArgs = @() + $opts
$scpArgs = @('-r') + $opts
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

# Remote commands run as the ubuntu user; root actions use `sudo` inline
# (passwordless). base64 stdin avoids the Windows-PowerShell UTF-8 BOM.
function Invoke-Remote([string]$Script) {
    $Script = $Script -replace "`r`n", "`n"
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Script))
    $b64 | & $ssh @sshArgs $dest 'tmp=$(mktemp /tmp/auto_sync_remote.XXXXXX); base64 -d > "$tmp" && sh "$tmp" </dev/null; rc=$?; rm -f "$tmp"; exit "$rc"'
    if ($LASTEXITCODE -ne 0) { Write-Host "! remote step exit $LASTEXITCODE"; $script:errCount++ }
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

    $tarArgs = @('-C', $root, '-cf', '-') + $excludeArgs + @($tarPaths)
    & tar @tarArgs | & $ssh @sshArgs $dest "tar -C $RemoteStage -xf -"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "! tar stage transfer failed"
        $script:errCount++
    }
}

# 1. Stage every path in the Collector list, excluding anything in Ignore, then
#    install with sudo rsync.
Invoke-Remote 'rm -rf ~/.auto_sync_stage; mkdir -p ~/.auto_sync_stage'
Transfer-CollectedPathsToStage $collectPaths @() '~/.auto_sync_stage'
# rsync without -o/-g; ownership for ubuntu-owned trees is fixed below.
Invoke-Remote 'sudo rsync -rltD --no-perms ~/.auto_sync_stage/ / && rm -rf ~/.auto_sync_stage && echo "installed collected paths"'

# 2. Restore recorded permissions for the pushed /etc files.
if (-not [string]::IsNullOrWhiteSpace($env:AS_PERMS_FILE) -and (Test-Path -LiteralPath $env:AS_PERMS_FILE)) {
    $chmods = New-Object System.Collections.Generic.List[string]
    foreach ($line in [IO.File]::ReadAllLines($env:AS_PERMS_FILE)) {
        $t = $line.Trim(); if ($t -eq '') { continue }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        if ($path.StartsWith('/etc/')) { $chmods.Add("sudo chmod $mode '$path' 2>/dev/null || true") }
    }
    if ($chmods.Count -gt 0) { Write-Host "restoring $($chmods.Count) /etc permissions"; Invoke-Remote ($chmods -join "`n") }
}

# 3. Operational setup: services, acme/ssl, backup.
$remote = @'
# Files installed by sudo rsync may be root-owned when they were created fresh.
# rblog and acme.sh run as ubuntu, so restore ownership for those trees.
for p in /home/ubuntu /usr/local/blog; do
    if [ -e "$p" ]; then
        sudo chown -R ubuntu:ubuntu "$p" || echo "!! chown $p FAILED"
    fi
done

# rblog backup: /usr/local/blog/.backup-worktree is the real blog_data checkout
# used by rblog-backup. Do not copy it back over /usr/local/blog.
BLOG_DIR=/usr/local/blog
BACKUP_WORKTREE=/usr/local/blog/.backup-worktree
BACKUP_REPO=git@github.com:xiedeacc/blog_data.git
if [ ! -d "$BLOG_DIR" ]; then
    sudo mkdir -p "$BLOG_DIR"
fi
sudo chown ubuntu:ubuntu "$BLOG_DIR" 2>/dev/null || true
if [ -d "$BACKUP_WORKTREE/.git" ]; then
    sudo -u ubuntu git -C "$BACKUP_WORKTREE" remote set-url origin "$BACKUP_REPO" \
        && echo "rblog backup worktree remote ok" \
        || echo "!! rblog backup worktree remote update FAILED"
    sudo -u ubuntu git -C "$BACKUP_WORKTREE" pull --ff-only \
        && echo "rblog backup worktree pulled" \
        || echo "!! rblog backup worktree pull FAILED"
elif [ -e "$BACKUP_WORKTREE" ]; then
    echo "!! $BACKUP_WORKTREE exists but is not a git checkout"
else
    sudo -u ubuntu git clone "$BACKUP_REPO" "$BACKUP_WORKTREE" \
        && echo "rblog backup worktree cloned" \
        || echo "!! rblog backup worktree clone FAILED"
fi

sudo systemctl daemon-reload
sudo sysctl --system >/dev/null 2>&1 && echo "sysctl applied" || echo "!! sysctl apply FAILED"

# enable at boot + restart (restart, since a process may already be running)
for s in rblog rblog-backup.timer shadowsocks-rust tbox_server nginx vlmcsd; do
    sudo systemctl enable "$s" >/dev/null 2>&1
    sudo systemctl restart "$s" && echo "restarted $s" || echo "!! restart $s FAILED"
done

# waiwei + xray: off and stay off. `waiwei` is the old monolithic
# unit name; current waiwei is split into waiwei-web and waiwei-puller.
for s in waiwei waiwei-web waiwei-puller xray; do
    sudo systemctl disable "$s" >/dev/null 2>&1
    sudo systemctl stop "$s" 2>/dev/null
    sudo systemctl reset-failed "$s" 2>/dev/null
    echo "disabled+stopped $s"
done

# acme.sh: auto-upgrade + renew cron + install certs into /etc/nginx/ssl with a
# reload hook so renewals copy to the destination and reload nginx automatically.
ACME=/home/ubuntu/.acme.sh/acme.sh
[ -d /etc/nginx/ssl ] || sudo mkdir -p /etc/nginx/ssl
# acme.sh runs as ubuntu and rewrites these on renewal, so the whole dir must be
# ubuntu-owned (the config push above re-created them root-owned via rsync).
sudo chown -R ubuntu:ubuntu /etc/nginx/ssl
sudo chmod 0755 /etc/nginx/ssl
if [ -x "$ACME" ]; then
    "$ACME" --upgrade --auto-upgrade >/dev/null 2>&1 && echo "acme auto-upgrade on"
    "$ACME" --install-cronjob >/dev/null 2>&1 || true

    ensure_acme_domain() {
        d="$1"
        wildcard="$2"
        conf="/home/ubuntu/.acme.sh/${d}_ecc/${d}.conf"
        need_issue=0
        if ! "$ACME" --info -d "$d" --ecc >/dev/null 2>&1; then
            need_issue=1
        elif [ -n "$wildcard" ] && ! grep -Fq "$wildcard" "$conf" 2>/dev/null; then
            need_issue=1
        fi

        if [ "$need_issue" -eq 1 ]; then
            if grep -q '^SAVED_AWS_ACCESS_KEY_ID=' /home/ubuntu/.acme.sh/account.conf 2>/dev/null \
                && grep -q '^SAVED_AWS_SECRET_ACCESS_KEY=' /home/ubuntu/.acme.sh/account.conf 2>/dev/null; then
                if [ -n "$wildcard" ]; then
                    "$ACME" -f --issue --ocsp --dns dns_aws -d "$d" -d "$wildcard" --keylength ec-256 --server zerossl \
                        && echo "acme cert issued: $d $wildcard" \
                        || echo "!! acme issue $d $wildcard failed"
                else
                    "$ACME" -f --issue --ocsp --dns dns_aws -d "$d" --keylength ec-256 --server zerossl \
                        && echo "acme cert issued: $d" \
                        || echo "!! acme issue $d failed"
                fi
            else
                echo "!! acme dns_aws credentials missing for $d"
            fi
        fi

        if "$ACME" --info -d "$d" --ecc >/dev/null 2>&1; then
            "$ACME" --install-cert -d "$d" --ecc \
                --key-file "/etc/nginx/ssl/$d.key" \
                --cert-file "/etc/nginx/ssl/$d.cer" \
                --ca-file "/etc/nginx/ssl/$d.ca.cer" \
                --fullchain-file "/etc/nginx/ssl/$d.fullchain.cer" \
                --reloadcmd "sudo systemctl reload nginx" >/dev/null 2>&1 \
                && echo "acme cert installed: $d" || echo "!! acme install-cert $d failed"
        fi
    }

    ensure_acme_domain xiedeacc.com "*.xiedeacc.com"
    ensure_acme_domain youkechat.net "*.youkechat.net"
    ensure_acme_domain waiwei.io ""
    "$ACME" --cron >/dev/null 2>&1 || true   # renew any that are due
fi
sudo nginx -t >/dev/null 2>&1 && sudo systemctl reload nginx && echo "nginx reloaded"

# rblog data backup: keep the hourly timer on and run it once now.
sudo systemctl enable rblog-backup.timer >/dev/null 2>&1
sudo systemctl start rblog-backup.service
sleep 5
echo "rblog-backup: $(systemctl show rblog-backup.service -p Result --value 2>/dev/null)"
git -C /usr/local/blog/.backup-worktree log --oneline -1 2>/dev/null | sed 's/^/  latest backup: /'

echo "--- final states ---"
for s in rblog rblog-backup.timer shadowsocks-rust tbox_server nginx vlmcsd waiwei waiwei-web waiwei-puller xray; do
    printf "  %s: " "$s"; systemctl is-enabled "$s" 2>/dev/null | tr -d '\n'; printf " / "; systemctl is-active "$s" 2>/dev/null | tr -d '\n'; echo
done
# `systemctl is-active` returns non-zero for the intentionally-stopped services,
# which would otherwise make this block's exit code look like a failure.
exit 0
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
