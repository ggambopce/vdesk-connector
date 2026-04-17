#Requires -Version 5.1
<#
.SYNOPSIS
    VDesk 빌드 + 배포 통합 스크립트
.PARAMETER ApiUrl
    빌드 시 바이너리에 고정할 백엔드 API URL
    (기본: https://your-server.com)
.EXAMPLE
    .\deploy.ps1 -ApiUrl "https://vdesk.example.com"
#>

param(
    [string]$ApiUrl = "https://your-server.com"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ScriptDir   = $PSScriptRoot
$BackendStatic = Join-Path $ScriptDir "..\vdesk\src\main\resources\static\downloads"
$CargoTarget   = Join-Path $ScriptDir "target\release"

Write-Host "=== VDesk 배포 빌드 ==="
Write-Host "API URL : $ApiUrl"
Write-Host ""

# ── API URL 환경변수 설정 ──────────────────────────────────────────────────────
$env:VDESK_API_URL = $ApiUrl

# ── 빌드 ──────────────────────────────────────────────────────────────────────
Write-Host "[1/3] vdesk_agent 빌드..."
Push-Location $ScriptDir
cargo build --release --package vdesk_agent
if ($LASTEXITCODE -ne 0) { throw "vdesk_agent 빌드 실패" }

Write-Host "[2/3] vdesk_viewer 빌드..."
cargo build --release --package vdesk_viewer
if ($LASTEXITCODE -ne 0) { throw "vdesk_viewer 빌드 실패" }
Pop-Location

# ── 뷰어 → 백엔드 static 복사 ────────────────────────────────────────────────
Write-Host "[3/3] 뷰어 → 백엔드 static 복사..."
New-Item -ItemType Directory -Force -Path $BackendStatic | Out-Null
Copy-Item "$CargoTarget\vdesk_viewer.exe" "$BackendStatic\vdesk_viewer.exe" -Force

Write-Host ""
Write-Host "=== 배포 완료 ==="
Write-Host "  API URL 고정값  : $ApiUrl"
Write-Host "  에이전트 exe    : $CargoTarget\vdesk_agent.exe"
Write-Host "  뷰어 static 경로: $BackendStatic\vdesk_viewer.exe"
Write-Host "  뷰어 다운로드   : $ApiUrl/downloads/vdesk_viewer.exe"
Write-Host ""
Write-Host "[다음 단계]"
Write-Host "  VM 설치: vdesk_agent.exe + install_agent.ps1 복사 후 install_agent.ps1 실행"
Write-Host "  백엔드 재시작 후 /downloads/vdesk_viewer.exe HTTP 200 확인"
