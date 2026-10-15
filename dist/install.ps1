<#
.SYNOPSIS
    Axiom VCS Automated Windows Installer Script
.DESCRIPTION
    Installs axiom.exe to %LOCALAPPDATA%\Programs\Axiom\bin, registers it in the persistent
    Windows User PATH, and verifies GitHub CLI (gh) interoperability.
.EXAMPLE
    irm https://raw.githubusercontent.com/aljojoby9/Repo-test/main/dist/install.ps1 | iex
#>

$ErrorActionPreference = "Stop"

Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "               Axiom VCS Automated Windows Installer                  " -ForegroundColor Cyan
Write-Host "======================================================================" -ForegroundColor Cyan

$installDir = "$env:LOCALAPPDATA\Programs\Axiom\bin"
Write-Host ">>> Preparing installation directory: $installDir"
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

$localCandidate = Join-Path $PSScriptRoot "axiom.exe"
$releaseCandidate = Join-Path $PSScriptRoot "..\target\release\axiom.exe"

if (Test-Path $localCandidate) {
    Copy-Item $localCandidate "$installDir\axiom.exe" -Force
    Write-Host "    [OK] Installed local axiom.exe" -ForegroundColor Green
} elseif (Test-Path $releaseCandidate) {
    Copy-Item $releaseCandidate "$installDir\axiom.exe" -Force
    Write-Host "    [OK] Installed compiled release axiom.exe" -ForegroundColor Green
} else {
    Write-Host ">>> Downloading latest binary from GitHub repository..."
    $downloadUrl = "https://github.com/aljojoby9/Repo-test/raw/main/dist/axiom.exe"
    Invoke-WebRequest -Uri $downloadUrl -OutFile "$installDir\axiom.exe"
    Write-Host "    [OK] Downloaded and installed axiom.exe" -ForegroundColor Green
}

# Update User PATH
$userPath = [System.Environment]::GetEnvironmentVariable("Path", [System.EnvironmentVariableTarget]::User)
$parts = $userPath -split ';' | Where-Object { $_ -ne "" }
if ($parts -notcontains $installDir) {
    $newUserPath = if ([string]::IsNullOrEmpty($userPath)) { $installDir } else { "$userPath;$installDir" }
    [System.Environment]::SetEnvironmentVariable("Path", $newUserPath, [System.EnvironmentVariableTarget]::User)
    Write-Host "    [OK] Added to persistent Windows User PATH." -ForegroundColor Green
} else {
    Write-Host "    [OK] Already in Windows User PATH." -ForegroundColor Green
}

$env:PATH = "$installDir;$env:PATH"

# Ecosystem Check
Write-Host "`n>>> Ecosystem Check:"
if (Get-Command gh -ErrorAction SilentlyContinue) {
    Write-Host "    [OK] GitHub CLI (gh) detected! Ready to run side-by-side with Axiom." -ForegroundColor Green
} else {
    Write-Host "    [INFO] GitHub CLI not found. Install with: winget install GitHub.cli" -ForegroundColor Yellow
}

Write-Host "`n>>> Verification:"
& "$installDir\axiom.exe" --version

Write-Host "`n======================================================================" -ForegroundColor Cyan
Write-Host "[SUCCESS] Axiom VCS is ready to use!" -ForegroundColor Green
Write-Host "Type 'axiom --help' in your terminal." -ForegroundColor Green
Write-Host "======================================================================" -ForegroundColor Cyan
