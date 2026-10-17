; Build only from the reviewed production staging directory. No downloader.
#ifndef ProductionStage
  #error ProductionStage must point to the complete docs staging image
#endif
#ifndef OutputMedia
  #error OutputMedia must point inside docs
#endif

[Setup]
AppId={{43EC29CD-67D8-4EF5-A1FD-4D50FEAE26B1}
AppName=律金智库 M0 验证原型
AppVersion=0.1.0
AppVerName=律金智库 0.1.0 · M0
SetupArchitecture=x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={code:DefaultInstallDir}
DefaultGroupName=律金智库 M0
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
OutputDir={#OutputMedia}
OutputBaseFilename=Lvjin-M0-0.1.0-Windows-x64-Setup
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
Name: "{group}\律金智库 M0"; Filename: "{app}\lvjin.exe"; WorkingDir: "{app}"
Name: "{group}\第三方许可"; Filename: "{app}\licenses\THIRD-PARTY-NOTICES.txt"

[Run]
Filename: "{app}\lvjin.exe"; Description: "打开 M0 验证原型"; Flags: nowait postinstall skipifsilent

[Code]
function GetDriveType(lpRootPathName: String): LongWord;
  external 'GetDriveTypeW@kernel32.dll stdcall';

function DefaultInstallDir(Param: String): String;
begin
  if GetDriveType('E:\') = 3 then
    Result := 'E:\律金智库-M0'
  else
    Result := ExpandConstant('{userpf}\LvjinM0');
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  DataRoot: String;
begin
  if CurStep = ssPostInstall then begin
    DataRoot := AddBackslash(ExtractFileDir(ExpandConstant('{app}'))) + 'LvjinData-M0';
    if not ForceDirectories(DataRoot) then
      RaiseException('无法建立数据目录：' + DataRoot);
    if not SaveStringToFile(ExpandConstant('{app}\data-root.txt'), Utf8Encode(DataRoot), False) then
      RaiseException('无法保存数据目录设置');
  end;
end;
