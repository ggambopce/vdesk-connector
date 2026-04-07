# 로컬 테스트용 뷰어 실행 스크립트
# 같은 PC에서 에이전트+뷰어를 동시에 실행할 때 사용합니다.

param(
    [string]$ApiUrl   = "http://localhost:8080",
    [string]$Email    = "",
    [string]$Password = "",
    [string]$Device   = "",
    [string]$LogLevel = "info"
)

$env:VDESK_API_URL = $ApiUrl
$env:RUST_LOG      = $LogLevel

if ($Email)    { $env:VDESK_EMAIL    = $Email }
if ($Password) { $env:VDESK_PASSWORD = $Password }

Write-Host "[뷰어] API: $ApiUrl" -ForegroundColor Cyan

$exe = "$PSScriptRoot\target\release\vdesk_viewer.exe"
if (-not (Test-Path $exe)) {
    Write-Host "[오류] 빌드가 없습니다. 먼저 실행하세요:" -ForegroundColor Red
    Write-Host "  cargo build --release --package vdesk_viewer" -ForegroundColor Yellow
    exit 1
}

if ($Device) {
    & $exe --device $Device
} else {
    & $exe
}
