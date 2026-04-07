# 로컬 테스트용 에이전트 실행 스크립트
# 같은 PC에서 에이전트+뷰어를 동시에 실행할 때 사용합니다.
# 입력 주입이 자동으로 비활성화되어 마우스 피드백 루프가 방지됩니다.

param(
    [string]$ApiUrl   = "http://localhost:8080",
    [string]$Port     = "20020",
    [string]$LogLevel = "info"
)

$env:VDESK_API_URL  = $ApiUrl
$env:AGENT_RELAY_IP = "127.0.0.1"
$env:AGENT_PORT     = $Port
$env:RUST_LOG       = $LogLevel

Write-Host "[에이전트] API: $ApiUrl  포트: $Port  Relay: 127.0.0.1 (로컬 테스트 모드)" -ForegroundColor Cyan

$exe = "$PSScriptRoot\target\release\vdesk_agent.exe"
if (-not (Test-Path $exe)) {
    Write-Host "[오류] 빌드가 없습니다. 먼저 실행하세요:" -ForegroundColor Red
    Write-Host "  cargo build --release --package vdesk_agent" -ForegroundColor Yellow
    exit 1
}

& $exe
