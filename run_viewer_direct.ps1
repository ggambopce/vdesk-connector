# 다이렉트 모드 뷰어 — 백엔드 불필요
# 에이전트에 직접 TCP 연결합니다.
# 제어 모드: 커서가 뷰어 안에 고정되고 ESC로만 해제됩니다.

param(
    [string]$AgentHost = "127.0.0.1",
    [string]$Port      = "20020",
    [string]$Key       = "direct",
    [string]$LogLevel  = "info"
)

$env:VDESK_DIRECT      = "1"
$env:VDESK_DIRECT_HOST = $AgentHost
$env:VDESK_DIRECT_PORT = $Port
$env:VDESK_DIRECT_KEY  = $Key
$env:RUST_LOG          = $LogLevel

Write-Host "[뷰어 다이렉트] 연결: ${AgentHost}:${Port}  세션키: $Key" -ForegroundColor Green

$exe = "$PSScriptRoot\target\release\vdesk_viewer.exe"
if (-not (Test-Path $exe)) {
    Write-Host "[오류] 빌드가 없습니다:" -ForegroundColor Red
    Write-Host "  cargo build --release --package vdesk_viewer" -ForegroundColor Yellow
    exit 1
}

& $exe
