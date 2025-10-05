@echo off
echo Starting portable React app...
echo.
echo Opening browser in 3 seconds...
echo Press Ctrl+C to stop the server
echo.

cd /d "%~dp0"

REM Check if serve is installed
if not exist "node_modules\serve" (
    echo Installing serve package...
    node-portable\npm.cmd install serve
    echo.
)

REM Start browser after delay
start /min cmd /c "timeout /t 3 /nobreak >nul && start http://localhost:3000"

REM Start the server using npx to ensure correct path resolution
node-portable\node node_modules\serve\build\main.js -s build -p 3000

pause