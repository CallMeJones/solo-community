#define AppName "Solo"
#ifndef AppVersion
#define AppVersion "0.0.0-dev"
#endif
#ifndef SourceDir
#define SourceDir "..\..\target\x86_64-pc-windows-msvc\release"
#endif
#ifndef OutputDir
#define OutputDir "."
#endif

; v0.11.7: detect whether solo-tray.exe was built into SourceDir at
; installer-compile time. Inno preprocessor's FileExists is a
; compile-time check, so the gate flips ON only when the release build
; included the tray. Wraps the [Files] / [Icons] / [Registry] / [Run]
; entries that reference solo-tray.exe — so older release branches
; (or manual builds without -p solo-tray) still compile the installer
; cleanly with no broken shortcuts or registry entries.
#if FileExists(SourceDir + "\solo-tray.exe")
  #define HasTray 1
#else
  #define HasTray 0
#endif

[Setup]
AppId={{C88B8E7B-F47F-4B90-B3D5-4C3498E51913}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=CallMeJones
AppPublisherURL=https://github.com/CallMeJones/solo-community
AppSupportURL=https://github.com/CallMeJones/solo-community/issues
AppUpdatesURL=https://github.com/CallMeJones/solo-community/releases/latest
DefaultDirName={localappdata}\Programs\Solo
DefaultGroupName=Solo
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=SoloSetup-{#AppVersion}-x86_64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
UninstallDisplayIcon={app}\solo.exe
SetupIconFile=solo.ico
SetupLogging=yes

#if HasTray
[Tasks]
; Optional autostart-on-login for solo-tray. The tray itself can also
; toggle this at runtime via its menu (HKCU\...\Run); this checkbox
; just pre-installs the entry so users don't need a second step.
; Unchecked by default — installing the tray binary doesn't imply
; they want it autostarted.
Name: "trayautostart"; Description: "Start Solo Controls on login (recommended)"; GroupDescription: "Optional:"; Flags: unchecked
#endif

[Files]
Source: "{#SourceDir}\solo.exe"; DestDir: "{app}"; Flags: ignoreversion
#if HasTray
Source: "{#SourceDir}\solo-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif
Source: "{#SourceDir}\*.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#SourceDir}\models\*"; DestDir: "{app}\models"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "README-WINDOWS.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "skills\*"; DestDir: "{app}\skills"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Solo PowerShell"; Filename: "powershell.exe"; Parameters: "-NoExit -Command ""Set-Location -LiteralPath '{app}'; .\solo.exe --help"""; WorkingDir: "{app}"
#if HasTray
Name: "{group}\Solo Controls"; Filename: "{app}\solo-tray.exe"; WorkingDir: "{app}"
#endif
Name: "{group}\Solo README"; Filename: "notepad.exe"; Parameters: """{app}\README-WINDOWS.txt"""
Name: "{group}\Uninstall Solo"; Filename: "{uninstallexe}"

#if HasTray
[Registry]
; Write the autostart Run-key entry when the user ticks
; "Start Solo Controls on login". `Flags: uninsdeletevalue` removes the
; value on uninstall. Mirrors what solo-tray would write at runtime
; if the user toggled autostart from its tray menu.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: string; ValueName: "Solo Controls"; \
    ValueData: """{app}\solo-tray.exe"""; \
    Flags: uninsdeletevalue; Tasks: trayautostart
; Remove the pre-rename autostart value so Windows Startup Apps does
; not show both Solo Controls and the old Solo Tray entry.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: none; ValueName: "Solo Tray"; Flags: deletevalue
#endif

[Run]
Filename: "{app}\solo.exe"; Parameters: "--help"; Description: "Show Solo help"; Flags: nowait postinstall skipifsilent unchecked
#if HasTray
; Optional "Launch Solo Controls now" checkbox on the install-complete
; page. Unchecked by default; user opts in if they want.
Filename: "{app}\solo-tray.exe"; Description: "Launch Solo Controls"; \
    Flags: nowait postinstall skipifsilent unchecked
#endif

[Code]
const
  EnvironmentKey = 'Environment';
  PathValueName = 'Path';
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  WM_SETTINGCHANGE = $001A;
  SMTO_ABORTIFHUNG = $0002;

function SendMessageTimeout(hWnd: Longint; Msg: Longint; wParam: Longint;
  lParam: string; fuFlags: Longint; uTimeout: Longint;
  var lpdwResult: Longint): Longint;
  external 'SendMessageTimeoutW@user32.dll stdcall';

function StripTrailingBackslash(Value: string): string;
begin
  Result := Value;
  while (Length(Result) > 3) and (Copy(Result, Length(Result), 1) = '\') do
    Delete(Result, Length(Result), 1);
end;

function SamePath(Left, Right: string): Boolean;
begin
  Result := Lowercase(StripTrailingBackslash(Left)) =
    Lowercase(StripTrailingBackslash(Right));
end;

function GetUserPath(): string;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, PathValueName, Result) then
    Result := '';
end;

function PathContains(Dir: string): Boolean;
var
  PathValue: string;
  Part: string;
  PosSemi: Integer;
begin
  Result := False;
  PathValue := GetUserPath();

  while PathValue <> '' do
  begin
    PosSemi := Pos(';', PathValue);
    if PosSemi = 0 then
    begin
      Part := PathValue;
      PathValue := '';
    end
    else
    begin
      Part := Copy(PathValue, 1, PosSemi - 1);
      Delete(PathValue, 1, PosSemi);
    end;

    if SamePath(Part, Dir) then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

function NeedsAddPath(Dir: string): Boolean;
begin
  Result := not PathContains(Dir);
end;

function AddToPath(Dir: string): string;
var
  PathValue: string;
begin
  PathValue := GetUserPath();
  if PathValue = '' then
    Result := Dir
  else
    Result := PathValue + ';' + Dir;
end;

function RemoveFromPath(PathValue, Dir: string): string;
var
  Part: string;
  PosSemi: Integer;
begin
  Result := '';

  while PathValue <> '' do
  begin
    PosSemi := Pos(';', PathValue);
    if PosSemi = 0 then
    begin
      Part := PathValue;
      PathValue := '';
    end
    else
    begin
      Part := Copy(PathValue, 1, PosSemi - 1);
      Delete(PathValue, 1, PosSemi);
    end;

    if (Part <> '') and (not SamePath(Part, Dir)) then
    begin
      if Result = '' then
        Result := Part
      else
        Result := Result + ';' + Part;
    end;
  end;
end;

procedure BroadcastEnvironmentChanged();
var
  ResultCode: Longint;
begin
  SendMessageTimeout(HWND_BROADCAST, WM_SETTINGCHANGE, 0,
    'Environment', SMTO_ABORTIFHUNG, 5000, ResultCode);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  InstallDir: string;
begin
  if CurStep = ssPostInstall then
  begin
    InstallDir := ExpandConstant('{app}');
    if NeedsAddPath(InstallDir) then
    begin
      RegWriteExpandStringValue(HKCU, EnvironmentKey, PathValueName,
        AddToPath(InstallDir));
      BroadcastEnvironmentChanged();
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  InstallDir: string;
  PathValue: string;
  NewPathValue: string;
begin
  { Remove both the current and pre-rename startup values even when the
    installer task never created them. The tray can create either value at
    runtime, so task-scoped [Registry] cleanup alone is insufficient. }
  if CurUninstallStep = usUninstall then
  begin
    RegDeleteValue(HKCU, RunKey, 'Solo Controls');
    RegDeleteValue(HKCU, RunKey, 'Solo Tray');
  end;

  if CurUninstallStep = usPostUninstall then
  begin
    InstallDir := ExpandConstant('{app}');
    PathValue := GetUserPath();
    NewPathValue := RemoveFromPath(PathValue, InstallDir);
    if NewPathValue <> PathValue then
    begin
      RegWriteExpandStringValue(HKCU, EnvironmentKey, PathValueName,
        NewPathValue);
      BroadcastEnvironmentChanged();
    end;
  end;
end;
