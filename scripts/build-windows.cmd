@echo off
setlocal

if not defined VSCMD_VER call :load_vcvars

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
exit /b 0

:load_vcvars
for %%V in (
  "%ProgramFiles(x86)%\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
  "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
  "%ProgramFiles%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
) do (
  if exist "%%~V" (
    call "%%~V" >nul
    exit /b 0
  )
)
exit /b 0
