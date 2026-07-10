$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected NAS tree back to the Ubuntu host, then
# applies packages, services, runtimes, quotas, database restore, and cron.
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$opts    = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-o','ConnectTimeout=20')
$sshArgs = @() + $opts
$scpArgs = @('-r','-p') + $opts
if (-not [string]::IsNullOrEmpty($env:AS_PORT)) { $sshArgs += @('-p', $env:AS_PORT); $scpArgs += @('-P', $env:AS_PORT) }
if (-not [string]::IsNullOrEmpty($env:AS_KEY))  { $sshArgs += @('-i', $env:AS_KEY);  $scpArgs += @('-i', $env:AS_KEY) }

$errCount = 0
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })

function Reset-TestKnownHost {
    if ([string]::IsNullOrWhiteSpace($env:AS_HOSTNAME)) { return }
    $sshKeygen = Get-Command ssh-keygen -ErrorAction SilentlyContinue
    if (-not $sshKeygen) { return }
    $ports = New-Object System.Collections.Generic.List[string]
    if (-not [string]::IsNullOrWhiteSpace($env:AS_PORT)) { $ports.Add($env:AS_PORT) }
    if (-not $ports.Contains('22')) { $ports.Add('22') }
    & $sshKeygen.Source -R $env:AS_HOSTNAME *> $null
    foreach ($p in $ports) {
        & $sshKeygen.Source -R ("[{0}]:{1}" -f $env:AS_HOSTNAME, $p) *> $null
    }
}

function Test-RootSshReady {
    & $ssh @sshArgs $dest 'true' 2>$null
    return $LASTEXITCODE -eq 0
}

function Get-PythonExe {
    $candidates = @()
    $python = (Get-Command python -ErrorAction SilentlyContinue | Select-Object -First 1)
    if ($python) { $candidates += $python.Source }
    $py = (Get-Command py -ErrorAction SilentlyContinue | Select-Object -First 1)
    if ($py) { $candidates += $py.Source }
    foreach ($candidate in $candidates) {
        if ([string]::IsNullOrWhiteSpace($candidate)) { continue }
        return $candidate
    }
    return ''
}

