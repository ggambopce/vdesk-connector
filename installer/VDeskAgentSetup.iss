[Setup]
AppName=VDesk Agent
AppVersion=1.0
AppPublisher=VDesk
DefaultDirName={commonpf64}\VDesk
OutputDir=Output
OutputBaseFilename=VDesk-Agent-Setup
Compression=lzma2/ultra64
SolidCompression=yes
PrivilegesRequired=admin
DisableProgramGroupPage=yes
ShowLanguageDialog=no
UninstallDisplayIcon={app}\vdesk_agent.exe
SetupIconFile=
; 설치 완료 후 앱 실행 체크박스 없음 (백그라운드 SYSTEM 서비스로 자동 실행)
DisableFinishedPage=no

[Files]
; Rust 릴리스 빌드 결과물
Source: "..\target\release\vdesk_agent.exe"; DestDir: "{app}"; Flags: ignoreversion
; TightVNC — installer/ 폴더에 미리 복사 필요 (build_installer.ps1 참조)
Source: "TightVNC.msi"; DestDir: "{tmp}"; Flags: deleteafterinstall
; 설치 스크립트 (TightVNC 설치 + Task Scheduler 등록)
Source: "..\install_agent.ps1"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Code]
var
  ApiUrlPage: TInputQueryWizardPage;

procedure InitializeWizard;
var
  apiUrl: String;
begin
  // /ApiUrl= 커맨드라인 파라미터 지원 (사일런트 배포: /VERYSILENT /ApiUrl=https://...)
  apiUrl := ExpandConstant('{param:ApiUrl|https://vdesk.co.kr}');

  ApiUrlPage := CreateInputQueryPage(wpWelcome,
    'VDesk 서버 설정',
    '에이전트가 연결할 VDesk 서버 URL을 입력하세요.',
    '');
  ApiUrlPage.Add('서버 URL:', False);
  ApiUrlPage.Values[0] := apiUrl;
end;

function GetApiUrl(Param: String): String;
begin
  Result := ApiUrlPage.Values[0];
end;

[Run]
; install_agent.ps1 실행 — TightVNC 설치 + 방화벽 + Task Scheduler(SYSTEM) 등록 + 즉시 시작
Filename: "powershell.exe";
Parameters: "-ExecutionPolicy Bypass -WindowStyle Hidden -File ""{tmp}\install_agent.ps1"" -ApiUrl ""{code:GetApiUrl}"" -VncPassword ""1234""";
Flags: runhidden waituntilterminated;
StatusMsg: "에이전트 및 TightVNC 설치 중... (1-2분 소요)"

[UninstallRun]
; 언인스톨 시 Task Scheduler 등록 해제
Filename: "powershell.exe";
Parameters: "-ExecutionPolicy Bypass -WindowStyle Hidden -Command ""Stop-Process -Name vdesk_agent -Force -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName VDeskAgent -Confirm:$false -ErrorAction SilentlyContinue""";
Flags: runhidden
