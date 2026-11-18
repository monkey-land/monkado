# Captures the Monkado window (only it) to a PNG for visual checks.
# Usage: pwsh -File capture-window.ps1 -Out .evidence/<slug>/<state>.png -Id <pid>
# Pass the PID of the --demo instance you started: the owner may have their real, signed-in
# Monkado open too, and that window must never be captured.
param(
	[Parameter(Mandatory = $true)][string]$Out,
	[Parameter(Mandatory = $true)][int]$Id
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class MonkadoWindow {
	[StructLayout(LayoutKind.Sequential)]
	public struct Rect { public int Left, Top, Right, Bottom; }
	[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rect rect);
	[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
	[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
'@
[void][MonkadoWindow]::SetProcessDPIAware()
$target = Get-Process -Id $Id -ErrorAction SilentlyContinue
if (-not $target -or $target.MainWindowHandle -eq [IntPtr]::Zero) {
	throw "Process $Id has no visible window. Did the --demo instance exit?"
}
if ($target.CommandLine -and $target.CommandLine -notmatch '--demo') {
	throw "Process $Id was not started with --demo; refusing to capture a real session."
}
$rect = New-Object MonkadoWindow+Rect
[void][MonkadoWindow]::GetWindowRect($target.MainWindowHandle, [ref]$rect)
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) { throw 'The window is minimized or has no size.' }
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$dc = $graphics.GetHdc()
# 2 = PW_RENDERFULLCONTENT: includes GPU-rendered content such as the egui surface.
$printed = [MonkadoWindow]::PrintWindow($target.MainWindowHandle, $dc, 2)
$graphics.ReleaseHdc($dc)
$graphics.Dispose()
if (-not $printed) { $bitmap.Dispose(); throw 'PrintWindow failed.' }
$path = [System.IO.Path]::GetFullPath($Out)
[void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($path))
$bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
$bitmap.Dispose()
Write-Output $path
