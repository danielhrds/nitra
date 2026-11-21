#Requires -Version 5.1
<#
.SYNOPSIS
    Draws the app icon (apps/ziz/assets/ziz.ico): a blue rounded square
    with the logo, at the sizes Windows uses (16 to 256 px).

.DESCRIPTION
    The logo is the app's mark: two z's joined by the i's stem, drawn as
    shapes with the same proportions as `Mark` in apps/ziz/src/ui/view.rs.
    Each size is drawn on its own (not scaled down from a big one), in whole
    pixels, so the bars stay sharp at 16 px. The .ico holds PNG images
    (Windows Vista and later read them). Keep this file ASCII-only
    (PowerShell 5.1 reads BOM-less files with the ANSI code page).
#>
[CmdletBinding()]
param([string]$Out)
# PowerShell 5.1 has no $PSScriptRoot in parameter defaults.
if (-not $Out) { $Out = Join-Path (Split-Path -Parent $PSScriptRoot) 'apps\ziz\assets\ziz.ico' }

Add-Type -AssemblyName System.Drawing
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$blue = [System.Drawing.Color]::FromArgb(255, 0x00, 0x47, 0xFF)
$white = [System.Drawing.Brushes]::White

# Away-from-zero rounding, like Rust's f32::round (PowerShell rounds to even).
function Round([double]$v) { [Math]::Round($v, [MidpointRounding]::AwayFromZero) }

# The mark's parts for letter height $h (see `Mark::new` in view.rs).
function New-Mark([double]$h) {
    $bar = [Math]::Max(1, (Round ($h * 0.22)))
    $stem = [Math]::Max($bar, (Round ($h * 0.3)))
    [pscustomobject]@{
        H = $h; Bar = $bar; Stem = $stem
        Z = Round ($h * 0.85)
        Slant = [Math]::Max($bar + 1, (Round ($h * 0.36)))
        Gap = [Math]::Max(1, (Round ($h * 0.08)))
    }
}

# The dot as wide as the stem or a bit wider, by an even amount.
function Get-Dot($m) { $m.Stem + 2 * [Math]::Max(0, (Round (($m.H * 0.36 - $m.Stem) / 2))) }

function Draw-Mark($g, $m, [double]$left, [double]$top) {
    $dot = Get-Dot $m
    $base = $top + $dot + $m.Gap
    $stem = $left + $m.Z
    $g.FillRectangle($white, [single]$stem, [single]$base, [single]$m.Stem, [single]$m.H)
    # A circle of 3 px or less blurs into gray: a square there.
    $dotX = [single]($stem + ($m.Stem - $dot) / 2)
    if ($dot -le 3) { $g.FillRectangle($white, $dotX, [single]$top, [single]$dot, [single]$dot) }
    else { $g.FillEllipse($white, $dotX, [single]$top, [single]$dot, [single]$dot) }
    foreach ($x in $left, ($stem + $m.Stem)) {
        $g.FillRectangle($white, [single]$x, [single]$base, [single]$m.Z, [single]$m.Bar)
        $g.FillRectangle($white, [single]$x, [single]($base + $m.H - $m.Bar), [single]$m.Z, [single]$m.Bar)
        # The diagonal reaches half a pixel into both bars: no seam between them.
        $y0 = $base + $m.Bar - 0.5; $y1 = $base + $m.H - $m.Bar + 0.5
        $run = ($m.Z - $m.Slant) / ($m.H - 2 * $m.Bar) * 0.5
        $right = $x + $m.Z
        $points = [System.Drawing.PointF[]]@(
            (New-Object System.Drawing.PointF ($right - $m.Slant + $run), $y0),
            (New-Object System.Drawing.PointF ([Math]::Min($right + $run, $right)), $y0),
            (New-Object System.Drawing.PointF ($x + $m.Slant - $run), $y1),
            (New-Object System.Drawing.PointF ([Math]::Max($x - $run, $x)), $y1))
        $g.FillPolygon($white, $points)
    }
}

function New-RoundedSquare([float]$size, [float]$radius) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $radius * 2
    $path.AddArc(0, 0, $d, $d, 180, 90)
    $path.AddArc($size - $d, 0, $d, $d, 270, 90)
    $path.AddArc($size - $d, $size - $d, $d, $d, 0, 90)
    $path.AddArc(0, $size - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    $path
}

$images = foreach ($size in $sizes) {
    $bitmap = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bitmap)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)
    $square = New-RoundedSquare $size ([Math]::Max(3, $size * 0.22))
    $g.FillPath((New-Object System.Drawing.SolidBrush $blue), $square)
    # The mark about 3/4 of the square wide, centered in whole pixels: a
    # wider stem when the leftover width is odd.
    $mark = New-Mark (Round ($size * 0.37))
    if (($size - (2 * $mark.Z + $mark.Stem)) % 2) { $mark.Stem += 1 }
    $width = 2 * $mark.Z + $mark.Stem
    $height = (Get-Dot $mark) + $mark.Gap + $mark.H
    Draw-Mark $g $mark (($size - $width) / 2) ([Math]::Floor(($size - $height) / 2))
    $g.Dispose()
    $stream = New-Object System.IO.MemoryStream
    $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    [pscustomobject]@{ Size = $size; Png = $stream.ToArray() }
}

# ICONDIR, one ICONDIRENTRY per image, then the PNG data.
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
New-Item -ItemType Directory -Force (Split-Path -Parent $Out) | Out-Null
[System.IO.File]::WriteAllBytes($Out, $file.ToArray())
"wrote $Out ($($file.Length) bytes, sizes $($sizes -join ', '))"