function Ensure-TestRootSsh {
    if (Test-RootSshReady) { return }

    $bootstrapUser = $env:AS_BOOTSTRAP_USER
    if ([string]::IsNullOrWhiteSpace($bootstrapUser)) { $bootstrapUser = 'dev' }
    $bootstrapPassword = $env:AS_BOOTSTRAP_PASSWORD
    if ([string]::IsNullOrWhiteSpace($bootstrapPassword)) { $bootstrapPassword = 'qh6288QHW' }
    if ([string]::IsNullOrWhiteSpace($bootstrapUser) -or [string]::IsNullOrWhiteSpace($bootstrapPassword)) {
        throw "root SSH is not ready and no bootstrap SSH password is configured"
    }

    Write-Host "root SSH not ready; bootstrapping root SSH over normal SSH as $bootstrapUser"
    $pubKey = ''
    if (-not [string]::IsNullOrWhiteSpace($env:AS_KEY) -and (Test-Path -LiteralPath ($env:AS_KEY + '.pub'))) {
        $pubKey = Get-Content -LiteralPath ($env:AS_KEY + '.pub') -Raw
    } elseif (Test-Path -LiteralPath "$env:USERPROFILE\.ssh\id_ed25519.pub") {
        $pubKey = Get-Content -LiteralPath "$env:USERPROFILE\.ssh\id_ed25519.pub" -Raw
    }
    if ([string]::IsNullOrWhiteSpace($pubKey)) { throw "cannot bootstrap root SSH: public key not found" }

    $tmpDir = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_test_bootstrap_" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
    try {
        $python = Get-PythonExe
        if ([string]::IsNullOrWhiteSpace($python)) { throw "cannot bootstrap root SSH: Python is required for password SSH bootstrap" }

        $paramikoRoot = Join-Path ([IO.Path]::GetTempPath()) 'auto_sync_paramiko'
        $paramikoCheck = @"
import sys
sys.path.insert(0, r'$paramikoRoot')
import paramiko
"@
        $paramikoCheckPath = Join-Path $tmpDir 'check_paramiko.py'
        Set-Content -LiteralPath $paramikoCheckPath -Value $paramikoCheck -NoNewline -Encoding utf8
        & $python $paramikoCheckPath 2>$null
        if ($LASTEXITCODE -ne 0) {
            Write-Host "installing local Paramiko for password SSH bootstrap"
            New-Item -ItemType Directory -Force -Path $paramikoRoot | Out-Null
            & $python -m pip install --quiet --target $paramikoRoot paramiko
            if ($LASTEXITCODE -ne 0) { throw "pip install paramiko failed" }
        }

        $pub = Join-Path $tmpDir 'id_ed25519.pub'
        $sshd = Join-Path $tmpDir 'sshd_config'
        $helper = Join-Path $tmpDir 'bootstrap_root_ssh.py'
        Set-Content -LiteralPath $pub -Value $pubKey -NoNewline -Encoding ascii
        $localSshd = Get-LocalCollectedPath '/etc/ssh/sshd_config'
        if (Test-Path -LiteralPath $localSshd) {
            Copy-Item -LiteralPath $localSshd -Destination $sshd -Force
        } else {
            Set-Content -LiteralPath $sshd -Value "Port $($env:AS_PORT)`nPubkeyAuthentication yes`nPasswordAuthentication no`nPermitRootLogin yes`n" -NoNewline -Encoding ascii
        }
        @"
import pathlib
import sys

sys.path.insert(0, r'$paramikoRoot')
import paramiko

host = r'$($env:AS_HOSTNAME)'
target_port = int(r'$($env:AS_PORT)' or '22')
port_candidates = []
for text in (r'$($env:AS_PORT)', '22'):
    if not text:
        continue
    port = int(text)
    if port not in port_candidates:
        port_candidates.append(port)
user = r'$bootstrapUser'
password = r'$bootstrapPassword'
pub_path = pathlib.Path(r'$pub')
sshd_path = pathlib.Path(r'$sshd')

client = paramiko.SSHClient()
client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
last_error = None
for port in port_candidates:
    try:
        client.connect(hostname=host, port=port, username=user, password=password, look_for_keys=False, allow_agent=False, timeout=20, auth_timeout=20)
        break
    except Exception as exc:
        last_error = exc
else:
    raise SystemExit(f"password SSH bootstrap failed on ports {port_candidates}: {last_error}")
sftp = client.open_sftp()
sftp.put(str(pub_path), '/tmp/auto_sync_root_key.pub')
sftp.put(str(sshd_path), '/tmp/auto_sync_sshd_config')

script = f'''
set -e
cp /tmp/auto_sync_sshd_config /etc/ssh/sshd_config
sed -i -E 's/^([[:space:]]*)(Port|PermitRootLogin|PubkeyAuthentication|PasswordAuthentication)([[:space:]].*)/# auto_sync test disabled: \1\2\3/' /etc/ssh/sshd_config
grep -Eq '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config || printf '\nInclude /etc/ssh/sshd_config.d/*.conf\n' >> /etc/ssh/sshd_config
mkdir -p /etc/ssh/sshd_config.d
cat > /etc/ssh/sshd_config.d/99-auto-sync-test.conf <<'SSHD'
Port {target_port}
PubkeyAuthentication yes
PasswordAuthentication yes
PermitRootLogin yes
SSHD
mkdir -p /root/.ssh
cat /tmp/auto_sync_root_key.pub > /root/.ssh/authorized_keys
chown root:root /root /root/.ssh /root/.ssh/authorized_keys
chmod 700 /root
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys
systemctl disable --now ssh.socket 2>/dev/null || true
systemctl restart ssh.service 2>/dev/null || systemctl restart sshd.service
'''
with sftp.file('/tmp/auto_sync_bootstrap_root_ssh.sh', 'w') as f:
    f.write(script)
sftp.chmod('/tmp/auto_sync_bootstrap_root_ssh.sh', 0o700)
sftp.close()

stdin, stdout, stderr = client.exec_command("sudo -S bash /tmp/auto_sync_bootstrap_root_ssh.sh", get_pty=True)
stdin.write(password + "\n")
stdin.flush()
rc = stdout.channel.recv_exit_status()
out = stdout.read().decode(errors='replace')
err = stderr.read().decode(errors='replace')
client.close()
if out:
    print(out, end='')
if err:
    print(err, end='', file=sys.stderr)
if rc != 0:
    raise SystemExit(rc)
"@ | Set-Content -LiteralPath $helper -NoNewline -Encoding utf8

        & $python $helper
        if ($LASTEXITCODE -ne 0) { throw "normal SSH bootstrap failed; target may have disabled password login for $bootstrapUser on ports $($env:AS_PORT),22" }
    } finally {
        Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    Start-Sleep -Seconds 2
    if (-not (Test-RootSshReady)) { throw "root SSH bootstrap finished but root key login still failed" }
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
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Script))
    $b64 | & $ssh @sshArgs $dest 'tmp=$(mktemp /tmp/auto_sync_remote.XXXXXX); base64 -d > "$tmp" && bash "$tmp" </dev/null; rc=$?; rm -f "$tmp"; exit "$rc"'
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

