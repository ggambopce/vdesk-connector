#Requires -Version 5.1
<#
.SYNOPSIS
    VDesk Agent 제거 스크립트
.DESCRIPTION
    1. 작업 스케줄러 "VDeskAgent" 해제
    2. 방화벽 규칙 제거
    3. 프로세스 종료
    4. C:\VDesk\ 삭제
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'SilentlyContinue'

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host "관리자 권한이 필요합니다. UAC 재실행 중..."
    Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`"" `
        -Verb RunAs -Wait
    exit
}

$InstallDir   = "C:\VDesk"
$TaskName     = "VDeskAgent"
$FirewallRule = "VDesk Agent TCP 20020"

Write-Host "[1/4] 작업 스케줄러 해제: $TaskName"
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false

Write-Host "[2/4] 방화벽 규칙 제거: $FirewallRule"
Remove-NetFirewallRule -DisplayName $FirewallRule

Write-Host "[3/4] 프로세스 종료"
Get-Process -Name "vdesk_agent" | Stop-Process -Force
Start-Sleep -Milliseconds 500

Write-Host "[4/4] 설치 디렉터리 삭제: $InstallDir"
Remove-Item -Recurse -Force $InstallDir

Write-Host ""
Write-Host "✓ VDesk Agent 제거 완료"
