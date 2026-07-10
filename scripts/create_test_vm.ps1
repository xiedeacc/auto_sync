param(
    [string]$VmDir = 'D:\test_vmware',
    [string]$MediaDir = 'D:\auto_sync_test_vm_media',
    [string]$InstallerIso = '',
    [string]$SeedIso = '',
    [string]$VmName = 'auto_sync_test',
    [string]$GuestIp = '192.168.255.131',
    [string]$Gateway = '192.168.255.2',
    [string]$Netmask = '24',
    [string]$MacAddress = '00:50:56:25:51:31',
    [int]$MemoryMb = 16384,
    [int]$Vcpus = 8,
    [int]$CoresPerSocket = 8,
    [string]$DiskSize = '60GB',
    [string]$DevUser = 'dev',
    [string]$DevPasswordHash = '$6$autoSyncTestVm01$UEaEZIM5/CM.kzRch28cCmWmwLdUQjfgSprEfSuIlRqUpsnDNeCRugUjLFOsL/irIbH2XCPuHXf3DQoV7ZcDs/',
    [switch]$NoStart,
    [switch]$KeepExisting
)

$ErrorActionPreference = 'Stop'

function Resolve-Tool([string]$Name, [string[]]$Candidates) {
    foreach ($candidate in $Candidates) {
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "Required VMware tool not found: $Name"
}

function Stop-ExistingVm([string]$VmxPath, [string]$VmrunPath) {
    if (-not (Test-Path -LiteralPath $VmxPath)) { return }
    try {
        & $VmrunPath stop $VmxPath hard *> $null
    } catch {
        # The VM may already be off or vmrun may not know this VM yet.
    }
    Start-Sleep -Seconds 3

    $escaped = [regex]::Escape($VmxPath)
    Get-CimInstance Win32_Process -Filter "name = 'vmware-vmx.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match $escaped } |
        ForEach-Object {
            try {
                Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop
            } catch {
                Write-Warning "Failed to stop vmware-vmx.exe pid=$($_.ProcessId): $($_.Exception.Message)"
            }
        }
}

function Remove-TestVmDir([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $resolved = (Resolve-Path -LiteralPath $Path).Path.TrimEnd('\')
    if ($resolved -ne 'D:\test_vmware') {
        throw "Refusing to delete unexpected VM directory: $resolved"
    }
    $vmArtifacts = @(
        '*.vmdk',
        '*.vmem',
        '*.vmss',
        '*.vmsd',
        '*.vmxf',
        '*.vmx',
        '*.scoreboard',
        '*.nvram',
        'nvram',
        '*.lck'
    )
    foreach ($pattern in $vmArtifacts) {
        Get-ChildItem -LiteralPath $resolved -Force -Filter $pattern -ErrorAction SilentlyContinue | ForEach-Object {
            for ($i = 1; $i -le 8; $i++) {
                try {
                    Remove-Item -LiteralPath $_.FullName -Recurse -Force -ErrorAction Stop
                    break
                } catch {
                    if ($i -eq 8) { throw }
                    Start-Sleep -Seconds 2
                }
            }
        }
    }
}

function Write-CloudInitSeedFiles(
    [string]$Dir,
    [string]$Ip,
    [string]$Mask,
    [string]$RouteGateway,
    [string]$Mac,
    [string]$User,
    [string]$PasswordHash
) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $userData = @"
#cloud-config
autoinstall:
  version: 1
  refresh-installer:
    update: false
  locale: en_US.UTF-8
  keyboard:
    layout: us
  identity:
    hostname: test
    username: $User
    password: '$PasswordHash'
  ssh:
    install-server: true
    allow-pw: true
  network:
    version: 2
    ethernets:
      vmnet0:
        match:
          macaddress: '$Mac'
        set-name: ens33
        dhcp4: false
        addresses:
          - $Ip/$Mask
        routes:
          - to: default
            via: $RouteGateway
        nameservers:
          addresses:
            - $RouteGateway
            - 8.8.8.8
  storage:
    layout:
      name: direct
  late-commands:
    - curtin in-target --target=/target -- bash -lc "mkdir -p /etc/ssh/sshd_config.d && printf '%s\n' 'Port 10022' 'PasswordAuthentication yes' 'PubkeyAuthentication yes' 'PermitRootLogin prohibit-password' > /etc/ssh/sshd_config.d/99-auto-sync-test.conf"
    - curtin in-target --target=/target -- systemctl enable ssh.service
    - curtin in-target --target=/target -- systemctl disable ssh.socket || true
  shutdown: reboot
"@
    $metaData = @"
instance-id: auto-sync-test
local-hostname: test
"@
    [IO.File]::WriteAllText((Join-Path $Dir 'user-data'), $userData, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $Dir 'meta-data'), $metaData, [Text.UTF8Encoding]::new($false))
}

function Get-PythonExe {
    $python = Get-Command python -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($python) { return $python.Source }
    $py = Get-Command py -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($py) { return $py.Source }
    throw 'Python is required to build the cloud-init seed ISO'
}

function Ensure-PythonModule([string]$Python, [string]$Module, [string]$Target) {
    $check = "import sys; sys.path.insert(0, r'$Target'); import $Module"
    & $Python -c $check 2>$null
    if ($LASTEXITCODE -eq 0) { return }

    New-Item -ItemType Directory -Force -Path $Target | Out-Null
    & $Python -m pip install --quiet --index-url https://pypi.tuna.tsinghua.edu.cn/simple --target $Target $Module
    if ($LASTEXITCODE -ne 0) { throw "Failed to install Python module $Module" }
}

function New-CloudInitSeedIso([string]$Dir, [string]$OutputIso) {
    $python = Get-PythonExe
    $moduleDir = Join-Path ([IO.Path]::GetTempPath()) 'auto_sync_pycdlib'
    Ensure-PythonModule $python 'pycdlib' $moduleDir

    $builder = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_seed_" + [guid]::NewGuid().ToString('N') + '.py')
    try {
        $script = @"
import pathlib
import sys

sys.path.insert(0, r'$moduleDir')
import pycdlib

root = pathlib.Path(r'$Dir')
out = pathlib.Path(r'$OutputIso')
tmp = out.with_suffix(out.suffix + '.tmp')
if tmp.exists():
    tmp.unlink()
iso = pycdlib.PyCdlib()
iso.new(interchange_level=4, vol_ident='CIDATA', joliet=3, rock_ridge='1.09')
iso.add_file(str(root / 'user-data'), '/USER-DATA.;1', joliet_path='/user-data', rr_name='user-data')
iso.add_file(str(root / 'meta-data'), '/META-DATA.;1', joliet_path='/meta-data', rr_name='meta-data')
iso.write(str(tmp))
iso.close()
tmp.replace(out)
"@
        [IO.File]::WriteAllText($builder, $script, [Text.UTF8Encoding]::new($false))
        & $python $builder
        if ($LASTEXITCODE -ne 0) { throw "Failed to build seed ISO $OutputIso" }
    } finally {
        Remove-Item -LiteralPath $builder -Force -ErrorAction SilentlyContinue
    }
}

function Get-InstallerIso([string]$MediaRoot, [string]$RequestedIso) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedIso)) {
        if (-not (Test-Path -LiteralPath $RequestedIso)) { throw "Installer ISO not found: $RequestedIso" }
        return (Resolve-Path -LiteralPath $RequestedIso).Path
    }
    $prepared = Join-Path $MediaRoot 'ubuntu-26.04-autoinstall.iso'
    if (Test-Path -LiteralPath $prepared) { return (Resolve-Path -LiteralPath $prepared).Path }
    $download = Join-Path $env:USERPROFILE 'Downloads\ubuntu-26.04-live-server-amd64.iso'
    if (Test-Path -LiteralPath $download) { return (Resolve-Path -LiteralPath $download).Path }
    throw "Installer ISO not found. Expected $prepared or $download"
}

