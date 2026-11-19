# Draw the 2moon logo (the one the client paints, theme.rs paint_logo: an accent moon
# behind a bone-white one, with craters) into installer/windows/2moon.ico (16 to 256 px,
# used by both executables, the tray and the installers) and
# crates/moon-client/assets/icon.png (the client window icon).
#   powershell -ExecutionPolicy Bypass -File installer\windows\make-icon.ps1
Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\..\.."

# Master at 1024 px; the logo grid of paint_logo is 32 units high, its visible part
# spans x 3.2..43.5 (with the dark ring), y 2.2..29.8: centred in a square of 42 units.
$N = 1024
$u = $N / 42.0
$dx = 21.0 - 23.35
$dy = 21.0 - 16.0
function P([double]$v, [double]$d) { ($v + $d) * $u }

$master = New-Object System.Drawing.Bitmap $N, $N, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($master)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.Clear([System.Drawing.Color]::Transparent)
function Disc($g, $brush, [double]$x, [double]$y, [double]$r) {
    $g.FillEllipse($brush, [single](P ($x - $r) $dx), [single](P ($y - $r) $dy), [single](2 * $r * $u), [single](2 * $r * $u))
}
$accent = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 0xCB, 0xFF, 0x4D))
$shade = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(41, 0x09, 0x09, 0x0B))
$bone = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 0xF5, 0xF5, 0xF2))
$crater = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 0xCB, 0xCB, 0xC4))
$ring = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 0x09, 0x09, 0x0B))

Disc $g $accent 31.5 16 12
foreach ($c in @(@(36, 11, 2.4), @(38.2, 19.5, 1.6), @(32.5, 23, 1.9))) { Disc $g $shade $c[0] $c[1] $c[2] }
# The dark ring around the white moon (stroke 2.6 centred on r 12.5), as in the app:
# invisible on a dark taskbar, it outlines the white moon on a light one.
$g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
Disc $g $ring 17 16 13.8
$g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceOver
Disc $g $bone 17 16 11.2
foreach ($c in @(@(12.5, 12, 2.8), @(20.5, 20.5, 3.3), @(21.5, 10, 1.6), @(11.5, 21, 1.3), @(25, 15, 1.0))) { Disc $g $crater $c[0] $c[1] $c[2] }
$g.Dispose()

function Scale($src, [int]$size) {
    $b = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($b)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.DrawImage($src, 0, 0, $size, $size)
    $g.Dispose()
    $b
}

function PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return , $ms.ToArray()
}

# Classic DIB entry (BITMAPINFOHEADER, bottom-up BGRA, empty AND mask): what every
# icon reader handles; the 256 px entry is PNG as usual.
function DibBytes($bmp) {
    $w = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle 0, 0, $w, $w
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $px = New-Object byte[] ($w * $w * 4)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $px, 0, $px.Length)
    $bmp.UnlockBits($data)
    $maskRow = [int]([math]::Ceiling($w / 32.0) * 4)
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter $ms
    $bw.Write([uint32]40); $bw.Write([int32]$w); $bw.Write([int32]($w * 2)); $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]0); $bw.Write([uint32]($px.Length + $maskRow * $w)); $bw.Write([int32]0); $bw.Write([int32]0); $bw.Write([uint32]0); $bw.Write([uint32]0)
    for ($y = $w - 1; $y -ge 0; $y--) { $bw.Write($px, $y * $w * 4, $w * 4) }
    $bw.Write((New-Object byte[] ($maskRow * $w)))
    $bw.Flush()
    return , $ms.ToArray()
}

$sizes = 256, 128, 96, 64, 48, 40, 32, 24, 20, 16
$images = New-Object System.Collections.Generic.List[byte[]]
foreach ($s in $sizes) {
    $b = Scale $master $s
    if ($s -eq 256) { [byte[]]$bytes = PngBytes $b } else { [byte[]]$bytes = DibBytes $b }
    $images.Add($bytes)
    $b.Dispose()
}
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $ms
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $len = $images[$i].Length
    $bw.Write([byte]($s % 256)); $bw.Write([byte]($s % 256)); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]$len); $bw.Write([uint32]$offset)
    $offset += $len
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Flush()
[System.IO.File]::WriteAllBytes("$root\installer\windows\2moon.ico", $ms.ToArray())

$png = Scale $master 256
$png.Save("$root\crates\moon-client\assets\icon.png", [System.Drawing.Imaging.ImageFormat]::Png)
$png.Dispose()
$master.Dispose()
"2moon.ico and icon.png written"
