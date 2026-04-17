#Requires -Version 5.1
<#
.SYNOPSIS
    VDesk Agent 설치 스크립트 — 입력 없이 완료
.DESCRIPTION
    1. 관리자 권한 확인 (없으면 UAC 재실행)
    2. C:\VDesk\ 에 vdesk_agent.exe 복사
    3. TightVNC 서버 설치 (VNC 원격 화면 제공용, 포트 5900)
    4. 방화벽 TCP 20020 inbound 허용 (5900은 내부 전용 — 외부 차단)
    5. 작업 스케줄러 "VDeskAgent" 등록 (로그온 시 자동 시작, 최상위 권한)
    6. 즉시 실행

환경변수:
    VNC_PASSWORD  — TightVNC 비밀번호 (기본: "vdesk1234")
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

$InstallDir   = "C:\VDesk"
$ExeName      = "vdesk_agent.exe"
$TaskName     = "VDeskAgent"
$FirewallRule = "VDesk Agent TCP 20020"
$SrcExe       = Join-Path $PSScriptRoot $ExeName
$VncPassword  = if ($env:VNC_PASSWORD) { $env:VNC_PASSWORD } else { "vdesk1234" }

# TightVNC 설치 관련 상수
$TvncVersion  = "2.8.85"
$TvncUrl      = "https://www.tightvnc.com/download/$TvncVersion/tightvnc-$TvncVersion-gpl-setup-64bit.msi"
$TvncInstaller= "$env:TEMP\tightvnc-setup.msi"
$TvncService  = "tvnserver"

# ── 소스 파일 확인 ────────────────────────────────────────────────────────────
if (-not (Test-Path $SrcExe)) {
    Write-Error "오류: $SrcExe 를 찾을 수 없습니다. install_agent.ps1 과 같은 폴더에 $ExeName 을 복사하세요."
    exit 1
}

# ── 설치 디렉터리 생성 ────────────────────────────────────────────────────────
Write-Host "[1/6] 설치 디렉터리 생성: $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path "$InstallDir\logs" | Out-Null

# ── exe 복사 ──────────────────────────────────────────────────────────────────
Write-Host "[2/6] 에이전트 복사: $SrcExe → $InstallDir\$ExeName"
Copy-Item $SrcExe "$InstallDir\$ExeName" -Force

# ── TightVNC 서버 설치 ────────────────────────────────────────────────────────
Write-Host "[3/6] TightVNC 서버 설치 (포트 5900, 비밀번호 인증)"

$tvncInstalled = Get-Service -Name $TvncService -ErrorAction SilentlyContinue
if ($tvncInstalled) {
    Write-Host "      (TightVNC 이미 설치됨 — 비밀번호만 업데이트)"
    # 실행 중인 서비스 중지 후 레지스트리로 비밀번호 업데이트
    Stop-Service -Name $TvncService -Force -ErrorAction SilentlyContinue
    $regPath = "HKLM:\SOFTWARE\TightVNC\Server"
    if (Test-Path $regPath) {
        # TightVNC는 비밀번호를 DES 암호화해 저장하지만, msiexec /modify로 재설정 가능
        # 여기서는 재설치(덮어쓰기)로 비밀번호를 보장
        Write-Host "      (TightVNC 재설치로 비밀번호 갱신)"
        $tvncInstalled = $null  # 아래 설치 블록 진행
    }
}

if (-not $tvncInstalled) {
    $tvncInstallOk = $false
    Write-Host "      TightVNC MSI 다운로드 중: $TvncUrl"
    try {
        Invoke-WebRequest -Uri $TvncUrl -OutFile $TvncInstaller -UseBasicParsing
        $tvncInstallOk = $true
    } catch {
        Write-Warning "다운로드 실패: $_"
        Write-Warning "TightVNC를 수동 설치하고 포트 5900에서 실행하세요."
    }

    if ($tvncInstallOk) {
        Write-Host "      TightVNC 자동 설치 중 (포트 5900, 비밀번호 인증)..."
        $msiArgs = @(
            "/i", $TvncInstaller,
            "/quiet",
            "/norestart",
            "ADDLOCAL=Server",              # 서버 컴포넌트만 설치
            "SET_USEVNCAUTHENTICATION=1",   # VNC 비밀번호 인증 활성화
            "VALUE_OF_RFBPORT=5900",        # 포트 5900
            "SET_RFBPORT=1",
            "VALUE_OF_PASSWORD=$VncPassword",
            "SET_PASSWORD=1"
        )
        $proc = Start-Process msiexec.exe -ArgumentList $msiArgs -Wait -PassThru
        if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
            Write-Warning "TightVNC 설치 ExitCode=$($proc.ExitCode) — 수동 설치 필요"
        } else {
            Write-Host "      TightVNC 설치 완료"
        }
        Remove-Item $TvncInstaller -Force -ErrorAction SilentlyContinue
    }
}

# TightVNC 서비스 자동 시작 + 즉시 시작
$svc = Get-Service -Name $TvncService -ErrorAction SilentlyContinue
if ($svc) {
    Set-Service -Name $TvncService -StartupType Automatic
    if ($svc.Status -ne 'Running') {
        Start-Service -Name $TvncService -ErrorAction SilentlyContinue
    }
    Write-Host "      TightVNC 서비스 상태: $((Get-Service $TvncService).Status)"
} else {
    Write-Warning "TightVNC 서비스($TvncService)를 찾을 수 없습니다. 수동 설치 필요"
}

# 포트 5900은 외부에서 직접 접근 불가 — 방화벽 차단 (에이전트 중계로만 접근)
$vnc5900Rule = Get-NetFirewallRule -DisplayName "VDesk VNC 5900 Block" -ErrorAction SilentlyContinue
if (-not $vnc5900Rule) {
    New-NetFirewallRule `
        -DisplayName  "VDesk VNC 5900 Block" `
        -Direction    Inbound `
        -Protocol     TCP `
        -LocalPort    5900 `
        -Action       Block `
        -Profile      Any | Out-Null
    Write-Host "      포트 5900 외부 차단 규칙 추가 (에이전트 내부 전용)"
}

# ── 방화벽 ────────────────────────────────────────────────────────────────────
Write-Host "[4/6] 방화벽 규칙 설정: TCP 20020 inbound"
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
Write-Host "[5/6] 작업 스케줄러 등록: $TaskName"

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
Write-Host "[6/6] 에이전트 즉시 시작"
# 이미 실행 중이면 종료 후 재시작
Stop-Process -Name "vdesk_agent" -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Start-Process "$InstallDir\$ExeName"

Write-Host ""
Write-Host "✓ VDesk Agent 설치 완료"
Write-Host "  설치 경로 : $InstallDir\$ExeName"
Write-Host "  로그 파일 : $InstallDir\logs\vdesk_agent.log"
Write-Host "  자동 시작 : 로그온 시 (작업 스케줄러 '$TaskName')"
Write-Host "  방화벽    : TCP 20020 inbound 허용 / TCP 5900 외부 차단"
Write-Host "  TightVNC  : 포트 5900 (내부 전용, 에이전트가 중계)"
Write-Host ""
Write-Host "  VNC 비밀번호: $VncPassword"
Write-Host "  (환경변수 VNC_PASSWORD 로 변경 가능)"
