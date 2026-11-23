#Requires -Version 5.1
<#
.SYNOPSIS
    Draws the app icon (apps/nitra/assets/nitra.ico), the logo the app shows
    in its sidebar (apps/nitra/assets/logo@*.png) and the README logo
    (docs/logo.png).

.DESCRIPTION
    Both use Bricolage Grotesque ExtraBold (800), loaded from Google Fonts,
    drawn by Microsoft Edge in headless mode from small HTML pages:
    - the icon: a white "n" on a blue (#0047FF) rounded square, drawn at
      every size Windows uses (16 to 256 px) rather than scaled down;
    - the logo: "nitra" in white on a blue pill 1.3 times the font size
      tall, letter spacing -0.02em, 0.45 of the font size on each side, the
      lowercase letters centered and the stems on whole pixels (see
      make-icon-logo.cs). The app's copy is drawn at the exact pixel size
      of each display scale (1x, 1.5x, 2x of a 26 px font), so it is never
      resized at run time.
    Needs an internet connection (for the font) and Edge. Keep this file
    ASCII-only (PowerShell 5.1 reads BOM-less files with the ANSI code page).
#>
[CmdletBinding()]
param(
    [string]$Edge = "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe"
)
$ErrorActionPreference = 'Stop'
# CSS wants "6.4px": a Portuguese Windows would write "6,4px".
[Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::InvariantCulture
$root = Split-Path -Parent $PSScriptRoot
$assets = Join-Path $root 'apps\nitra\assets'
$work = Join-Path ([IO.Path]::GetTempPath()) "nitra-icon-$PID"
New-Item -ItemType Directory -Force $work | Out-Null
Add-Type -AssemblyName System.Drawing

$font = '<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:opsz,wght@12..96,800&display=block">'
$face = "font-family:'Bricolage Grotesque';font-weight:800;line-height:1;color:#fff"

# Renders an HTML body on a transparent page and returns the PNG path.
function Render([string]$name, [string]$body, [int]$width, [int]$height, [double]$scale = 1) {
    $html = Join-Path $work "$name.html"
    $png = Join-Path $work "$name.png"
    $page = "<!doctype html><html><head><meta charset=`"utf-8`">$font<style>html,body{margin:0;background:transparent}</style></head><body>$body</body></html>"
    [IO.File]::WriteAllText($html, $page)
    $arguments = @('--headless=new', '--disable-gpu', '--hide-scrollbars',
        # The window size is in device pixels: scaled with the page.
        '--default-background-color=00000000', "--window-size=$([Math]::Ceiling($width * $scale)),$([Math]::Ceiling($height * $scale))",
        "--force-device-scale-factor=$scale", '--virtual-time-budget=8000',
        "--screenshot=$png", ([Uri]$html).AbsoluteUri)
    $process = Start-Process $Edge -ArgumentList $arguments -PassThru -WindowStyle Hidden
    if (-not $process.WaitForExit(60000)) { $process | Stop-Process -Force; throw "Edge did not finish $name" }
    if (-not (Test-Path $png)) { throw "Edge wrote no picture for $name" }
    $png
}

# The logo, built by make-icon-logo.cs from its five letters drawn 8 times
# larger (one picture per letter, each at its place in the word): the letters
# with a straight stem (n, i, r) move onto whole pixels, the lowercase band
# is centered in the pill, and everything is averaged 8x8 down. $px is the
# design font size (it also picks the font's optical size), $scale the
# display scale.
Add-Type -Path (Join-Path $PSScriptRoot 'make-icon-logo.cs') -ReferencedAssemblies System.Drawing
function Logo([string]$name, [double]$px, [double]$scale, [string]$out) {
    $size = $px * $scale * 8
    $letters = 'n', 'i', 't', 'r', 'a'
    $pictures = for ($k = 0; $k -lt $letters.Count; $k++) {
        $spans = for ($j = 0; $j -lt $letters.Count; $j++) {
            $color = if ($j -eq $k) { '#fff' } else { 'transparent' }
            "<span style=`"color:$color`">$($letters[$j])</span>"
        }
        $body = "<div style=`"padding:$($size / 4)px`"><span style=`"display:block;$face;font-variation-settings:'opsz' $px;font-size:${size}px;letter-spacing:-0.02em;white-space:nowrap`">$($spans -join '')</span></div>"
        Render "$name-$k" $body ([Math]::Ceiling($size * 4)) ([Math]::Ceiling($size * 1.6))
    }
    $height = [int][Math]::Round($px * $scale * 1.3)
    $result = [LogoSnap]::Build([string[]]$pictures, $out, $px * $scale * 0.45, $height)
    "wrote $out ($result)"
}

foreach ($scale in 1, 1.5, 2) { Logo "logo-$scale" 26 $scale (Join-Path $assets "logo@${scale}x.png") }
Logo 'logo-readme' 80 2 (Join-Path $root 'docs\logo.png')

# The icon at each size: the "n" sits a little above the middle, so its
# x-height (not its line box) is centered in the square.
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$images = foreach ($size in $sizes) {
    $radius = [Math]::Round($size * 0.22, 2)
    $body = "<div style=`"width:${size}px;height:${size}px;background:#0047FF;border-radius:${radius}px;display:flex;align-items:center;justify-content:center;$face;font-size:$($size * 0.906)px`"><span style=`"transform:translateY($(-$size * 0.055)px)`">n</span></div>"
    $png = Render "icon-$size" $body $size $size
    [pscustomobject]@{ Size = $size; Png = [IO.File]::ReadAllBytes($png) }
}

# ICONDIR, one ICONDIRENTRY per image, then the PNG data (Windows Vista and
# later read PNG images inside an .ico).
$file = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter $file
$writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($image in $images) {
    $side = if ($image.Size -ge 256) { 0 } else { $image.Size }
    $writer.Write([byte]$side); $writer.Write([byte]$side)
    $writer.Write([byte]0); $writer.Write([byte]0)
    $writer.Write([uint16]1); $writer.Write([uint16]32)
    $writer.Write([uint32]$image.Png.Length); $writer.Write([uint32]$offset)
    $offset += $image.Png.Length
}
foreach ($image in $images) { $writer.Write($image.Png) }
$writer.Flush()
$ico = Join-Path $assets 'nitra.ico'
[IO.File]::WriteAllBytes($ico, $file.ToArray())
"wrote $ico ($($file.Length) bytes, sizes $($sizes -join ', '))"
Remove-Item -Recurse -Force $work
