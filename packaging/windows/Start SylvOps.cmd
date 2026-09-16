@echo off
setlocal
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0Start-SylvOps.ps1" %*
if errorlevel 1 (
  echo.
  echo SylvOps could not start. Review the error above.
  pause
)