function Get-SeedIso([string]$MediaRoot, [string]$RequestedSeed) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedSeed)) {
        if (-not (Test-Path -LiteralPath $RequestedSeed)) { throw "Seed ISO not found: $RequestedSeed" }
        return (Resolve-Path -LiteralPath $RequestedSeed).Path
    }
    $seed = Join-Path $MediaRoot 'seed.iso'
    New-CloudInitSeedIso $MediaRoot $seed
    return (Resolve-Path -LiteralPath $seed).Path
}

function Write-Vmx(
    [string]$Path,
    [string]$Name,
    [string]$Iso,
    [string]$Seed,
    [string]$Mac,
    [int]$Mem,
    [int]$Cpu,
    [int]$Cores
) {
    $text = @"
.encoding = "UTF-8"
config.version = "8"
virtualHW.version = "21"
displayName = "$Name"
guestOS = "ubuntu-64"
memsize = "$Mem"
numvcpus = "$Cpu"
cpuid.coresPerSocket = "$Cores"
firmware = "bios"
bios.bootOrder = "cdrom,hdd"
pciBridge0.present = "TRUE"
pciBridge4.present = "TRUE"
pciBridge4.virtualDev = "pcieRootPort"
pciBridge4.functions = "8"
pciBridge5.present = "TRUE"
pciBridge5.virtualDev = "pcieRootPort"
pciBridge5.functions = "8"
pciBridge6.present = "TRUE"
pciBridge6.virtualDev = "pcieRootPort"
pciBridge6.functions = "8"
pciBridge7.present = "TRUE"
pciBridge7.virtualDev = "pcieRootPort"
pciBridge7.functions = "8"
scsi0.present = "TRUE"
scsi0.virtualDev = "lsilogic"
scsi0:0.present = "TRUE"
scsi0:0.fileName = "test.vmdk"
sata0.present = "TRUE"
sata0:0.present = "TRUE"
sata0:0.fileName = "$Iso"
sata0:0.deviceType = "cdrom-image"
sata0:0.startConnected = "TRUE"
sata0:0.autodetect = "FALSE"
sata0:1.present = "TRUE"
sata0:1.fileName = "$Seed"
sata0:1.deviceType = "cdrom-image"
sata0:1.startConnected = "TRUE"
sata0:1.autodetect = "FALSE"
ethernet0.present = "TRUE"
ethernet0.connectionType = "nat"
ethernet0.addressType = "static"
ethernet0.address = "$Mac"
ethernet0.virtualDev = "e1000e"
usb.present = "TRUE"
sound.present = "FALSE"
extendedConfigFile = "$Name.vmxf"
virtualHW.productCompatibility = "hosted"
vmxstats.filename = "$Name.scoreboard"
"@
    [IO.File]::WriteAllText($Path, $text, [Text.UTF8Encoding]::new($false))
}

