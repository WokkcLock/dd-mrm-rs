@echo off
rem Build release and assemble dist\ (dd-mrm.exe + DdHotapply.dll)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\dist.ps1" %*
