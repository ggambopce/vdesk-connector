# 다이렉트 모드 에이전트 — 백엔드 불필요
# 에이전트가 바로 TCP 대기 상태로 진입합니다.

param(
    [string]$Key      = "direct",
    [string]$Port     = "20020",
    [string]$LogLevel = "info"
)

$env:VDESK_DIRECT     = "1"
$env:VDESK_DIRECT_KEY = $Key
$env:AGENT_PORT       = $Port
$env:RUST_LOG         = $LogLevel

Write-Host "[에이전트 다이렉트] 포트: $Port  세션키: $Key" -ForegroundColor Green

$exe = "$PSScriptRoot\target\release\vdesk_agent.exe"
if (-not (Test-Path $exe)) {
    Write-Host "[오류] 빌드가 없습니다:" -ForegroundColor Red
    Write-Host "  cargo build --release --package vdesk_agent" -ForegroundColor Yellow
    exit 1
}

& $exe
