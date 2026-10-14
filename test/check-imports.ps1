# Fails if the exe imports any DLL that does not ship with Windows (single-exe rule).
#   powershell -ExecutionPolicy Bypass -File test\check-imports.ps1 [-ExePath target\release\flashtester.exe]
param([string]$ExePath = "$PSScriptRoot\..\target\release\flashtester.exe")
$allowed = 'kernel32|user32|gdi32|combase|uiautomationcore|opengl32|advapi32|shell32|ole32|oleaut32|setupapi|cfgmgr32|dwmapi|imm32|ntdll|userenv|uxtheme|ws2_32|bcrypt|bcryptprimitives|comctl32|shlwapi|winmm|version|msvcrt|ucrtbase|api-ms-win-[a-z0-9-]+'
$objdump = Get-Command llvm-objdump, objdump -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $objdump) {
    $sysroot = & rustc --print sysroot
    $objdump = Get-ChildItem "$sysroot\lib\rustlib" -Recurse -Filter llvm-objdump.exe | Select-Object -First 1
}
if (-not $objdump) { Write-Error 'need objdump or llvm-objdump (rustup component add llvm-tools)'; exit 2 }
$dlls = & $objdump.Source -p $ExePath | Select-String 'DLL Name:\s*(\S+)' | ForEach-Object { $_.Matches[0].Groups[1].Value.ToLower() } | Sort-Object -Unique
$bad = $dlls | Where-Object { $_ -notmatch "^($allowed)\.dll$" }
"Imports: $($dlls -join ', ')"
if ($bad) { "NOT ALLOWED: $($bad -join ', ')"; exit 1 }
'OK: only Windows system DLLs'
