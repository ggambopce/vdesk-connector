#Requires -Version 5.1
<#
.SYNOPSIS
    VDesk Agent 설치 스크립트 — 입력 없이 완료
.DESCRIPTION
    1. 관리자 권한 확인 (없으면 UAC 재실행)
    2. C:\VDesk\ 에 vdesk_agent.exe 복사
    3. 방화벽 TCP 20020 inbound 허용
    4. 작업 스케줄러 "VDeskAgent" 등록 (로그온 시 자동 시작, 최상위 권한)
    5. 즉시 실행
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── 관리자 권한 확인 ──────────────────────────────────────────────────────────
if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host "관리자 권한이 필요합니다. UAC 재실행 중..."
    Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`"" `
        -Verb RunAs -Wait
    exit
}

$InstallDir  = "C:\VDesk"
$ExeName     = "vdesk_agent.exe"
$TaskName    = "VDeskAgent"
$FirewallRule = "VDesk Agent TCP 20020"
$SrcExe      = Join-Path $PSScriptRoot $ExeName

# ── 소스 파일 확인 ────────────────────────────────────────────────────────────
if (-not (Test-Path $SrcExe)) {
    Write-Error "오류: $SrcExe 를 찾을 수 없습니다. install_agent.ps1 과 같은 폴더에 $ExeName 을 복사하세요."
    exit 1
}

# ── 설치 디렉터리 생성 ────────────────────────────────────────────────────────
Write-Host "[1/5] 설치 디렉터리 생성: $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path "$InstallDir\logs" | Out-Null

# ── exe 복사 ──────────────────────────────────────────────────────────────────
Write-Host "[2/5] 에이전트 복사: $SrcExe → $InstallDir\$ExeName"
Copy-Item $SrcExe "$InstallDir\$ExeName" -Force

# ── 방화벽 ────────────────────────────────────────────────────────────────────
Write-Host "[3/5] 방화벽 규칙 설정: TCP 20020 inbound"
$existing = Get-NetFirewallRule -DisplayName $FirewallRule -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "      (기존 규칙 있음 — 업데이트)"
    Remove-NetFirewallRule -DisplayName $FirewallRule
}
New-NetFirewallRule `
    -DisplayName  $FirewallRule `
    -Direction    Inbound `
    -Protocol     TCP `
    -LocalPort    20020 `
    -Action       Allow `
    -Profile      Any | Out-Null

# ── 작업 스케줄러 등록 ────────────────────────────────────────────────────────
Write-Host "[4/5] 작업 스케줄러 등록: $TaskName"

# 기존 작업 제거
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue

$action  = New-ScheduledTaskAction -Execute "$InstallDir\$ExeName"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit (New-TimeSpan -Seconds 0) `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable

$principal = New-ScheduledTaskPrincipal `
    -UserId    $env:USERNAME `
    -LogonType Interactive `
    -RunLevel  Highest

Register-ScheduledTask `
    -TaskName  $TaskName `
    -Action    $action `
    -Trigger   $trigger `
    -Settings  $settings `
    -Principal $principal `
    -Force | Out-Null

# ── 즉시 시작 ─────────────────────────────────────────────────────────────────
Write-Host "[5/5] 에이전트 즉시 시작"
# 이미 실행 중이면 종료 후 재시작
Stop-Process -Name "vdesk_agent" -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Start-Process "$InstallDir\$ExeName"

Write-Host ""
Write-Host "✓ VDesk Agent 설치 완료"
Write-Host "  설치 경로 : $InstallDir\$ExeName"
Write-Host "  로그 파일 : $InstallDir\logs\vdesk_agent.log"
Write-Host "  자동 시작 : 로그온 시 (작업 스케줄러 '$TaskName')"
Write-Host "  방화벽    : TCP 20020 inbound 허용"