Reset-TestKnownHost
Ensure-TestRootSsh
Invoke-Remote 'rm -rf ~/.auto_sync_stage; mkdir -p ~/.auto_sync_stage'
Transfer-CollectedPathsToStage $collectPaths @('/root/auto_sync_db_dumps', '/tmp/auto_sync_db_dumps') '~/.auto_sync_stage'

Invoke-Remote 'rsync -rltD --no-perms ~/.auto_sync_stage/ /; rc=$?; if [ -f /tmp/auto_sync_root_key.pub ]; then mkdir -p /root/.ssh; touch /root/.ssh/authorized_keys; grep -qxF -f /tmp/auto_sync_root_key.pub /root/.ssh/authorized_keys 2>/dev/null || cat /tmp/auto_sync_root_key.pub >> /root/.ssh/authorized_keys; fi; chmod 755 / /etc /usr /usr/bin 2>/dev/null || true; chmod 700 /root/.ssh 2>/dev/null || true; chmod 600 /root/.ssh/authorized_keys 2>/dev/null || true; rm -rf ~/.auto_sync_stage; [ "$rc" -eq 0 ] && echo "installed collected paths"; exit "$rc"'

$testSshPort = $env:AS_PORT
if ([string]::IsNullOrWhiteSpace($testSshPort)) { $testSshPort = '22' }
Invoke-Remote @"
set -e
sed -i -E 's/^([[:space:]]*)(Port|PermitRootLogin|PubkeyAuthentication|PasswordAuthentication)([[:space:]].*)/# auto_sync test disabled: \1\2\3/' /etc/ssh/sshd_config
grep -Eq '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config || printf '\nInclude /etc/ssh/sshd_config.d/*.conf\n' >> /etc/ssh/sshd_config
mkdir -p /etc/ssh/sshd_config.d /root/.ssh
cat > /etc/ssh/sshd_config.d/99-auto-sync-test.conf <<'SSHD'
Port $testSshPort
PubkeyAuthentication yes
PasswordAuthentication yes
PermitRootLogin yes
SSHD
if [ -f /tmp/auto_sync_root_key.pub ]; then
    cat /tmp/auto_sync_root_key.pub > /root/.ssh/authorized_keys
fi
chmod 700 /root /root/.ssh
chmod 600 /root/.ssh/authorized_keys 2>/dev/null || true
systemctl disable --now ssh.socket 2>/dev/null || true
systemctl restart ssh.service 2>/dev/null || systemctl restart sshd.service
"@

function Quote-ShellArg([string]$Value) {
    return "'" + $Value.Replace("'", "'""'""'") + "'"
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
            if (-not ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/'))) { continue }
            try { $target = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($targetB64)) } catch { continue }
            $qPath = Quote-ShellArg $path
            $qTarget = Quote-ShellArg $target
            $links.Add("mkdir -p -- `$(dirname -- $qPath) 2>/dev/null || true; rm -rf -- $qPath 2>/dev/null || true; ln -s -- $qTarget $qPath 2>/dev/null || true")
            continue
        }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        if ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/')) {
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

log() { printf '[test-deploy] %s\n' "$*"; }
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
restart_if_exists() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    if unit_exists "$unit"; then
        systemctl enable "$unit" >/dev/null 2>&1 || true
        systemctl restart "$unit" && log "restarted $unit" || log "WARN: restart $unit failed"
    fi
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

if [ -f /etc/apt/sources.list.d/gitlab_gitlab-ce.list ] && grep -q '/ubuntu/resolute\| resolute ' /etc/apt/sources.list.d/gitlab_gitlab-ce.list; then
    log "GitLab CE has no resolute repo yet; use noble package repo"
    sed -i 's#/ubuntu/resolute#/ubuntu/noble#g; s/ resolute / noble /g' /etc/apt/sources.list.d/gitlab_gitlab-ce.list
fi

if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
    sed -i 's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' /etc/apt/sources.list.d/ubuntu.sources
fi
apt-get update
apt-get purge -y vim || true
apt-get autoremove -y || true
apt-get install -y \
    dialog zfsutils-linux openssh-server zsh net-tools curl iputils-ping iftop cron ca-certificates xfonts-utils dnsutils hdparm \
    autoconf libtool libtool-bin cmake make gcc g++ texinfo bison flex gdb build-essential automake pkg-config help2man gettext \
    python3-pip ruby luajit netcat-openbsd gperf cscope exuberant-ctags vim-nox git git-lfs lcov graphviz \
    libgeoip-dev libxml2-dev libxslt1.1 libxslt1-dev libatomic-ops-dev libgd-dev libperl-dev libluajit-5.1-dev tcl-dev ruby-dev libncurses-dev \
    mysql-server postgresql postgresql-client libpq-dev postgresql-contrib sysstat nginx-full \
    ffmpeg libimage-exiftool-perl postgresql-server-dev-all redis quota
apt-get remove -y apport || true
apt-get autoremove -y || true

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
ensure_host_entry 192.168.2.126 dev.xiedeacc.com
ensure_host_entry 192.168.2.126 coverage.xiedeacc.com

swapoff -a || true
sed -i '/^\/swap\.img[[:space:]]/s/^/#/' /etc/fstab 2>/dev/null || true
rm -f /swap.img

if [ ! -d /root/.oh-my-zsh ]; then
    RUNZSH=no CHSH=no KEEP_ZSHRC=yes sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" || log "WARN: oh-my-zsh install failed"
fi

if [ ! -x /usr/local/go/go1.25.1/bin/go ]; then
    mkdir -p /usr/local/go
    cd /usr/local/go
    rm -rf go go1.25.1 go1.25.1.linux-amd64.tar.gz
    if curl --retry 5 --retry-all-errors -L -O https://go.dev/dl/go1.25.1.linux-amd64.tar.gz || curl --retry 5 --retry-all-errors -L -O https://dl.google.com/go/go1.25.1.linux-amd64.tar.gz; then
        tar zxf go1.25.1.linux-amd64.tar.gz
        mv go go1.25.1
        rm -f go1.25.1.linux-amd64.tar.gz
    else
        log "WARN: go download failed"
    fi
fi
export PATH="/usr/local/go/go1.25.1/bin:/root/go/bin:$PATH"
if command -v go >/dev/null 2>&1; then
    go install github.com/google/pprof@latest || log "WARN: go install pprof failed"
fi

if [ ! -x /root/.cargo/bin/rustup ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y || log "WARN: rustup install failed"
fi

mkdir -p /opt/software/src/tools
if [ -L /opt/software/src/tools/nvm ]; then
    rm -f /opt/software/src/tools/nvm
fi
export NVM_DIR=/opt/software/src/tools/nvm
if [ ! -s "$NVM_DIR/nvm.sh" ]; then
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash || log "WARN: nvm install failed"
fi
if [ -s "$NVM_DIR/nvm.sh" ]; then
    . "$NVM_DIR/nvm.sh"
    nvm install 24.18.0 || nvm install 24 || log "WARN: nvm install 24 failed"
    nvm use 24.18.0 || nvm use 24 || true
fi
if [ -d "$NVM_DIR" ]; then
    chmod -R a+rX "$NVM_DIR" || true
fi

if [ ! -x /usr/local/bin/buildifier ]; then
    curl -L -o /usr/local/bin/buildifier https://github.com/bazelbuild/buildtools/releases/download/v7.1.2/buildifier-linux-amd64
    chmod +x /usr/local/bin/buildifier
fi

if [ ! -d /usr/local/java/jdk/jdk-21.0.3 ]; then
    mkdir -p /usr/local/java/jdk
    cd /usr/local/java/jdk
    rm -f jdk-21.0.3_linux-x64_bin.tar.gz
    curl -L -O https://download.oracle.com/java/21/archive/jdk-21.0.3_linux-x64_bin.tar.gz
    tar zxf jdk-21.0.3_linux-x64_bin.tar.gz
    rm -f jdk-21.0.3_linux-x64_bin.tar.gz
fi

if [ -f /root/src/share/ubuntu/conf/.vimrc ]; then
    cp /root/src/share/ubuntu/conf/.vimrc /root/.vimrc
fi
if [ ! -d /root/.vim/bundle/Vundle.vim ]; then
    git clone https://github.com/VundleVim/Vundle.vim.git /root/.vim/bundle/Vundle.vim || true
fi
if command -v vim >/dev/null 2>&1; then
    timeout --kill-after=10s 180s vim +PluginInstall +qall </dev/null || log "WARN: vim PluginInstall failed"
fi
if [ ! -d /root/.vim/bundle/vim-airline-fonts ]; then
    git clone https://github.com/powerline/fonts /root/.vim/bundle/vim-airline-fonts || true
fi
if [ -x /root/.vim/bundle/vim-airline-fonts/install.sh ]; then
    (cd /root/.vim/bundle/vim-airline-fonts && ./install.sh) || true
fi
if [ -d /root/.vim/bundle/YouCompleteMe ]; then
    (cd /root/.vim/bundle/YouCompleteMe && git submodule update --init --recursive && ./install.py --all) || log "WARN: YouCompleteMe install failed"
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

mkdir -p /root/src/software
if [ ! -d /root/src/software/pgvector ]; then
    git clone https://github.com/pgvector/pgvector.git /root/src/software/pgvector || true
fi
if [ -d /root/src/software/pgvector ]; then
    (cd /root/src/software/pgvector && make && make install) || log "WARN: pgvector build/install failed"
fi

deploy_immich_from_git() {
    mkdir -p /opt/software/src
    repo=/opt/software/src/immich
    if [ -e "$repo" ] && [ ! -d "$repo/.git" ]; then
        mv "$repo" "${repo}.before-git-$(date +%Y%m%d%H%M%S)" || return 1
    fi
    if [ ! -d "$repo/.git" ]; then
        git clone --branch deploy git@github.com:xiedeacc/immich.git "$repo" || return 1
    fi
    (
        cd "$repo" &&
        git remote set-url origin git@github.com:xiedeacc/immich.git &&
        git fetch origin deploy &&
        git checkout deploy &&
        git pull --ff-only origin deploy
    ) || return 1
    for script in deploy.sh scripts/deploy.sh install.sh; do
        if [ -f "$repo/$script" ]; then
            chmod +x "$repo/$script" 2>/dev/null || true
            (cd "$repo" && bash "$script") || return 1
            return 0
        fi
    done
    log "WARN: immich deploy script not found in $repo"
    return 1
}
deploy_immich_from_git || log "WARN: immich deploy from git failed"

setquota -u root 20000000 20000000 0 0 /dev/mmcblk0p2 2>/dev/null || true
for u in tiger git gitlab immich; do
    id "$u" >/dev/null 2>&1 && setquota -u "$u" 1000000 1000000 0 0 / 2>/dev/null || true
done

ensure_zfs_for_gitlab() {
    log "test VM: use plain /zfs directory for GitLab data"
    mkdir -p /zfs /zfs/gitlab_data /zfs/gitlab_data/lfs-objects
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
}

ensure_gitlab_repo_compatible() {
    list="/etc/apt/sources.list.d/gitlab_gitlab-ce.list"
    [ -f "$list" ] || return 0
    if grep -q '/ubuntu/resolute\| resolute ' "$list"; then
        log "GitLab CE has no resolute repo yet; use noble package repo"
        sed -i 's#/ubuntu/resolute#/ubuntu/noble#g; s/ resolute / noble /g' "$list"
    fi
}

ensure_zfs_for_gitlab
installed_gitlab_version="$(dpkg-query -W -f='${Version}' gitlab-ce 2>/dev/null || true)"
if [ "$installed_gitlab_version" != "$GITLAB_CE_VERSION" ]; then
    curl -s https://packages.gitlab.com/install/repositories/gitlab/gitlab-ce/script.deb.sh | bash || log "WARN: gitlab repo install failed"
    ensure_gitlab_repo_compatible
    apt-get update || true
    apt-get install -y --allow-downgrades "gitlab-ce=$GITLAB_CE_VERSION" || log "WARN: gitlab-ce $GITLAB_CE_VERSION install failed"
fi
ensure_zfs_for_gitlab
mkdir -p /zfs/gitlab_data /zfs/gitlab_data/lfs-objects 2>/dev/null || true
passwd -S git 2>/dev/null || true
passwd -u git 2>/dev/null || true
[ -d /zfs/gitlab_data ] && chown -R git:git /zfs/gitlab_data 2>/dev/null || true
[ -d /zfs/gitlab_data ] && chmod 2770 /zfs/gitlab_data 2>/dev/null || true
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
    case "$kind" in
        mysql)
            find "$@" -type f \( \
                -name 'mysql_full_backup_*.sql' -o \
                -name 'mysql_full_backup_*.sql.gz' -o \
                -name 'mysql-all.sql' -o \
                -name 'mysql-all.sql.gz' -o \
                -name '*mysql*.sql' -o \
                -name '*mysql*.sql.gz' \
            \) -print0 2>/dev/null | score_dumps | sort -nr | head -1 | cut -f2-
            ;;
        postgres)
            find "$@" -type f \( \
                -name 'pg_full_backup_*.sql' -o \
                -name 'pg_full_backup_*.sql.gz' -o \
                -name 'postgres-all.sql' -o \
                -name 'postgres-all.sql.gz' -o \
                -name '*postgres*.sql' -o \
                -name '*postgres*.sql.gz' -o \
                -name '*pg*.sql' -o \
                -name '*pg*.sql.gz' \
            \) -print0 2>/dev/null | score_dumps | sort -nr | head -1 | cut -f2-
            ;;
        *)
            return 1
            ;;
    esac
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
            [ -d "/etc/postgresql/$old_version" ] && mv "/etc/postgresql/$old_version" "/etc/postgresql/${old_version}.disabled-by-auto-sync" 2>/dev/null || true
        done
    fi
    systemctl restart postgresql 2>/dev/null || true
    if command -v pg_lsclusters >/dev/null 2>&1; then
        if ! pg_lsclusters 2>/dev/null | awk '$4 == "online" { found = 1 } END { exit found ? 0 : 1 }'; then
            version="$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -V | tail -1)"
            if [ -n "$version" ]; then
                [ -d "/etc/postgresql/$version/main" ] || pg_createcluster "$version" main --start || true
                pg_ctlcluster "$version" main start 2>/dev/null || true
            fi
        fi
        if ! pg_lsclusters 2>/dev/null | awk '$3 == "5432" && $4 == "online" { found = 1 } END { exit found ? 0 : 1 }'; then
            version="$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -V | tail -1)"
            if [ -n "$version" ] && [ -f "/etc/postgresql/$version/main/postgresql.conf" ]; then
                pg_ctlcluster "$version" main stop 2>/dev/null || true
                sed -i -E "s/^#?[[:space:]]*port[[:space:]]*=.*/port = 5432/" "/etc/postgresql/$version/main/postgresql.conf"
                pg_ctlcluster "$version" main start 2>/dev/null || true
            fi
        fi
    fi
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
        grep -Eq '^[[:space:]]*gitlab[[:space:]]+tiger[[:space:]]+immich([[:space:]]|$)' "$ident" || printf '\ngitlab  tiger  immich\n' >> "$ident"
    done
    systemctl reload postgresql 2>/dev/null || true
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
rm -rf /root/auto_sync_db_dumps /tmp/auto_sync_db_dumps 2>/dev/null || true
if [ "$zfs_woken" = "1" ]; then
    standby_zfs
