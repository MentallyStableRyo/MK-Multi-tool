; Windows installer for MK Multi-tool.
;
; Inno Setup rather than NSIS or MSI, for one reason: size. With all three
; speech models bundled the payload is about 2.9 GB (2.6 GB before the General
; Converter's Pandoc and PDFium were added), and both of Tauri's Windows
; targets cap below 2 GB — MSI at the 2 GB CAB format limit (`LGHT0306`, CAB
; finalization), NSIS at makensis's 32-bit file mapping (`error mmapping
; file ... is out of range`). Both were built and both failed.
;
; Build with:
;   npx tauri build --no-bundle
;   iscc installer\mk-multitool.iss                 (version from below)
;   iscc /DAppVersion=0.7.0 installer\mk-multitool.iss
;
; The install layout mirrors exactly what Tauri's own NSIS script produces, so
; `binaries.rs` resolves the sidecars and models identically either way:
;
;   <install>\mk-multitool.exe
;   <install>\ffmpeg.exe  ffprobe.exe  yt-dlp.exe  pandoc.exe   (externalBin, suffix stripped)
;   <install>\binaries\pdfium.dll                               (bundle resource — a library, not spawned)
;   <install>\binaries\whisper\*.dll  whisper-cli.exe  manifest.json
;   <install>\binaries\whisper\models\*.bin

#ifndef AppVersion
  #define AppVersion "0.6.0"
#endif

#define AppName        "MK Multi-tool"
#define AppPublisher   "MK"
#define AppExeName     "mk-multitool.exe"
#define SourceRoot     "..\src-tauri"
#define BuildRoot      SourceRoot + "\target\release"

; Fixed forever. Inno matches installs by AppId, so changing it would make the
; next version install alongside this one instead of upgrading it.
#define AppId          "{{6F3C1A84-2E77-4B19-9E5D-1C0A7F2D8B46}"

[Setup]
AppId={#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
VersionInfoVersion={#AppVersion}

DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExeName}

; Per-machine, matching the NSIS builds this replaces.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

OutputDir={#BuildRoot}\bundle\inno
OutputBaseFilename={#AppName}_{#AppVersion}_x64-setup

; The models are already compressed weights and barely shrink, but the two
; ~140 MB FFmpeg binaries and the app's own 174 MB do. Solid LZMA2 across the
; lot is worth the slower compile.
Compression=lzma2/max
SolidCompression=yes
LZMANumBlockThreads=4

; Refuse early rather than failing halfway through writing 2.6 GB.
ExtraDiskSpaceRequired=0
DisableDirPage=no
DisableProgramGroupPage=yes
WizardStyle=modern
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
; The app itself. Its frontend — including the background-removal ONNX model
; and the ONNX Runtime WASM — is embedded in this binary by Tauri, which is why
; it is ~174 MB rather than a few megabytes.
Source: "{#BuildRoot}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; Sidecars. Tauri's externalBin strips the target triple at bundle time; these
; DestName entries reproduce that, because binaries.rs looks for the bare names.
Source: "{#SourceRoot}\binaries\ffmpeg-x86_64-pc-windows-msvc.exe";  DestDir: "{app}"; DestName: "ffmpeg.exe";  Flags: ignoreversion
Source: "{#SourceRoot}\binaries\ffprobe-x86_64-pc-windows-msvc.exe"; DestDir: "{app}"; DestName: "ffprobe.exe"; Flags: ignoreversion
Source: "{#SourceRoot}\binaries\yt-dlp-x86_64-pc-windows-msvc.exe";  DestDir: "{app}"; DestName: "yt-dlp.exe";  Flags: ignoreversion

; General Converter. Pandoc is a self-contained executable, so it follows the
; same externalBin pattern as the three above. PDFium is a library dlopen'd by
; pdfium-render rather than spawned, so it is a bundle resource instead and
; keeps its own filename under binaries\, not beside the executable.
Source: "{#SourceRoot}\binaries\pandoc-x86_64-pc-windows-msvc.exe"; DestDir: "{app}"; DestName: "pandoc.exe"; Flags: ignoreversion
Source: "{#SourceRoot}\binaries\pdfium.dll"; DestDir: "{app}\binaries"; Flags: ignoreversion

; whisper.cpp travels as a directory, not a lone file: whisper-cli dlopens
; whisper.dll and picks a ggml-cpu backend at runtime from the host's
; instruction set, so all of them have to sit beside it.
Source: "{#SourceRoot}\binaries\whisper\whisper-cli.exe";  DestDir: "{app}\binaries\whisper"; Flags: ignoreversion
Source: "{#SourceRoot}\binaries\whisper\*.dll";            DestDir: "{app}\binaries\whisper"; Flags: ignoreversion
Source: "{#SourceRoot}\binaries\whisper\manifest.json";    DestDir: "{app}\binaries\whisper"; Flags: ignoreversion

; All three speech models — the reason this is an Inno installer at all.
Source: "{#SourceRoot}\binaries\whisper\models\*.bin"; DestDir: "{app}\binaries\whisper\models"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Tauri's WebView2 data directory, which the app creates next to itself at run
; time and the file list therefore does not cover.
Type: filesandordirs; Name: "{app}\EBWebView"

[Code]
const
  WEBVIEW2_GUID = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
  WEBVIEW2_URL  = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703';

{ WebView2 is present on essentially every current Windows install, but the app
  is a blank window without it. Mirrors Tauri's own downloadBootstrapper mode. }
function WebView2Installed: Boolean;
var
  Version: String;
begin
  Result :=
    RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\' + WEBVIEW2_GUID, 'pv', Version) or
    RegQueryStringValue(HKLM, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\' + WEBVIEW2_GUID, 'pv', Version) or
    RegQueryStringValue(HKCU, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\' + WEBVIEW2_GUID, 'pv', Version);
  if Result then
    Result := (Version <> '') and (Version <> '0.0.0.0');
end;

{ An install left by the earlier NSIS or MSI packages is tracked separately from
  this one, so it would sit alongside rather than be replaced. Say so plainly
  instead of leaving two entries in Add/Remove Programs. }
function LegacyInstallPresent(out DisplayName: String): Boolean;
var
  Roots: array[0..2] of Integer;
  Paths: array[0..1] of String;
  Keys: TArrayOfString;
  Name: String;
  I, J, K: Integer;
begin
  Result := False;
  Roots[0] := HKLM; Roots[1] := HKLM; Roots[2] := HKCU;
  Paths[0] := 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall';
  Paths[1] := 'SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall';

  for I := 0 to 2 do
    for J := 0 to 1 do
      if RegGetSubkeyNames(Roots[I], Paths[J], Keys) then
        for K := 0 to GetArrayLength(Keys) - 1 do
        begin
          { Skip our own entry, which Inno keys by AppId. }
          if Pos('6F3C1A84', Uppercase(Keys[K])) > 0 then
            Continue;
          if RegQueryStringValue(Roots[I], Paths[J] + '\' + Keys[K], 'DisplayName', Name) then
            if Pos('MK MULTI-TOOL', Uppercase(Name)) > 0 then
            begin
              DisplayName := Name;
              Result := True;
              Exit;
            end;
        end;
end;

function InitializeSetup: Boolean;
var
  Legacy: String;
  Downloaded: Boolean;
  ErrorCode: Integer;
begin
  Result := True;

  if LegacyInstallPresent(Legacy) then
  begin
    if MsgBox('An earlier "' + Legacy + '" is installed by a different installer format'
      + ' (MSI or NSIS), which this one cannot upgrade in place.' + #13#10#13#10
      + 'Installing now would leave two entries in Add/Remove Programs.'
      + ' Uninstall the old one first?' + #13#10#13#10
      + 'Choose No to install anyway.', mbConfirmation, MB_YESNO) = IDYES then
    begin
      Result := False;
      Exit;
    end;
  end;

  if not WebView2Installed then
  begin
    if MsgBox('This app needs the Microsoft WebView2 runtime, which is not installed.'
      + #13#10#13#10 + 'Download and install it now? (Requires an internet connection.)',
      mbConfirmation, MB_YESNO) = IDYES then
    begin
      Downloaded := False;
      try
        DownloadTemporaryFile(WEBVIEW2_URL, 'MicrosoftEdgeWebview2Setup.exe', '', nil);
        Downloaded := True;
      except
        Downloaded := False;
      end;

      if Downloaded then
        Exec(ExpandConstant('{tmp}\MicrosoftEdgeWebview2Setup.exe'), '/silent /install',
             '', SW_SHOW, ewWaitUntilTerminated, ErrorCode)
      else
        MsgBox('The WebView2 installer could not be downloaded. Install it manually'
          + ' from https://developer.microsoft.com/microsoft-edge/webview2/ and then'
          + ' run this setup again.', mbError, MB_OK);
    end;
  end;
end;
