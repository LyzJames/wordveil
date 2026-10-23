; M3 internal build. Build only from the reviewed production staging image.
; The installer places every file offline; it downloads nothing.
#ifndef ProductionStage
  #error ProductionStage must point to the complete docs staging image
#endif
#ifndef OutputMedia
  #error OutputMedia must point inside docs
#endif

[Setup]
AppId={{E4C4DA8F-C626-4069-B634-1EA352AE1A17}
AppName=律金智库
AppVersion=0.4.0
AppVerName=律金智库 0.4.0 · M3 内部版
AppPublisher=律金智库（个人首测）
SetupArchitecture=x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={userpf}\Lvjin
DefaultGroupName=律金智库
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
OutputDir={#OutputMedia}
OutputBaseFilename=Lvjin-M3-0.4.0-Windows-x64-Setup
LicenseFile={#ProductionStage}\licenses\THIRD-PARTY-NOTICES.txt
Compression=zip/3
SolidCompression=no
DiskSpanning=yes
DiskSliceSize=2000000000
SlicesPerDisk=1
ExtraDiskSpaceRequired=8589934592
WizardStyle=modern
UninstallDisplayIcon={app}\lvjin.exe
CloseApplications=yes
RestartApplications=no
SetupLogging=yes
MinVersion=10.0.22000
AllowNoIcons=yes

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Files]
Source: "{#ProductionStage}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\律金智库"; Filename: "{app}\lvjin.exe"; WorkingDir: "{app}"
Name: "{group}\第三方许可"; Filename: "{app}\licenses\THIRD-PARTY-NOTICES.txt"

[Run]
Filename: "{app}\lvjin.exe"; Description: "打开律金智库"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only files the installer wrote. Private spaces under the data directory are
; never removed by uninstalling; the user deletes them deliberately.
Type: files; Name: "{app}\data-root.txt"

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
var
  DataRoot: String;
begin
  if CurStep = ssPostInstall then begin
    // Per-user application data, outside the installation directory.
    DataRoot := ExpandConstant('{localappdata}\Lvjin\data');
    if not ForceDirectories(DataRoot) then
      RaiseException('无法建立数据目录：' + DataRoot);
    if not SaveStringToFile(ExpandConstant('{app}\data-root.txt'), Utf8Encode(DataRoot), False) then
      RaiseException('无法保存数据目录设置');
  end;
end;
