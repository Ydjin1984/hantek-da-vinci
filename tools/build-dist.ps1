# Собирает всё, что отдаётся пользователю, в папку dist/.
#
#   pwsh tools\build-dist.ps1              сборка (интерфейс берётся готовый)
#   pwsh tools\build-dist.ps1 -BuildUi     сначала пересобрать интерфейс
#
# Порядок важен: установщик вкладывает в себя готовый davinci.exe, поэтому
# программа собирается первой. Оба шага — обычные cargo-вызовы, никаких
# внешних установщиков и упаковщиков не нужно.
param(
    [switch]$BuildUi
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$version = '0.1.0'
$dist = Join-Path $root 'dist'

Push-Location $root
try {
    if ($BuildUi) {
        Write-Host '== интерфейс ==' -ForegroundColor Cyan
        Push-Location (Join-Path $root 'ui')
        if (-not (Test-Path 'node_modules')) { npm ci --no-audit --no-fund }
        npm run build
        Pop-Location
    }

    if (-not (Test-Path (Join-Path $root 'ui\dist\index.html'))) {
        throw 'Интерфейс не собран: запустите pwsh tools\build-dist.ps1 -BuildUi'
    }

    Write-Host '== программа ==' -ForegroundColor Cyan
    cargo build --release -p davinci
    if ($LASTEXITCODE -ne 0) { throw "cargo build -p davinci: код $LASTEXITCODE" }

    Write-Host '== установщик ==' -ForegroundColor Cyan
    cargo build --release --manifest-path (Join-Path $root 'crates\setup\Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "cargo build установщика: код $LASTEXITCODE" }

    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    $app = Join-Path $root 'target\release\davinci.exe'
    $setup = Join-Path $root 'crates\setup\target\release\davinci-setup.exe'

    Copy-Item $app (Join-Path $dist 'DaVinci.exe') -Force
    Copy-Item $setup (Join-Path $dist "DaVinci-Setup.exe") -Force

    Write-Host ''
    Write-Host '== готово ==' -ForegroundColor Green
    Get-ChildItem $dist | Sort-Object Name | ForEach-Object {
        '{0,-28} {1,8:N0} КБ' -f $_.Name, ($_.Length / 1KB)
    }
    Write-Host ''
    Write-Host 'Проверка без установки:  dist\DaVinci.exe doctor'
    Write-Host 'Установка:               dist\DaVinci-Setup.exe'
}
finally {
    Pop-Location
}
