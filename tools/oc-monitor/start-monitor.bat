@echo off
rem ============================================================
rem  oc-monitor - opencode serve HTTP traffic monitor
rem
rem  usage:
rem    1) start the real server:
rem         opencode serve --hostname 127.0.0.1 --port 4096
rem    2) double-click this file
rem    3) open http://127.0.0.1:5599/ in a browser
rem    4) point your client at 127.0.0.1:5599 instead of 4096
rem       (every request/response, with all headers, shows up
rem        in the dashboard - custom tracking headers included)
rem
rem  change ports here if needed:
set REALPORT=4096
set MONITORPORT=5599
rem ============================================================
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0oc-monitor.ps1" -RealPort %REALPORT% -MonitorPort %MONITORPORT%
pause
