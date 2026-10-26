@echo off
setlocal

set "REPO=%~dp0.."
set "APP=%REPO%\apps\desktop-gpui"
set "EXE=%APP%\target\debug\cap-gpui.exe"
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

if /i "%~1"=="rebuild" goto rebuild

tasklist /fi "imagename eq cap-gpui.exe" | find /i "cap-gpui.exe" >nul
if not errorlevel 1 (
	echo Local Cap is already running.
	exit /b 0
)
if not exist "%EXE%" goto build
goto launch

:rebuild
tasklist /fi "imagename eq cap-gpui.exe" | find /i "cap-gpui.exe" >nul
if not errorlevel 1 (
	echo Closing the running local Cap so it can be rebuilt...
	taskkill /im cap-gpui.exe >nul
	timeout /t 2 /nobreak >nul
)

:build
echo Building local Cap. The first build takes 20+ minutes, later ones about a minute.
pushd "%APP%"
cargo build
if errorlevel 1 (
	popd
	echo.
	echo Build failed. See the errors above.
	pause
	exit /b 1
)
popd

:launch
for %%F in ("%REPO%\target\debug\*.dll") do (
	if not exist "%APP%\target\debug\%%~nxF" copy /y "%%F" "%APP%\target\debug\" >nul
)
if not exist "%REPO%\target\gpui-dev-data" mkdir "%REPO%\target\gpui-dev-data"
set "CAP_GPUI_APP_DATA_DIR=%REPO%\target\gpui-dev-data"
start "" /d "%APP%" "%EXE%"
