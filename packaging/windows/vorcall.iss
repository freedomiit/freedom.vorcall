; Per-user installer for the Vorcall client, compiled by Inno Setup 6 on the
; Windows runner (release.yml). It installs under the account's local programs
; folder with no admin prompt, which is also what the in-app updater needs: it
; replaces vorcall.exe in place, and a Program Files install would push it into
; its manual fallback. The bare vorcall-windows-x86_64.exe stays the updater's
; download.
;
; iscc /DAppVersion=X.Y.Z /DSourceDir=<dir holding vorcall-windows-x86_64.exe>
;      /DOutputDir=<dir> /DIconFile=<vorcall.ico> vorcall.iss

#ifndef AppVersion
  #error AppVersion is required: /DAppVersion=X.Y.Z
#endif
#ifndef SourceDir
  #error SourceDir is required: /DSourceDir=<dir holding vorcall-windows-x86_64.exe>
#endif
#ifndef OutputDir
  #error OutputDir is required: /DOutputDir=<dir>
#endif
#ifndef IconFile
  #error IconFile is required: /DIconFile=<vorcall.ico>
#endif

[Setup]
; never change: it is how Windows tells an upgrade of Vorcall from a new program
AppId={{AF97CCD5-E15F-4644-A2DC-9A3095996CE4}
AppName=Vorcall
AppVersion={#AppVersion}
AppVerName=Vorcall {#AppVersion}
AppPublisher=freedomit
VersionInfoVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\Vorcall
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=vorcall-windows-x86_64-setup
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\vorcall.ico
UninstallDisplayName=Vorcall
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; a running Vorcall holds vorcall.exe: ask to close it instead of failing
CloseApplications=yes
RestartApplications=no
ShowLanguageDialog=auto

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#SourceDir}\vorcall-windows-x86_64.exe"; DestDir: "{app}"; DestName: "vorcall.exe"; Flags: ignoreversion
Source: "{#IconFile}"; DestDir: "{app}"; DestName: "vorcall.ico"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Vorcall"; Filename: "{app}\vorcall.exe"; IconFilename: "{app}\vorcall.ico"
Name: "{autodesktop}\Vorcall"; Filename: "{app}\vorcall.exe"; IconFilename: "{app}\vorcall.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\vorcall.exe"; Description: "{cm:LaunchProgram,Vorcall}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; what the updater leaves next to the binary: the copy a swap moved aside, and downloads
Type: files; Name: "{app}\vorcall.exe.old"
Type: files; Name: "{app}\.vorcall-update-*"
