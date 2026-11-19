#Requires -Version 5.1
<#
.SYNOPSIS
    Lines up `bench loopback --display-csv` frames with a PresentMon capture
    and prints when the viewer's frames really reached the screen.

.DESCRIPTION
    For each <name>-frames.csv in -Dir, reads <name>-presentmon.csv (PresentMon
    run with --qpc_time on bench.exe). Both files use the QPC timeline, so a
    frame's present is the PresentMon present of the viewer's swap chain that
    started just before our "presented" time; its display time is
    TimeInQPC + MsUntilDisplayed. Prints render -> displayed (sampled frames),
    decoded -> displayed, present -> displayed and how far the DXGI frame
    statistics time is from PresentMon's.

    Keep this file ASCII-only: PowerShell 5.1 reads BOM-less files with the
    ANSI code page.

.EXAMPLE
    .\scripts\display-latency.ps1 -Dir D:\Projects\live\lab\loopback\2026-11-19\presentmon
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Dir,
    # Largest gap between PresentMon's present start and our "presented"
    # time (taken right after Present returns) for the two to be paired.
    [double] $MaxGapMs = 0.5
)

$ErrorActionPreference = 'Stop'
$invariant = [Globalization.CultureInfo]::InvariantCulture
$frequency = [Diagnostics.Stopwatch]::Frequency
$maxGap = [int64]($MaxGapMs * $frequency / 1000)

function Get-Quantile([double[]] $sorted, [double] $q) {
    if ($sorted.Count -eq 0) { return [double]::NaN }
    $sorted[[int][math]::Floor(($sorted.Count - 1) * $q)]
}

# Index of the last element of $times not after $value (-1 if none).
function Find-Previous([int64[]] $times, [int64] $value) {
    $index = [Array]::BinarySearch($times, $value)
    if ($index -lt 0) { $index = (-bnot $index) - 1 }
    $index
}

function Format-Ms([double[]] $values) {
    $sorted = [double[]]($values | Sort-Object)
    '{0:N1} / {1:N1} / {2:N1}' -f (Get-Quantile $sorted 0.05), (Get-Quantile $sorted 0.5), (Get-Quantile $sorted 0.95)
}

$frameFiles = Get-ChildItem $Dir -Filter '*-frames.csv' | Sort-Object Name
if (-not $frameFiles) { throw "no *-frames.csv in $Dir" }
foreach ($file in $frameFiles) {
    $name = $file.BaseName -replace '-frames$', ''
    $ours = Import-Csv $file.FullName
    $presentedTimes = [int64[]]($ours | ForEach-Object { [int64]$_.presented_qpc })
    $presents = Import-Csv (Join-Path $Dir "$name-presentmon.csv") |
        Where-Object { $_.PresentRuntime -eq 'DXGI' }

    # The viewer's swap chain is the one whose presents line up with ours.
    $viewer = $null
    $bestCount = -1
    foreach ($group in ($presents | Group-Object SwapChainAddress)) {
        $times = [int64[]]($group.Group | ForEach-Object { [int64]$_.TimeInQPC } | Sort-Object)
        $count = 0
        foreach ($time in $presentedTimes) {
            $index = Find-Previous $times $time
            if ($index -ge 0 -and ($time - $times[$index]) -le $maxGap) { $count++ }
        }
        if ($count -gt $bestCount) { $viewer = $group; $bestCount = $count }
    }
    $viewerPresents = $viewer.Group | Sort-Object { [int64]$_.TimeInQPC }
    $starts = [int64[]]($viewerPresents | ForEach-Object { [int64]$_.TimeInQPC })

    $renderToDisplayed = New-Object System.Collections.Generic.List[double]
    $decodedToDisplayed = New-Object System.Collections.Generic.List[double]
    $presentToDisplayed = New-Object System.Collections.Generic.List[double]
    $dxgiMinusPresentMon = New-Object System.Collections.Generic.List[double]
    $notDisplayed = 0
    foreach ($frame in $ours) {
        $presented = [int64]$frame.presented_qpc
        $index = Find-Previous $starts $presented
        if ($index -lt 0 -or ($presented - $starts[$index]) -gt $maxGap) { continue }
        $until = $viewerPresents[$index].MsUntilDisplayed
        if ($until -eq 'NA') { $notDisplayed++; continue }
        $displayed = $starts[$index] + [int64]([double]::Parse($until, $invariant) * $frequency / 1000)
        $toMs = { param($ticks) $ticks * 1000.0 / $frequency }
        $decodedToDisplayed.Add((& $toMs ($displayed - [int64]$frame.decoded_qpc)))
        $presentToDisplayed.Add((& $toMs ($displayed - $presented)))
        $dxgiMinusPresentMon.Add((& $toMs ([int64]$frame.dxgi_displayed_qpc - $displayed)))
        if ($frame.rendered_qpc) {
            $renderToDisplayed.Add((& $toMs ($displayed - [int64]$frame.rendered_qpc)))
        }
    }
    Write-Output ("{0}: {1} of {2} frames paired ({3} not displayed per PresentMon), swap chain {4}" -f `
        $name, $presentToDisplayed.Count, $ours.Count, $notDisplayed, $viewer.Name)
    Write-Output ("  render -> displayed  p5 / p50 / p95 (ms): {0} ({1} sampled frames)" -f (Format-Ms $renderToDisplayed), $renderToDisplayed.Count)
    Write-Output ("  decoded -> displayed p5 / p50 / p95 (ms): {0}" -f (Format-Ms $decodedToDisplayed))
    Write-Output ("  present -> displayed p5 / p50 / p95 (ms): {0}" -f (Format-Ms $presentToDisplayed))
    Write-Output ("  DXGI minus PresentMon p5 / p50 / p95 (ms): {0}" -f (Format-Ms $dxgiMinusPresentMon))
}
