#Requires -Version 5.1
<#
.SYNOPSIS
    VDesk-Agent-Setup.exe 빌드 자동화
.DESCRIPTION
    1. cargo build --release (VDESK_API_URL 바이너리에 고정)
    2. TightVNC.msi 존재 확인
    3. iscc VDeskAgentSetup.iss → Output\VDesk-Agent-Setup.exe

사전 준비:
    - Rust 툴체인 설치 (cargo)
    - Inno Setup 6 설치 (C:\Program Files (x86)\Inno Setup 6\ISCC.exe)
    - installer\TightVNC.msi 복사 (https://www.tightvnc.com/download.php)

사용법:
    .\build_installer.ps1                          # URL: https://vdesk.co.kr (기본)
    .\build_installer.ps1 -ApiUrl "https://vdesk.co.kr"
#>
param(
    [string]$ApiUrl = "https://vdesk.co.kr"
)

$ErrorActionPreference = 'Stop'
$root      = Split-Path $PSScriptRoot -Parent
$iscc      = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
$msiPath   = Join-Path $PSScriptRoot "TightVNC.msi"
$outputExe = Join-Path $PSScriptRoot "Output\VDesk-Agent-Setup.exe"

# ── [1/3] Rust 릴리스 빌드 ─────────────────────────────────────────────────
Write-Host "[1/3] Rust 릴리스 빌드 (URL 고정: $ApiUrl)"
$env:VDESK_API_URL = $ApiUrl
cargo build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build 실패 (exit $LASTEXITCODE)"
    exit 1
}
Write-Host "      → $root\target\release\vdesk_agent.exe"

# ── [2/3] TightVNC.msi 확인 ────────────────────────────────────────────────
Write-Host "[2/3] TightVNC.msi 확인"
if (-not (Test-Path $msiPath)) {
    Write-Error @"
TightVNC.msi 가 없습니다: $msiPath

다운로드:
  https://www.tightvnc.com/download.php
  → tightvnc-2.8.85-gpl-setup-64bit.msi 를 installer\ 폴더에 TightVNC.msi 로 저장
"@
    exit 1
}
$msiSize = (Get-Item $msiPath).Length / 1MB
Write-Host "      TightVNC.msi 확인 OK ($([math]::Round($msiSize,1)) MB)"

# ── [3/3] Inno Setup 컴파일 ────────────────────────────────────────────────
Write-Host "[3/3] Inno Setup 컴파일"
if (-not (Test-Path $iscc)) {
    Write-Error "Inno Setup 6 미설치: $iscc`n  https://jrsoftware.org/isdownload.php 에서 설치하세요."
    exit 1
}
& $iscc "$PSScriptRoot\VDeskAgentSetup.iss"
if ($LASTEXITCODE -ne 0) {
    Write-Error "iscc 컴파일 실패 (exit $LASTEXITCODE)"
    exit 1
}

# ── 완료 ────────────────────────────────────────────────────────────────────
$exeSize = [math]::Round((Get-Item $outputExe).Length / 1MB, 1)
Write-Host ""
Write-Host "✓ 빌드 완료: $outputExe ($exeSize MB)"
Write-Host ""
Write-Host "배포 방법:"
Write-Host "  더블클릭 설치   : Setup.exe 복사 → 더블클릭 → UAC 승인 → URL 확인 → 설치"
Write-Host "  사일런트 설치   : .\VDesk-Agent-Setup.exe /VERYSILENT /SUPPRESSMSGBOXES"
Write-Host "  URL 지정 사일런트: .\VDesk-Agent-Setup.exe /VERYSILENT /ApiUrl=""$ApiUrl"""
