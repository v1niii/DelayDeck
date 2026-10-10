$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" }
$repo = $env:DELAYDECK_UPDATE_REPO

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$name = "DelayDeck-$version-windows-x64"
$stage = Join-Path "dist" $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item (Join-Path $target "release\delaydeck.exe") (Join-Path $stage "DelayDeck.exe")
Copy-Item "packaging\README.txt" $stage

$crates = cargo tree -e normal --prefix none --format "{p}  ({l})" |
    ForEach-Object { $_ -replace ' \(\*\)$', '' } |
    Where-Object { $_ -notmatch '^delaydeck ' } |
    Sort-Object -Unique
$mit = @"
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"@
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
$other = foreach ($line in $crates) {
    if ($line -match '^(\S+) v(\S+)  \((.+)\)$' -and $Matches[3] -notmatch 'MIT|Apache-2\.0') {
        $crate = "$($Matches[1])-$($Matches[2])"
        $file = Get-ChildItem (Join-Path $cargoHome "registry\src\*\$crate\LICENSE*") -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $file) { throw "license file for $crate not found" }
        "=== $crate ($($Matches[3])) ==="
        ""
        (Get-Content $file.FullName -Raw)
        ""
    }
}
$notices = @(
    "DelayDeck $version includes the third-party software listed below."
    ""
    "=== Rust crates (name, version, license) ==="
    ""
    $crates
    ""
    "MIT-licensed crates above are used under the MIT License; copyright belongs"
    "to each crate's authors (see https://crates.io/crates/<name>):"
    ""
    $mit
    ""
    $other
    "=== mpegts.js 1.7.3, the output preview player (Apache-2.0) ==="
    "https://github.com/xqq/mpegts.js"
    "Apache-2.0-licensed crates above are used under the same license text:"
    ""
    (Get-Content "web\vendor\mpegts.LICENSE" -Raw)
)
Set-Content -Path (Join-Path $stage "THIRD-PARTY-NOTICES.txt") -Value $notices -Encoding utf8

$zip = Join-Path "dist" "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Write-Host ("Packed {0} ({1:N1} MB)" -f $zip, ((Get-Item $zip).Length / 1MB))

if ($repo) {
    $asset = "$name.exe"
    Copy-Item (Join-Path $stage "DelayDeck.exe") (Join-Path "dist" $asset)
    $sha = (Get-FileHash (Join-Path "dist" $asset) -Algorithm SHA256).Hash.ToLower()
    $manifest = [ordered]@{
        version = $version
        url     = "https://github.com/$repo/releases/download/v$version/$asset"
        sha256  = $sha
        notes   = "https://github.com/$repo/releases/tag/v$version"
    }
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText((Join-Path $PWD "dist\latest.json"), ($manifest | ConvertTo-Json), $utf8)
    Write-Host "Wrote dist\$asset and dist\latest.json for $repo"
}
