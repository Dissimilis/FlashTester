param(
    [string]$Exe = "$PSScriptRoot\..\target\release\flashtester.exe",
    [string]$WorkDir = "$env:TEMP\flashtester-backup-$PID"
)
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this test as administrator. It only writes to VHDs that it creates.'
}
$Exe = (Resolve-Path -LiteralPath $Exe).Path
if (Test-Path -LiteralPath $WorkDir) { throw 'Choose a new test directory.' }
$work = (New-Item -ItemType Directory -Path $WorkDir).FullName
Start-Transcript -Path (Join-Path $work 'test.log') | Out-Null
$vhds = @()
$fails = 0
function Run([string]$Name, [int]$Expected, [string[]]$Arguments) {
    $text = & $Exe @Arguments --json -q 2>&1 | Out-String
    $code = $LASTEXITCODE
    $result = $text | ConvertFrom-Json
    if ($code -ne $Expected) { throw "$Name failed: exit $code, expected $Expected. $text" }
    Write-Host "PASS $Name (exit $code): $($result.verdict.headline)"
    return $result
}
function MakeDisk([string]$Name, [int]$SizeMB) {
    $file = Join-Path $work "$Name.vhdx"
    $script = Join-Path $work "$Name.diskpart.txt"
    @"
create vdisk file="$file" maximum=$SizeMB type=expandable
select vdisk file="$file"
attach vdisk
"@ | Set-Content -LiteralPath $script -Encoding ascii
    diskpart /s $script | Out-Null
    $script:vhds += $file
    $disk = Get-DiskImage -ImagePath $file | Get-Disk
    if ($disk.IsSystem -or $disk.IsBoot -or $disk.BusType -ne 'File Backed Virtual') { throw 'Unexpected disk identity; refusing.' }
    return $disk
}
try {
    $source = MakeDisk 'source' 128
    $target = MakeDisk 'target' 128
    $small = MakeDisk 'small' 64
    Initialize-Disk -Number $source.Number -PartitionStyle MBR | Out-Null
    $partition = New-Partition -DiskNumber $source.Number -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel FTBACKUP -Confirm:$false | Out-Null
    $letter = $partition.DriveLetter
    $root = "${letter}:\"
    $folder = Join-Path $root 'original'
    New-Item -ItemType Directory -Path $folder | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $folder 'empty') | Out-Null
    $payload = Join-Path $folder 'payload.bin'
    $bytes = New-Object byte[] (2MB + 513)
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    $rng.GetBytes($bytes)
    $rng.Dispose()
    [IO.File]::WriteAllBytes($payload, $bytes)
    $hash = (Get-FileHash -LiteralPath $payload -Algorithm SHA256).Hash
    $zip = Join-Path $work 'files.zip'
    $null = Run 'ZIP backup' 0 @('backup', $folder, '--output', $zip)
    $null = Run 'backup does not replace an existing archive' 3 @('backup', $folder, '--output', $zip)
    $null = Run 'same-disk backup is refused' 3 @('backup', $folder, '--output', (Join-Path $root 'bad.zip'))
    $restored = Join-Path $root 'restored'
    $null = Run 'ZIP restore' 0 @('restore', $zip, '--to', $restored)
    if ((Get-FileHash -LiteralPath (Join-Path $restored 'payload.bin') -Algorithm SHA256).Hash -ne $hash) { throw 'Restored file differs.' }
    if (-not (Test-Path -LiteralPath (Join-Path $restored 'empty') -PathType Container)) { throw 'Empty directory missing.' }
    $null = Run 'restore does not replace an existing folder' 3 @('restore', $zip, '--to', $restored)
    $image = Join-Path $work 'source.img'
    $null = Run 'whole-disk backup' 0 @('backup', '--disk', "$($source.Number)", '--output', $image)
    $null = Run 'raw restore requires --destroy' 2 @('restore', $image, '--disk', "$($target.Number)")
    $null = Run 'image does not fit smaller disk' 3 @('restore', $image, '--disk', "$($small.Number)", '--destroy')
    $bad = Join-Path $work 'corrupt.img'
    Copy-Item -LiteralPath $image -Destination $bad
    Copy-Item -LiteralPath "$image.json" -Destination "$bad.json"
    $f = [IO.File]::Open($bad, 'Open', 'ReadWrite', 'None')
    $f.WriteByte(123)
    $f.Dispose()
    $null = Run 'corrupt image rejected before writing' 3 @('restore', $bad, '--disk', "$($target.Number)", '--destroy')
    if ((Get-Disk -Number $target.Number).PartitionStyle -ne 'RAW') { throw 'Corrupt restore changed the blank disk.' }
    $null = Run 'whole-disk restore and read-back' 0 @('restore', $image, '--disk', "$($target.Number)", '--destroy')
    $sys = (Get-Partition -DriveLetter $env:SystemDrive[0]).DiskNumber
    $null = Run 'system disk restore refused' 2 @('restore', $image, '--disk', "$sys", '--destroy')
    $null = Run 'system disk image backup refused without snapshot support' 2 @('backup', '--disk', "$sys", '--output', (Join-Path $work 'system.img'))
    $null = Run 'system folder restore refused' 2 @('restore', $zip, '--to', (Join-Path $env:SystemDrive 'ft-forbidden-restore'))
    'PASS all backup integration checks'
}
catch {
    $fails = 1
    "FAIL: $_"
    $_.ScriptStackTrace
}
finally {
    foreach ($vhd in $vhds) {
        $resolved = [IO.Path]::GetFullPath($vhd)
        if (-not $resolved.StartsWith($work + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Cleanup path escaped the test directory.' }
        try {
            Dismount-DiskImage -ImagePath $resolved -ErrorAction Stop | Out-Null
            Remove-Item -LiteralPath $resolved
        } catch { "Cleanup failed: $_"; $fails = 1 }
    }
    "RESULT: $fails failure(s)"
    Set-Content -LiteralPath (Join-Path $work 'exit-code.txt') -Value $fails
    Stop-Transcript | Out-Null
}
exit $fails
