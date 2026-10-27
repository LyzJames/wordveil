; 0.6.0 offline installer. Build from the reviewed production staging image.
; The installer places every file offline; it downloads nothing.
#ifndef ProductionStage
  #error ProductionStage must point to the complete docs staging image
#endif
#ifndef OutputMedia
  #error OutputMedia must point inside docs
#endif

[Setup]
AppId={{E4C4DA8F-C626-4069-B634-1EA352AE1A17}
AppName=守语
AppVersion=0.6.0
AppVerName=守语 0.6.0
AppPublisher=守语（个人首测）
SetupArchitecture=x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={userpf}\Lvjin
DefaultGroupName=守语
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
OutputDir={#OutputMedia}
OutputBaseFilename=Shouyu-0.6.0-Windows-x64-Setup
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

[InstallDelete]
Type: files; Name: "{group}\律金智库.lnk"
; Replace only managed program resources. Removed dependency metadata/DLLs from
; earlier full releases must not remain importable beside the new worker.
; Replace all managed resources to retire old specialist skills and weights.
; PrepareToInstall refuses a private-data pointer inside the program directory.
Type: filesandordirs; Name: "{app}\resources"
Type: files; Name: "{app}\lvjin-public-mcp.exe"
Type: files; Name: "{app}\SBOM.cdx.json"
Type: filesandordirs; Name: "{app}\runtimes\worker"
Type: filesandordirs; Name: "{app}\runtimes\webview2"
Type: filesandordirs; Name: "{app}\licenses"

[Icons]
Name: "{group}\守语"; Filename: "{app}\lvjin.exe"; WorkingDir: "{app}"
Name: "{group}\第三方许可"; Filename: "{app}\licenses\THIRD-PARTY-NOTICES.txt"

[Run]
Filename: "{app}\lvjin.exe"; Description: "打开守语"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only files the installer wrote. Private spaces under the data directory are
; never removed by uninstalling; the user deletes them deliberately.
Type: files; Name: "{app}\data-root.txt"

[Code]
function ManagedTreeHasLink(const Root: String): Boolean;
var Rec: TFindRec;
begin
  Result := False;
  if FindFirst(Root, Rec) then begin
    try
      Result := (Rec.Attributes and FILE_ATTRIBUTE_REPARSE_POINT) <> 0;
    finally
      FindClose(Rec);
    end;
  end;
  if Result then Exit;
  if FindFirst(AddBackslash(Root) + '*', Rec) then begin
    try
      repeat
        if (Rec.Name <> '.') and (Rec.Name <> '..') then begin
          if (Rec.Attributes and FILE_ATTRIBUTE_REPARSE_POINT) <> 0 then begin
            Result := True;
            Exit;
          end;
          if (Rec.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then begin
            if ManagedTreeHasLink(AddBackslash(Root) + Rec.Name) then begin
              Result := True;
              Exit;
            end;
          end;
        end;
      until not FindNext(Rec);
    finally
      FindClose(Rec);
    end;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Raw: AnsiString;
  DataPath, AppPath: String;
begin
  Result := '';
  if ManagedTreeHasLink(ExpandConstant('{app}\resources')) or ManagedTreeHasLink(ExpandConstant('{app}\runtimes')) or ManagedTreeHasLink(ExpandConstant('{app}\licenses')) then begin
    Result := '现有程序资源包含目录联接或符号链接；为保护链接外的数据，安装已停止。';
    Exit;
  end;
  if FileExists(ExpandConstant('{app}\data-root.txt')) then begin
    if not LoadStringFromFile(ExpandConstant('{app}\data-root.txt'), Raw) then begin
      Result := '无法读取现有数据位置；为保留原数据，安装已停止。';
      Exit;
    end;
    DataPath := Trim(Utf8Decode(Raw));
    if (Length(DataPath) < 3) or ((Copy(DataPath, 1, 2) <> '\\') and (Copy(DataPath, 2, 2) <> ':\') and (Copy(DataPath, 2, 2) <> ':/')) then begin
      Result := '现有数据位置不是有效绝对路径；安装已停止。';
      Exit;
    end;
    DataPath := RemoveBackslashUnlessRoot(ExpandFileName(DataPath));
    AppPath := RemoveBackslashUnlessRoot(ExpandFileName(ExpandConstant('{app}')));
    if (DataPath = '') or (CompareText(DataPath, AppPath) = 0) or
       (CompareText(Copy(AddBackslash(DataPath), 1, Length(AddBackslash(AppPath))), AddBackslash(AppPath)) = 0) then
      Result := '现有私密数据位于程序目录内；请先将数据迁移到程序目录以外，再升级。安装不会删除这些数据。';
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  DataRoot: String;
begin
  if CurStep = ssPostInstall then begin
    // A prior installation may explicitly point at a migrated private space.
    // Preserve that setting during a full offline upgrade/reinstall.
    if FileExists(ExpandConstant('{app}\data-root.txt')) then
      Exit;
    // Per-user application data, outside the installation directory.
    DataRoot := ExpandConstant('{localappdata}\Lvjin\data');
    if not ForceDirectories(DataRoot) then
      RaiseException('无法建立数据目录：' + DataRoot);
    if not SaveStringToFile(ExpandConstant('{app}\data-root.txt'), Utf8Encode(DataRoot), False) then
      RaiseException('无法保存数据目录设置');
  end;
end;
