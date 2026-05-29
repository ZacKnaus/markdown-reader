@echo off
setlocal

set "PROFILE=debug"
set "CARGO_ARGS=build"

if /I "%~1"=="--release" (
  set "PROFILE=release"
  set "CARGO_ARGS=build --release"
)

cargo %CARGO_ARGS%
if errorlevel 1 exit /b %errorlevel%

set "PROJECT_ROOT=%~dp0.."
set "SOURCE_EXE=%PROJECT_ROOT%\target\%PROFILE%\markdown-reader.exe"
set "FRIENDLY_EXE=%PROJECT_ROOT%\target\%PROFILE%\Markdown Reader.exe"

copy /Y "%SOURCE_EXE%" "%FRIENDLY_EXE%" >nul
if errorlevel 1 exit /b %errorlevel%

echo Built: %SOURCE_EXE%
echo Friendly Open With copy: %FRIENDLY_EXE%
