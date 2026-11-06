@echo off
setlocal
cd /d "%~dp0"
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
rem MSVC Rust needs the Visual Studio Desktop development with C++ workload.
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" goto build
for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSDIR=%%i"
if defined VSDIR call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>nul
:build
cargo build --release --locked || exit /b 1
cargo test --release --locked || exit /b 1
if not exist build mkdir build
copy /y target\release\SimpleAudioSplit.exe build\SimpleAudioSplit.exe >nul || exit /b 1
copy /y target\release\sas-cli.exe build\sas-cli.exe >nul || exit /b 1
echo Built build\SimpleAudioSplit.exe and build\sas-cli.exe
