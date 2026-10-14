# Elevated integration test: creates a throwaway VHDX (a non-removable, non-system
# disk, like an internal data drive), runs every raw and file-mode command against
# it, checks exit codes, and deletes it. Never touches a pre-existing disk.
#   powershell -ExecutionPolicy Bypass -File test\vhd-test.ps1 [-ExePath target\release\flashtester.exe]
param(
    [string]$ExePath = "$PSScriptRoot\..\target\release\flashtester.exe",
    [string]$LogPath = "$env:TEMP\flashtester-vhd-test.log"
)
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error 'Run this from an elevated (administrator) PowerShell.'
}
$exe = (Resolve-Path $ExePath).Path
$vhd = Join-Path $env:TEMP "flashtester-test-$PID.vhdx"
$fails = 0
Start-Transcript -Path $LogPath -Force | Out-Null

function Run([string]$name, [int]$want, [string[]]$argv) {
    $out = & $exe @argv 2>&1 | Out-String
    $code = $LASTEXITCODE
    $ok = $code -eq $want
    if (-not $ok) { $script:fails++ }
    "{0} {1} (exit {2}, want {3})" -f ($(if ($ok) { 'PASS' } else { 'FAIL' })), $name, $code, $want
    $out
}

try {
    "Creating $vhd"
    @"
create vdisk file="$vhd" maximum=2048 type=expandable
select vdisk file="$vhd"
attach vdisk
"@ | Set-Content "$env:TEMP\ft-dp-$PID.txt" -Encoding ascii
    diskpart /s "$env:TEMP\ft-dp-$PID.txt" | Out-Null
    $disk = Get-DiskImage -ImagePath $vhd | Get-Disk
    $n = $disk.Number
    if ($disk.IsSystem -or $disk.IsBoot -or $disk.BusType -ne 'File Backed Virtual') { throw "unexpected disk $n" }
    "VHD is disk $n"
    Initialize-Disk -Number $n -PartitionStyle MBR
    $part = New-Partition -DiskNumber $n -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel FTTEST -Confirm:$false | Out-Null
    $L = (Get-Partition -DiskNumber $n | Where-Object DriveLetter | Select-Object -First 1).DriveLetter
    "Volume is ${L}:"

    $sys = (Get-Partition -DriveLetter ($env:SystemDrive[0])).DiskNumber
    Run 'system disk is refused (raw)' 2 @('test', '--disk', "$sys", '--destroy')
    Run 'system drive is refused (file)' 2 @('test', $env:SystemDrive)
    Run 'raw write needs --destroy' 2 @('probe', '--disk', "$n")
    cmd /c mklink /J "${L}:\junction" "%TEMP%" | Out-Null
    Run 'junction into the system disk is refused' 2 @('small-files', "${L}:\junction", '--count', '10')
    cmd /c rmdir "${L}:\junction"
    Run 'list' 0 @('list')
    Run 'info' 0 @('info', '--disk', "$n")

    Run 'file test 300M' 0 @('test', "${L}:", '--size', '300M', '-q')
    Run 'verify' 0 @('verify', "${L}:", '-q')
    Run 'small files' 0 @('small-files', "${L}:", '--count', '500', '-q')
    Run 'iops (file)' 0 @('iops', "${L}:", '--seconds', '1', '-q')
    Run 'clean' 0 @('clean', "${L}:")
    Run 'free-space probe' 0 @('probe', "${L}:", '-q')

    Run 'raw probe' 0 @('probe', '--disk', "$n", '--destroy', '-q')
    Run 'raw test 300M' 0 @('test', '--disk', "$n", '--destroy', '--size', '300M', '-q')
    Run 'raw verify' 0 @('verify', '--disk', "$n", '-q')
    Run 'surface scan' 0 @('scan', '--disk', "$n", '-q')
    Run 'iops (raw read)' 0 @('iops', '--disk', "$n", '--seconds', '1', '-q')
    Run 'endurance 2 passes' 0 @('endurance', '--disk', "$n", '--destroy', '--size', '100M', '--passes', '2', '-q')
    Run 'fix to 1 GB' 0 @('fix', '--disk', "$n", '--size', '1G', '--fs', 'exfat', '--label', 'FIXED', '--destroy', '-q')
    $vol = Get-Partition -DiskNumber $n | Get-Volume | Select-Object -First 1
    if ($vol.FileSystem -eq 'exFAT' -and $vol.Size -lt 1.01GB -and $vol.Size -gt 0.9GB) { 'PASS fix created a ~1 GB exFAT volume' } else { $fails++; "FAIL fix: $($vol | Out-String)" }
    Run 'fix with probe' 0 @('fix', '--disk', "$n", '--destroy', '-q')
}
catch {
    $fails++
    "FAIL script error: $_"
    $_.ScriptStackTrace
}
finally {
    "Removing $vhd"
    @"
select vdisk file="$vhd"
detach vdisk
"@ | Set-Content "$env:TEMP\ft-dp-$PID.txt" -Encoding ascii
    diskpart /s "$env:TEMP\ft-dp-$PID.txt" | Out-Null
    Remove-Item $vhd, "$env:TEMP\ft-dp-$PID.txt" -ErrorAction SilentlyContinue
    "RESULT: $fails failure(s)"
    Stop-Transcript | Out-Null
}
exit $fails
