#!/usr/bin/env pwsh
# Build the Unterm native terminal and install it as a Unity Windows plugin DLL.
#
# Unity loads native plugins on Windows from a .dll. A Rust cdylib already builds
# `unterm.dll`, so we just copy it into the package's Plugins/Windows/x86_64.
[CmdletBinding()]
param(
    # 'release' or 'debug'. Named -Configuration to avoid PowerShell's automatic
    # $PROFILE variable.
    [ValidateSet('release', 'debug')]
    [string]$Configuration = 'release',

    # Default to the MSVC ABI that matches the Unity Editor. Pass
    # x86_64-pc-windows-gnu on a machine that only has the GNU toolchain.
    [string]$Target = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot

$cargoFlags = @()
$targetDir = 'debug'
if ($Configuration -eq 'release') {
    $cargoFlags += '--release'
    $targetDir = 'release'
}

# Idempotent (already-installed is fine). Pipe only stdout to Out-Null; do NOT
# redirect stderr — under $ErrorActionPreference='Stop', redirecting a native
# command's stderr turns its progress text into a terminating error.
rustup target add $Target | Out-Null

Write-Host "==> building unterm ($Configuration, $Target)"
cargo build -p unterm @cargoFlags --target $Target
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }

$dest = Join-Path $PSScriptRoot '..\Packages\dev.tnayuki.unterm\Editor\Plugins\Windows\x86_64\unterm.dll'
$destDir = Split-Path -Parent $dest
New-Item -ItemType Directory -Force -Path $destDir | Out-Null

$lib = Join-Path $PSScriptRoot "target\$Target\$targetDir\unterm.dll"
if (-not (Test-Path -LiteralPath $lib)) { throw "built dll not found: $lib" }

Write-Host "==> copy -> $dest"
Copy-Item -LiteralPath $lib -Destination $dest -Force

Write-Host "==> done: $dest"
