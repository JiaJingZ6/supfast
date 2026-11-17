@echo off
rem Build the release zip: dist\ + packaging\release-readme.txt -> release\supfast-<ver>-win64.zip
rem Assumes dist\ is already assembled (run build.bat first).
setlocal enabledelayedexpansion
cd /d "%~dp0"

if not exist "dist\supfast.exe" (
  echo [x] dist\supfast.exe not found - run build.bat first.
  exit /b 1
)

set VER=1.0.0
set NAME=supfast-%VER%-win64
set STAGE=%TEMP%\supfast-rel-%RANDOM%%RANDOM%

rem --- 1. stage -------------------------------------------------------------
echo [1/3] staging %NAME%
if exist "%STAGE%" rd /s /q "%STAGE%"
mkdir "%STAGE%\%NAME%"
xcopy /e /i /q /y "dist\*" "%STAGE%\%NAME%\" >nul
if errorlevel 1 goto fail
copy /y "packaging\release-readme.txt" "%STAGE%\%NAME%\README.txt" >nul
if errorlevel 1 goto fail

rem guard: the zip must not carry any user state or leftovers
if exist "%STAGE%\%NAME%\state.json" (
  echo [x] state.json leaked into the package
  goto fail
)

rem --- 2. zip ---------------------------------------------------------------
echo [2/3] compressing
if not exist "release" mkdir "release"
if exist "release\%NAME%.zip" del /q "release\%NAME%.zip"
powershell -NoProfile -Command "Compress-Archive -Path '%STAGE%\%NAME%' -DestinationPath 'release\%NAME%.zip' -CompressionLevel Optimal"
if errorlevel 1 goto fail

rem --- 3. report ------------------------------------------------------------
echo [3/3] done
for %%F in ("release\%NAME%.zip") do (
  echo   %%~fF
  echo   %%~zF bytes
)
rd /s /q "%STAGE%"
echo.
echo Upload this file to the GitHub release page, or:
echo   git tag -a v%VER% -m "supfast %VER%" ^&^& git push origin v%VER%
exit /b 0

:fail
echo [x] failed
if exist "%STAGE%" rd /s /q "%STAGE%"
exit /b 1
