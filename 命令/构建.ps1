param(
    [Parameter(ValueFromRemainingArguments=$true)][string[]]$CargoArgs = @('build','--release','--locked'),
    [string]$ToolsRoot = $env:EARTHLIVE_RUST_TOOLS,
    [switch]$UseSystemRust
)
$ErrorActionPreference = 'Stop'
$v2Root = Split-Path -Parent $PSScriptRoot
$toolchain = 'stable-x86_64-pc-windows-gnu'
$localTools = Join-Path (Split-Path -Parent $v2Root) 'base\工具链'
if ($UseSystemRust) { $ToolsRoot = $null }
if (-not $UseSystemRust -and -not $ToolsRoot -and (Test-Path -LiteralPath (Join-Path $localTools "rustup\toolchains\$toolchain\bin\rustc.exe"))) {
    $ToolsRoot = $localTools
}
if ($ToolsRoot) {
    $compiler = Join-Path $ToolsRoot "rustup\toolchains\$toolchain\bin"
    $env:CARGO_HOME = Join-Path $ToolsRoot 'cargo'
    $env:RUSTUP_HOME = Join-Path $ToolsRoot 'rustup'
} else {
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) { throw '请先安装 Rust/rustup，再执行 rustup toolchain install stable-x86_64-pc-windows-gnu。' }
    $rustCompiler = & rustup which --toolchain $toolchain rustc
    if ($LASTEXITCODE -ne 0) { throw "请先执行 rustup toolchain install $toolchain" }
    $compiler = Split-Path -Parent $rustCompiler
}
$rustc = Join-Path $compiler 'rustc.exe'
$cargo = Join-Path $compiler 'cargo.exe'
if (-not (Test-Path -LiteralPath $rustc) -or -not (Test-Path -LiteralPath $cargo)) { throw '指定位置缺少 Rust GNU 编译器或 Cargo。' }
$sysroot = & $rustc --print sysroot
if ($LASTEXITCODE -ne 0) { throw '无法读取 Rust sysroot。' }
$rustNative = Join-Path $sysroot "lib\rustlib\x86_64-pc-windows-gnu\bin"
$env:CARGO_TARGET_DIR = Join-Path $v2Root '中间文件\cargo'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$taskTemp = Join-Path $v2Root '中间文件\临时'
New-Item -ItemType Directory -Force -Path $taskTemp | Out-Null
$env:TEMP = $taskTemp
$env:TMP = $taskTemp
$env:PATH = "$compiler;$rustNative;$rustNative\self-contained;$env:PATH"
$env:RUSTUP_TOOLCHAIN = $toolchain
$env:EARTHLIVE_LLD = Join-Path $rustNative 'rust-lld.exe'
$buildTools = Join-Path $v2Root '中间文件\构建工具'
New-Item -ItemType Directory -Force -Path $buildTools | Out-Null
$bridge = Join-Path $buildTools 'dlltool-bridge.exe'
$bridgeSource = Join-Path $PSScriptRoot 'dlltool-bridge.rs'
if (-not (Test-Path -LiteralPath $bridge) -or (Get-Item -LiteralPath $bridgeSource).LastWriteTimeUtc -gt (Get-Item -LiteralPath $bridge).LastWriteTimeUtc) {
    & $rustc $bridgeSource --edition=2024 -O -C strip=symbols -C "linker=$env:EARTHLIVE_LLD" -C linker-flavor=ld.lld -C link-self-contained=yes -o $bridge
    if ($LASTEXITCODE -ne 0) { throw '构建 LLVM 导入库桥接工具失败。' }
}
$env:CARGO_ENCODED_RUSTFLAGS = @('-C','linker-flavor=ld.lld','-C','link-self-contained=yes','-C',"dlltool=$bridge") -join [char]31
Push-Location (Join-Path $v2Root '代码')
try {
    & $cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) { throw "Cargo 失败：$LASTEXITCODE" }
} finally { Pop-Location }
