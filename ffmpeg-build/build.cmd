@echo off
rem Builds the FFmpeg libraries that are linked into clipikon.exe (see build.sh).
rem Needs Visual Studio with the C++ workload, and w64devkit (for sh, make and nasm): either on PATH
rem or with W64DEVKIT set to its folder.
setlocal
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath > "%TEMP%\clipikon-vs.txt"
set /p VS=<"%TEMP%\clipikon-vs.txt"
if not defined VS (echo Visual Studio with C++ tools not found. & exit /b 1)
call "%VS%\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
rem After the VS entries, so MSVC's link.exe wins over coreutils' link.
if defined W64DEVKIT set "PATH=%PATH%;%W64DEVKIT%\bin"
sh "%~dp0build.sh"
