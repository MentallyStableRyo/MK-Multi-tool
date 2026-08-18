@echo off
setlocal

rem Builds the MSI. Run `npx tauri build --no-bundle` first.
rem
rem Uses the WiX 3.14 toolset Tauri already downloads for its own MSI target,
rem so there is nothing extra to install.
rem
rem   installer\build-msi.cmd            builds the version below
rem   installer\build-msi.cmd 0.6.0      builds that version

set "VERSION=%~1"
if "%VERSION%"=="" set "VERSION=0.6.0"

set "WIX=%LOCALAPPDATA%\tauri\WixTools314"
set "ROOT=%~dp0.."
set "SRC=%ROOT%\src-tauri"
set "BUILD=%SRC%\target\release"
set "OUT=%BUILD%\bundle\msi"

if not exist "%WIX%\candle.exe" (
  echo ERROR: WiX not found at %WIX%
  echo Run `npx tauri build` once to let Tauri download the toolset.
  exit /b 1
)
if not exist "%BUILD%\mk-multitool.exe" (
  echo ERROR: %BUILD%\mk-multitool.exe is missing.
  echo Run `npx tauri build --no-bundle` first.
  exit /b 1
)

if not exist "%OUT%" mkdir "%OUT%"

echo Compiling WiX source...
"%WIX%\candle.exe" -nologo -arch x64 ^
  -dVersion=%VERSION% ^
  -dSourceDir="%SRC%" ^
  -dBuildDir="%BUILD%" ^
  -ext WixUIExtension ^
  -out "%BUILD%\wix\mk-multitool.wixobj" ^
  "%~dp0mk-multitool.wxs"
if errorlevel 1 exit /b 1

echo Linking MSI (this compresses ~2.6 GB into four cabinets; expect several minutes)...
"%WIX%\light.exe" -nologo ^
  -ext WixUIExtension ^
  -cultures:en-us ^
  -sice:ICE61 ^
  -out "%OUT%\MK Multi-tool_%VERSION%_x64_en-US.msi" ^
  "%BUILD%\wix\mk-multitool.wixobj"
if errorlevel 1 exit /b 1

echo.
echo Done: %OUT%\MK Multi-tool_%VERSION%_x64_en-US.msi