$vmrun = Resolve-Tool 'vmrun.exe' @('C:\Program Files (x86)\VMware\VMware Workstation\vmrun.exe')
$vdisk = Resolve-Tool 'vmware-vdiskmanager.exe' @('C:\Program Files (x86)\VMware\VMware Workstation\vmware-vdiskmanager.exe')
$vmx = Join-Path $VmDir "$VmName.vmx"

if (-not $KeepExisting) {
    Stop-ExistingVm $vmx $vmrun
    Remove-TestVmDir $VmDir
}

Write-CloudInitSeedFiles $MediaDir $GuestIp $Netmask $Gateway $MacAddress $DevUser $DevPasswordHash
$installer = Get-InstallerIso $MediaDir $InstallerIso
$seed = Get-SeedIso $MediaDir $SeedIso

New-Item -ItemType Directory -Force -Path $VmDir | Out-Null
& $vdisk -c -s $DiskSize -a lsilogic -t 0 (Join-Path $VmDir 'test.vmdk')
if ($LASTEXITCODE -ne 0) { throw "vmware-vdiskmanager failed with exit code $LASTEXITCODE" }

Write-Vmx $vmx $VmName $installer $seed $MacAddress $MemoryMb $Vcpus $CoresPerSocket

if (-not $NoStart) {
    & $vmrun start $vmx nogui
    if ($LASTEXITCODE -ne 0) { throw "vmrun start failed with exit code $LASTEXITCODE" }
}

Write-Host "VM ready: $vmx"
Write-Host "Spec: ${Vcpus} vCPU (${CoresPerSocket} cores/socket), ${MemoryMb}MB memory, $DiskSize disk, IP $GuestIp"
Write-Host "Installer ISO: $installer"
Write-Host "Seed ISO: $seed"
