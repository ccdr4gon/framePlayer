# Build/run wrapper: activates the MSVC dev env (sets INCLUDE/LIB for bindgen), then runs cargo.
# Usage:  .\build.ps1 build      .\build.ps1 run      .\build.ps1 build --release
# ALWAYS build through this script so the env stays consistent and ffmpeg-sys is not
# needlessly regenerated (env changes invalidate its build).
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vcvars = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find "VC\Auxiliary\Build\vcvars64.bat") | Select-Object -First 1
if (-not $vcvars) { Write-Error "vcvars64.bat not found (need VS Build Tools C++ component)"; exit 1 }
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    $i = $_.IndexOf('=')
    if ($i -gt 0) { [Environment]::SetEnvironmentVariable($_.Substring(0, $i), $_.Substring($i + 1), 'Process') }
}
Set-Location $PSScriptRoot
cargo @args
exit $LASTEXITCODE
