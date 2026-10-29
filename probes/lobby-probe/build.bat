@echo off
rem Build lobby-probe and collect the exe and SDK DLLs into out\
setlocal
set "HERE=%~dp0"
set "SDK=%HERE%..\..\third_party\sdk-1.10.19337\discord_social_sdk"
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
for /f "usebackq delims=" %%i in (`call "%VSWHERE%" -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VS=%%i"
call "%VS%\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
if not exist "%HERE%out" mkdir "%HERE%out"
cl /nologo /std:c++20 /EHsc /O2 /utf-8 /W3 /I"%SDK%\include" "%HERE%lobby_probe.cpp" /Fo"%HERE%out\\" /Fe"%HERE%out\lobby-probe.exe" /link "%SDK%\lib\release\discord_partner_sdk.lib" || exit /b 1
copy /y "%SDK%\bin\release\*.dll" "%HERE%out\" >nul
copy /y "%SDK%\bin\release\*.kef" "%HERE%out\" >nul
echo built: %HERE%out\lobby-probe.exe
