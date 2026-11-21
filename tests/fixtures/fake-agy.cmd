@echo off
setlocal
set "d=%~dp0"

if "%~1"=="--version" (
  echo agy 1.5.9
  exit /b 0
)

if "%~1"=="-p" if "%~2"=="/usage" goto do_usage
if "%~1"=="models" goto do_models
exit /b 0

:do_usage
echo x>>"%d%usage.calls"
if exist "%d%usage.sleep" goto sleep_usage
if exist "%d%usage.out" type "%d%usage.out"
if exist "%d%usage.err" type "%d%usage.err" 1>&2
if not exist "%d%usage.code" exit /b 0
set /p code=<"%d%usage.code"
exit /b %code%

:sleep_usage
set /p secs=<"%d%usage.sleep"
set /a pings=secs+1
ping -n %pings% 127.0.0.1 >nul
exit /b 0

:do_models
echo x>>"%d%models.calls"
if exist "%d%models.sleep" goto sleep_models
if exist "%d%models.out" type "%d%models.out"
if exist "%d%models.err" type "%d%models.err" 1>&2
if not exist "%d%models.code" exit /b 0
set /p code=<"%d%models.code"
exit /b %code%

:sleep_models
set /p secs=<"%d%models.sleep"
set /a pings=secs+1
ping -n %pings% 127.0.0.1 >nul
exit /b 0
