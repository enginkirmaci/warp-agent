; Inno Setup 6 script — per-user installer for Warp Agent.
; build.ps1 -Installer invokes this with /DMyAppVersion=<cargo version>;
; the fallback below keeps plain `ISCC warp-agent.iss` working too.

#ifndef MyAppVersion
#define MyAppVersion "2.0.0"
#endif

#define MyAppName "Warp Agent"
#define MyAppExeName "warp-agent.exe"

[Setup]
AppId={{9E6B4C7A-3D2F-4E18-A7C5-1F0B9D8E6A32}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
PrivilegesRequired=lowest
ArchitecturesInstallIn64BitMode=x64
OutputDir=out
OutputBaseFilename=warp-agent-{#MyAppVersion}-setup-x64
SetupIconFile=warp-agent.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2
SolidCompression=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; \
    GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; \
    Flags: nowait postinstall skipifsilent
