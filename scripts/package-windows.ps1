$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw '请在 Windows 上运行此脚本。' }
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$hostLine = & rustc -vV | Where-Object { $_ -like 'host: *' }
if ($LASTEXITCODE -ne 0 -or -not $hostLine) { throw '无法获取 Rust 主机目标。' }
$rustHost = $hostLine.Substring(6).Trim()
if ($rustHost -notmatch '-pc-windows-msvc$') { throw "需要 Windows MSVC 原生工具链：$rustHost" }
$versionLine = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $versionLine) { throw '无法读取项目版本。' }
$version = $versionLine.Matches[0].Groups[1].Value
$name = "icloud-download-helper-$version-$rustHost"
$outputDir = Join-Path $root 'dist'
$output = Join-Path $outputDir "$name.zip"
$staging = Join-Path ([System.IO.Path]::GetTempPath()) ("icloud-package-" + [guid]::NewGuid().ToString('N'))
try {
    Push-Location $root
    try {
        & cargo build --release --locked --features gui --bins --target $rustHost --target-dir (Join-Path $root 'target')
        if ($LASTEXITCODE -ne 0) { throw "Cargo 构建失败（退出码 $LASTEXITCODE）。" }
    } finally { Pop-Location }
    $package = Join-Path $staging $name
    New-Item -ItemType Directory -Path $package, $outputDir -Force | Out-Null
    $release = Join-Path $root "target/$rustHost/release"
    Copy-Item (Join-Path $release 'icloud-dl.exe'), (Join-Path $release 'icloud-dl-gui.exe') -Destination $package
    Copy-Item (Join-Path $root 'README.md'), (Join-Path $root 'LICENSE'), (Join-Path $root 'THIRD_PARTY_NOTICES.md') -Destination $package
    Compress-Archive -Path $package -DestinationPath $output -Force
    Write-Host "打包完成：$output"
} finally {
    if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
}
