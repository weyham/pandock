Add-Type -AssemblyName System.Drawing

function Add-RoundedRectangle($path, [float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $d = $r * 2
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
}

function Add-PandockMark($path) {
    $path.StartFigure()
    $path.AddBezier(19.5, 43, 14, 43, 9.5, 38.5, 9.5, 33.5)
    $path.AddBezier(9.5, 33.5, 9.5, 28.5, 13, 24.5, 17.5, 23.5)
    $path.AddBezier(17.5, 23.5, 19.5, 15, 27, 10.5, 34.5, 12.5)
    $path.AddBezier(34.5, 12.5, 40.5, 14, 44.5, 19, 45.5, 25)
    $path.AddBezier(45.5, 25, 50.5, 25, 54.5, 29, 54.5, 34)
    $path.AddBezier(54.5, 34, 54.5, 39, 50.5, 43, 44.5, 43)
    $path.CloseFigure()

    $dock = [System.Drawing.Drawing2D.GraphicsPath]::new()
    Add-RoundedRectangle $dock 20 50 24 4 2
    $path.AddPath($dock, $false)
    $dock.Dispose()
}

function New-IconBitmap([int]$size, [System.Drawing.Color]$markColor, [bool]$withBackground) {
    $bmp = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)
    $scale = $size / 64.0
    $g.ScaleTransform([float]$scale, [float]$scale)

    if ($withBackground) {
        $backgroundPath = [System.Drawing.Drawing2D.GraphicsPath]::new()
        Add-RoundedRectangle $backgroundPath 1 1 62 62 14
        $background = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(16, 24, 32))
        $g.FillPath($background, $backgroundPath)
        $background.Dispose()
        $backgroundPath.Dispose()
    }

    $markPath = [System.Drawing.Drawing2D.GraphicsPath]::new()
    Add-PandockMark $markPath
    $brush = [System.Drawing.SolidBrush]::new($markColor)
    $g.FillPath($brush, $markPath)
    $brush.Dispose()
    $markPath.Dispose()
    $g.Dispose()
    return $bmp
}

$iconsDir = Join-Path $PSScriptRoot '.'
$appSizes = @(
    @{ Size = 16;  Name = '16x16.png' },
    @{ Size = 20;  Name = '20x20.png' },
    @{ Size = 24;  Name = '24x24.png' },
    @{ Size = 32;  Name = '32x32.png' },
    @{ Size = 48;  Name = '48x48.png' },
    @{ Size = 64;  Name = '64x64.png' },
    @{ Size = 128; Name = '128x128.png' },
    @{ Size = 256; Name = 'icon.png' }
)
foreach ($spec in $appSizes) {
    $bmp = New-IconBitmap $spec.Size ([System.Drawing.Color]::White) $true
    $bmp.Save((Join-Path $iconsDir $spec.Name), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

$states = @{
    green  = [System.Drawing.Color]::FromArgb(51, 197, 104)
    yellow = [System.Drawing.Color]::FromArgb(228, 176, 47)
    red    = [System.Drawing.Color]::FromArgb(232, 74, 85)
    gray   = [System.Drawing.Color]::FromArgb(148, 160, 174)
}
foreach ($state in $states.Keys) {
    foreach ($size in @(16, 20, 24, 32, 48, 64, 128, 256)) {
        $bmp = New-IconBitmap $size $states[$state] $false
        $bmp.Save((Join-Path $iconsDir "status-$state-$size.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
    }
}
