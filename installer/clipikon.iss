; Inno Setup 6 script for the per-user Clipikon installer (no administrator rights needed).
; Build after `cargo build --release`:  iscc /DAppVersion=0.1.0 installer\clipikon.iss
; The setup lands in target\installer.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
; Never change AppId: updates and the uninstaller find an existing install by it.
AppId={{79A0CA6E-5C6F-4A32-98D3-2FA3D248821B}
AppName=Clipikon
AppVersion={#AppVersion}
AppVerName=Clipikon {#AppVersion}
AppPublisher=Notterox
AppPublisherURL=https://github.com/Notterox/clipikon
AppSupportURL=https://github.com/Notterox/clipikon/issues
VersionInfoVersion={#AppVersion}
PrivilegesRequired=lowest
; Per-user install: {autopf} is %LOCALAPPDATA%\Programs.
DefaultDirName={autopf}\Clipikon
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\clipikon.exe
; Setup and the uninstaller ask the user to quit a running Clipikon first.
AppMutex=Clipikon.SingleInstance
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\target\installer
OutputBaseFilename=Clipikon-Setup-{#AppVersion}

[Tasks]
; Asked on the first install only: an update keeps what the tray menu's "Launch at startup" says now.
Name: startup; Description: "Launch Clipikon when Windows starts"; Check: not IsUpgrade
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\clipikon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"
Source: "..\ffmpeg-build\out\FFmpeg-LICENSE.txt"; DestDir: "{app}\licenses"
Source: "..\licenses\LGPL-3.0.txt"; DestDir: "{app}\licenses"; DestName: "FSRCNNX-LGPL-3.0.txt"

[Icons]
Name: "{autoprograms}\Clipikon"; Filename: "{app}\clipikon.exe"
Name: "{autodesktop}\Clipikon"; Filename: "{app}\clipikon.exe"; Tasks: desktopicon

[Registry]
; The same value the tray menu's "Launch at startup" writes (src/autostart.rs).
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Clipikon"; ValueData: """{app}\clipikon.exe"""; Tasks: startup; Check: not IsUpgrade
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Clipikon"; Flags: deletevalue; Tasks: not startup; Check: not IsUpgrade
; Removed on uninstall however it was set, along with Task Manager's record of it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Clipikon"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"; ValueType: none; ValueName: "Clipikon"; Flags: uninsdeletevalue

[Run]
; On a first install Clipikon then shows how to record, once.
Filename: "{app}\clipikon.exe"; Parameters: "{code:StartParams}"; Description: "Start Clipikon"; Flags: nowait postinstall skipifsilent
; Clipikon's "Check for updates" runs setup with /SILENT: start it again afterwards.
Filename: "{app}\clipikon.exe"; Flags: nowait; Check: WizardSilent

[Code]
var
  Upgrade: Boolean;

function InitializeSetup: Boolean;
begin
  Upgrade := RegValueExists(HKCU, ExpandConstant('Software\Microsoft\Windows\CurrentVersion\Uninstall\{#SetupSetting("AppId")}_is1'), 'UninstallString');
  Result := True;
end;

function IsUpgrade: Boolean;
begin
  Result := Upgrade;
end;

function StartParams(Param: String): String;
begin
  if Upgrade then
    Result := ''
  else
    Result := '--installed';
end;
