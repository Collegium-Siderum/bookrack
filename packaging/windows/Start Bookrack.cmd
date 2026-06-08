@echo off
rem Double-click launcher for `bookrack run`.
rem
rem `bookrack.exe` is a console-subsystem binary, so double-clicking it
rem opens a console window directly. The wrapper exists so the window
rem stays open when the daemon exits with an error -- without `pause`,
rem the console closes before the operator can read the message.
cd /d "%~dp0"
bookrack.exe run
if errorlevel 1 pause
