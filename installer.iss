; Inno Setup script. Build the release exe first, then:
;   ISCC.exe installer.iss
; The version comes from the exe (Cargo.toml -> winresource -> ProductVersion).

#define AppExe "target\release\dyndns.exe"
#define AppVersion GetStringFileInfo(AppExe, "ProductVersion")

[Setup]
AppId={{6B0E2C7A-4F3D-4C1B-9E2A-7D5F1A8C3B42}
AppName=DynDNS Updater
AppVersion={#AppVersion}
AppPublisher=pinkyBOZZ
AppPublisherURL=https://github.com/pinkyBOZZ/DynDNS-Updater
DefaultDirName={autopf}\DynDNS Updater
DisableProgramGroupPage=yes
LicenseFile=LICENSE
OutputDir=dist
OutputBaseFilename=DynDNS-Updater-{#AppVersion}-setup
SetupIconFile=assets\icon.ico
UninstallDisplayIcon={app}\dyndns.exe
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
; The app hides in the tray when its window closes, so it has to be closed by force.
CloseApplications=force

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\DynDNS Updater"; Filename: "{app}\dyndns.exe"

[Run]
Filename: "{app}\dyndns.exe"; Description: "{cm:LaunchProgram,DynDNS Updater}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\dyndns.exe"; Parameters: "--uninstall-service"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

[Code]
var
  ServiceStopped: Boolean;

function ServiceIsStopped(): Boolean;
var
  Code: Integer;
begin
  // find exits with 0 when the text is there.
  Result := Exec(ExpandConstant('{cmd}'), '/C sc query DynDNSUpdater | find "STOPPED"', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

// The service keeps dyndns.exe locked, so stop it before files are replaced on an upgrade.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code, I: Integer;
begin
  if Exec(ExpandConstant('{sys}\sc.exe'), 'stop DynDNSUpdater', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0) then
  begin
    ServiceStopped := True;
    // A sync in progress can take up to 40 s before the service sees the stop.
    for I := 1 to 60 do
    begin
      if ServiceIsStopped() then Break;
      Sleep(1000);
    end;
  end;
  Result := '';
end;

// Runs at the very end, also when setup fails or is cancelled, so a service we stopped
// never stays stopped.
procedure DeinitializeSetup();
var
  Code: Integer;
begin
  if ServiceStopped then
    Exec(ExpandConstant('{sys}\sc.exe'), 'start DynDNSUpdater', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;
