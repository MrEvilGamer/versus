param(
    [Parameter(Mandatory = $true)][string]$ShadersRpf,
    [Parameter(Mandatory = $true)][string]$MandyRepo,
    [Parameter(Mandatory = $true)][string]$Output
)

$ErrorActionPreference = "Stop"
$libraryTool = Join-Path $MandyRepo "tools\shader_library.py"
$carveTool = Join-Path $MandyRepo "tools\shader_carve.py"
if (-not (Test-Path -LiteralPath $ShadersRpf)) { throw "shaders.rpf not found: $ShadersRpf" }
if (-not (Test-Path -LiteralPath $libraryTool)) { throw "Mandy shader_library.py not found: $libraryTool" }
if (-not (Test-Path -LiteralPath $carveTool)) { throw "Mandy shader_carve.py not found: $carveTool" }

# Keep extracted game data outside the repository.  The cache contains native
# program bytes and carved source-container identities, not source changes.
New-Item -ItemType Directory -Force -Path $Output | Out-Null
$carve = Join-Path $Output "carve"
$library = Join-Path $Output "library"
python -I $carveTool $ShadersRpf $carve
python -I $libraryTool $ShadersRpf --out $library --stages ps,vs,ms --jobs 1 --carve $carve

# Slot 0x1C8's two program pointers are not a reliable stage label.  Index
# complete native program bytes and let the runtime resolve the stage from the
# matching carved container identity.
$records = @(
    Get-ChildItem (Join-Path $library "fspx") -Filter "*.code" -File | ForEach-Object {
        $stem = $_.BaseName
        $meta = Get-Content (Join-Path $_.DirectoryName ($stem + ".json")) -Raw | ConvertFrom-Json
        [pscustomobject]@{
            sha256 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            stage = $meta.stage
            code = $_.Name
            container = ($stem + ".dxbc")
        }
    }
)
$records | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $library "code-index.json") -Encoding utf8

Write-Output "Native graphics cache: $library"
Write-Output "Use VICTORY_FSPX_GFX_DIR=$library\fspx for the opt-in runtime resolver once enabled."
