# Generates crates/davinci/assets/davinci.ico — a small oscilloscope mark.
# Kept in the repo as a script so the icon can be rebuilt without Photoshop.
Add-Type -AssemblyName System.Drawing -ErrorAction Stop

# Absolute paths on purpose: Join-Path keeps a literal ".." and the file then
# lands in a stray directory instead of the crate.
$repo = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$out = Join-Path $repo 'crates\davinci\assets\davinci.ico'
$outDir = Split-Path $out -Parent
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

function New-Mark([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)

    # Dark rounded panel.
    $pad = [math]::Max(1, [int]($size * 0.04))
    $rect = New-Object System.Drawing.Rectangle($pad, $pad, ($size - 2 * $pad), ($size - 2 * $pad))
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $r = [single]([math]::Max(2, $size * 0.18))
    $path.AddArc($rect.X, $rect.Y, $r, $r, 180, 90)
    $path.AddArc(($rect.Right - $r), $rect.Y, $r, $r, 270, 90)
    $path.AddArc(($rect.Right - $r), ($rect.Bottom - $r), $r, $r, 0, 90)
    $path.AddArc($rect.X, ($rect.Bottom - $r), $r, $r, 90, 90)
    $path.CloseFigure()
    $bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, [System.Drawing.Color]::FromArgb(255, 18, 26, 38), [System.Drawing.Color]::FromArgb(255, 8, 12, 18), 90)
    $g.FillPath($bg, $path)

    # Faint graticule.
    $grid = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(46, 120, 190, 220), [single]([math]::Max(0.6, $size / 170)))
    $n = 4
    for ($i = 1; $i -lt $n; $i++) {
        $x = $rect.X + $rect.Width * $i / $n
        $y = $rect.Y + $rect.Height * $i / $n
        $g.DrawLine($grid, [single]$x, [single]$rect.Y, [single]$x, [single]$rect.Bottom)
        $g.DrawLine($grid, [single]$rect.X, [single]$y, [single]$rect.Right, [single]$y)
    }

    # Two traces: cyan (channel 1) and amber (channel 2).
    $traces = @(
        @{ color = [System.Drawing.Color]::FromArgb(255, 90, 226, 255); amp = 0.30; phase = 0.0; width = 2.2 },
        @{ color = [System.Drawing.Color]::FromArgb(255, 255, 176, 74); amp = 0.16; phase = 2.4; width = 1.8 }
    )
    foreach ($t in $traces) {
        $pen = New-Object System.Drawing.Pen($t.color, [single]([math]::Max(1.0, $size / 256 * $t.width * 2.4)))
        $pen.StartCap = 'Round'
        $pen.EndCap = 'Round'
        $pts = New-Object 'System.Collections.Generic.List[System.Drawing.PointF]'
        $steps = [int][math]::Max(24, $size / 3)
        for ($i = 0; $i -le $steps; $i++) {
            $f = $i / $steps
            $x = $rect.X + $rect.Width * $f
            $wave = [math]::Sin(($f * 2 * [math]::PI * 1.6) + $t.phase)
            $y = $rect.Y + $rect.Height * (0.5 + $t.amp * $wave)
            $pts.Add((New-Object System.Drawing.PointF([single]$x, [single]$y)))
        }
        $g.DrawLines($pen, $pts.ToArray())
        $pen.Dispose()
    }

    $g.Dispose()
    $path.Dispose()
    $bg.Dispose()
    $grid.Dispose()
    return $bmp
}

$sizes = 16, 24, 32, 48, 64, 128, 256
$images = @()
foreach ($s in $sizes) {
    $b = New-Mark $s
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $images += [pscustomobject]@{ Size = $s; Bytes = $ms.ToArray() }
    $ms.Dispose()
    $b.Dispose()
}

$fs = [System.IO.File]::Create($out)
$bw = New-Object System.IO.BinaryWriter($fs)
$bw.Write([uint16]0)                 # reserved
$bw.Write([uint16]1)                 # type: icon
$bw.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($img in $images) {
    $dim = if ($img.Size -ge 256) { 0 } else { $img.Size }
    $bw.Write([byte]$dim)            # width
    $bw.Write([byte]$dim)            # height
    $bw.Write([byte]0)               # palette
    $bw.Write([byte]0)               # reserved
    $bw.Write([uint16]1)             # planes
    $bw.Write([uint16]32)            # bpp
    $bw.Write([uint32]$img.Bytes.Length)
    $bw.Write([uint32]$offset)
    $offset += $img.Bytes.Length
}
foreach ($img in $images) { $bw.Write($img.Bytes) }
$bw.Flush(); $bw.Dispose(); $fs.Dispose()

Write-Output ("wrote {0} ({1} bytes, {2} images)" -f $out, (Get-Item $out).Length, $images.Count)
