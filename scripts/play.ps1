# Starts Versus with the inherited gameplay settings and backend defaults (README.md).
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         disable selected experimental paths, including GGVL hash bypass
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -More         other settings: -More @{VICTORY_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{}
)
$settings = @{
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = 'light'; VICTORY_SKIP_PIPELINES = 'none'
}
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
    $settings.VICTORY_GGVL_SKIP_READONLY_HASH = '0'
} else {
    $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
}
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
foreach ($k in $More.Keys) {
    if ($k -notmatch '^VICTORY_[A-Za-z0-9_]+
) { throw "Invalid Versus setting: $k" }
    $settings[$k] = $More[$k]
}

$victory = Join-Path $PSScriptRoot "..\target\$Build\versus.exe"
if (-not (Test-Path -LiteralPath $victory -PathType Leaf)) {
    throw "No $victory. Build it first (README.md)."
}
if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "No executable at $Exe." }
$victory = (Resolve-Path -LiteralPath $victory).Path
$Exe = (Resolve-Path -LiteralPath $Exe).Path

# Apply a clean profile to the child process, then restore the caller's environment.
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
$previous = @{}
Get-ChildItem Env: | Where-Object { $_.Name -like 'VICTORY_*' } | ForEach-Object {
    $previous[$_.Name] = $_.Value
}
Push-Location (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try {
    foreach ($k in $previous.Keys) {
        if ($k -notin $paths) { Remove-Item -LiteralPath "Env:$k" -ErrorAction Stop }
    }
    foreach ($k in $settings.Keys) { Set-Item -LiteralPath "Env:$k" $settings[$k] -ErrorAction Stop }
    & $victory run $Exe --execute
    $result = $LASTEXITCODE
} finally {
    Get-ChildItem Env: | Where-Object { $_.Name -like 'VICTORY_*' } | ForEach-Object {
        Remove-Item -LiteralPath "Env:$($_.Name)"
    }
    foreach ($k in $previous.Keys) { Set-Item -LiteralPath "Env:$k" $previous[$k] }
    Pop-Location
}
exit $result
