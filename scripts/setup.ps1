# Nexus-LLM Windows bootstrap entrypoint.
# Prefer WSL (AGENTS.md: Cargo lives in WSL, not native PowerShell).
#
# Usage (from repo root in PowerShell):
#   .\scripts\setup.ps1
#   .\scripts\setup.ps1 -SkipMoe -Jobs 4
#   .\scripts\setup.ps1 -Native   # only if MSVC + CMake + Rust are already installed

[CmdletBinding()]
param(
    [string]$Prefix = "",
    [int]$Jobs = 0,
    [switch]$SkipLlama,
    [switch]$SkipMoe,
    [switch]$SkipNexus,
    [switch]$SkipDoctor,
    [string]$Model = "",
    [switch]$DryRun,
    [switch]$Native
)

$ErrorActionPreference = "Stop"
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$SetupSh = Join-Path $RepoRoot "scripts\setup.sh"

function Build-BashArgs {
    $argsList = @()
    if ($Prefix) { $argsList += @("--prefix", $Prefix) }
    if ($Jobs -gt 0) { $argsList += @("--jobs", "$Jobs") }
    if ($SkipLlama) { $argsList += "--skip-llama" }
    if ($SkipMoe) { $argsList += "--skip-moe" }
    if ($SkipNexus) { $argsList += "--skip-nexus" }
    if ($SkipDoctor) { $argsList += "--skip-doctor" }
    if ($Model) { $argsList += @("--model", $Model) }
    if ($DryRun) { $argsList += "--dry-run" }
    return $argsList
}

$bashArgs = Build-BashArgs
$argString = ($bashArgs | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '

if (-not $Native) {
    $wsl = Get-Command wsl -ErrorAction SilentlyContinue
    if (-not $wsl) {
        Write-Error "WSL not found. Install WSL2, or re-run with -Native if you have MSVC+CMake+Rust."
    }
    # Map Windows path to /mnt/<drive>/...
    $drive = $RepoRoot.Path.Substring(0, 1).ToLower()
    $unixPath = $RepoRoot.Path -replace '\\', '/' -replace '^[A-Za-z]:', "/mnt/$drive"
    $cmd = "cd '$unixPath' && bash scripts/setup.sh $argString"
    Write-Host "==> Delegating to WSL: $cmd"
    & wsl bash -l -c $cmd
    exit $LASTEXITCODE
}

Write-Host "==> Native Windows path (experimental)"
if (-not (Get-Command bash -ErrorAction SilentlyContinue)) {
    Write-Error "Git Bash / bash not on PATH. Use WSL setup instead."
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not on PATH. Install Rust or use WSL (recommended)."
}
if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
    Write-Error "cmake not on PATH."
}

& bash $SetupSh @bashArgs
exit $LASTEXITCODE
