<#
.SYNOPSIS
    Axint VCS Automated Windows Installer Script
.DESCRIPTION
    Installs axint.exe to %LOCALAPPDATA%\Programs\Axint\bin, registers it in the persistent
    Windows User PATH, and verifies GitHub CLI (gh) interoperability.
.EXAMPLE
    irm https://raw.githubusercontent.com/aljojoby9/Axint/main/dist/install.ps1 | iex
#>

$ErrorActionPreference = "Stop"

Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "               Axint VCS Automated Windows Installer                  " -ForegroundColor Cyan
Write-Host "======================================================================" -ForegroundColor Cyan

$installDir = "$env:LOCALAPPDATA\Programs\Axint\bin"
Write-Host ">>> Preparing installation directory: $installDir"
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

$localCandidate = Join-Path $PSScriptRoot "axint.exe"
$releaseCandidate = Join-Path $PSScriptRoot "..\target\release\axint.exe"

if (Test-Path $localCandidate) {
    Copy-Item $localCandidate "$installDir\axint.exe" -Force
    Write-Host "    [OK] Installed local axint.exe" -ForegroundColor Green
} elseif (Test-Path $releaseCandidate) {
    Copy-Item $releaseCandidate "$installDir\axint.exe" -Force
    Write-Host "    [OK] Installed compiled release axint.exe" -ForegroundColor Green
} else {
    Write-Host ">>> Downloading latest binary from GitHub repository..."
    $downloadUrl = "https://github.com/aljojoby9/Axint/raw/main/dist/axint.exe"
    Invoke-WebRequest -Uri $downloadUrl -OutFile "$installDir\axint.exe"
    Write-Host "    [OK] Downloaded and installed axint.exe" -ForegroundColor Green
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
    Write-Host "    [OK] GitHub CLI (gh) detected! Ready to run side-by-side with Axint." -ForegroundColor Green
} else {
    Write-Host "    [INFO] GitHub CLI not found. Install with: winget install GitHub.cli" -ForegroundColor Yellow
}

Write-Host "`n>>> Verification:"
& "$installDir\axint.exe" --version

Write-Host "`n======================================================================" -ForegroundColor Cyan
Write-Host "[SUCCESS] Axint VCS is ready to use!" -ForegroundColor Green
Write-Host "Type 'axint --help' in your terminal." -ForegroundColor Green
Write-Host "======================================================================" -ForegroundColor Cyan
