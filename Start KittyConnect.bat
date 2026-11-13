@echo off
rem Starts KittyConnect from this folder, the same as double-clicking
rem KittyConnect.exe. Its data (and the TailCat programs it unpacks into
rem data\bin) stay in this folder.
cd /d "%~dp0"
if not exist "%~dp0KittyConnect.exe" (
  echo KittyConnect.exe is not in this folder: %~dp0
  pause
  exit /b 1
)
start "" "%~dp0KittyConnect.exe" %*