fi

if [ -d /usr/local/auto_sync ] && [ ! -e /opt/auto_sync ]; then
    mkdir -p /opt
    ln -s /usr/local/auto_sync /opt/auto_sync
fi

id tiger >/dev/null 2>&1 || useradd -m -s /bin/bash tiger
mkdir -p /usr/local/auto_sync/logs /usr/local/blog/logs /usr/local/tbox/log /usr/local/waiwei/logs /usr/local/xray/logs /opt/immich/server /opt/immich/upload /opt/immich/machine-learning /opt/immich/conf
for d in backups encoded-video library profile thumbs upload; do
    mkdir -p "/opt/immich/upload/$d"
    touch "/opt/immich/upload/$d/.immich"
done
for d in /usr/local/auto_sync /usr/local/halo /usr/local/tbox /opt/immich; do
    [ -e "$d" ] && chown -R tiger:tiger "$d" 2>/dev/null || true
done
find /opt/immich/server/bin /opt/immich/machine-learning/.venv/bin -type f -exec chmod a+rx {} + 2>/dev/null || true
chmod a+rx /opt/software/src/tools/nvm/versions/node/v24.18.0/bin/node 2>/dev/null || true
systemctl daemon-reload
rm -f /etc/nginx/sites-enabled/default /etc/nginx/conf.d/default.conf 2>/dev/null || true
for s in auto_sync halo2 immich immich-ml tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server waiwei-web waiwei-puller xray rblog rblog-backup.timer gitlab-runsvdir gitlab; do
    restart_if_exists "$s"
done

(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_pg.sh'; echo '0 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_pg.sh > /dev/null 2>&1') | crontab -
(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_mysql.sh'; echo '5 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_mysql.sh > /dev/null 2>&1') | crontab -

echo '--- final states ---'
for s in auto_sync halo2 immich immich-ml tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server waiwei-web waiwei-puller xray rblog rblog-backup.timer gitlab-runsvdir gitlab; do
    resolved="$(unit_name "$s" 2>/dev/null || true)"
    if [ -n "$resolved" ]; then
        printf '  %s: ' "$s"; systemctl is-enabled "$resolved" 2>/dev/null | tr -d '\n'; printf ' / '; systemctl is-active "$resolved" 2>/dev/null | tr -d '\n'; echo
    elif [ "$s" = "gitlab" ] && command -v gitlab-ctl >/dev/null 2>&1 && gitlab-ctl status >/dev/null 2>&1; then
        printf '  %s: enabled / active\n' "$s"
    else
        printf '  %s: not-found / inactive\n' "$s"
    fi
done
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
