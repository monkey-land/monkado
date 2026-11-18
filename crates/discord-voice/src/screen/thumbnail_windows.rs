//! Still previews of displays and windows for the share picker, taken with GDI on demand.
//! Each one is a single bounded RGBA picture; nothing is kept or captured continuously.
#![allow(unsafe_code)]

use super::{MAX_SOURCE_HEIGHT, MAX_SOURCE_WIDTH};
use crate::screen::SourceId;
use windows::Win32::{
	Foundation::{HWND, RECT},
	Graphics::Gdi::{
		BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
		CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, HALFTONE,
		HDC, HGDIOBJ, HMONITOR, MONITORINFO, ReleaseDC, SRCCOPY, SelectObject, SetBrushOrgEx,
		SetStretchBltMode, StretchBlt,
	},
	Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
	UI::WindowsAndMessaging::{GetWindowRect, IsIconic},
};

/// Largest preview edge; a 16:9 display becomes 320×180 (230 KB of RGBA).
pub(crate) const THUMBNAIL_WIDTH: u32 = 320;
pub(crate) const THUMBNAIL_HEIGHT: u32 = 180;
/// Asks the window to render its full content, including DirectX and browser surfaces.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);

/// A still preview of `source`, or `None` when it cannot be drawn (minimized, gone, too big).
pub(crate) fn thumbnail(source: SourceId) -> Option<image::RgbaImage> {
	match source {
		SourceId::Display(id) => display(HMONITOR(id as usize as *mut _)),
		SourceId::Window(id) => window(HWND(id as usize as *mut _)),
		_ => None,
	}
}

fn display(monitor: HMONITOR) -> Option<image::RgbaImage> {
	let mut info = MONITORINFO {
		cbSize: size_of::<MONITORINFO>() as u32,
		..Default::default()
	};
	if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
		return None;
	}
	let area = info.rcMonitor;
	let screen = ScreenDc::new()?;
	scaled(
		screen.0,
		area.left,
		area.top,
		area.right - area.left,
		area.bottom - area.top,
	)
}

fn window(window: HWND) -> Option<image::RgbaImage> {
	// A minimized window draws nothing useful; the picker shows its icon instead.
	if unsafe { IsIconic(window) }.as_bool() {
		return None;
	}
	let mut area = RECT::default();
	unsafe { GetWindowRect(window, &mut area) }.ok()?;
	let (width, height) = (area.right - area.left, area.bottom - area.top);
	if !within_bounds(width, height) {
		return None;
	}
	let screen = ScreenDc::new()?;
	let memory = MemoryDc::new(screen.0)?;
	let bitmap = unsafe { CreateCompatibleBitmap(screen.0, width, height) };
	if bitmap.is_invalid() {
		return None;
	}
	let bitmap = Object(bitmap.into());
	let previous = unsafe { SelectObject(memory.0, bitmap.0) };
	let printed = unsafe { PrintWindow(window, memory.0, PW_RENDERFULLCONTENT) }.as_bool();
	let preview = printed
		.then(|| scaled(memory.0, 0, 0, width, height))
		.flatten();
	unsafe { SelectObject(memory.0, previous) };
	preview
}

fn within_bounds(width: i32, height: i32) -> bool {
	width > 0
		&& height > 0
		&& width as u32 <= MAX_SOURCE_WIDTH
		&& height as u32 <= MAX_SOURCE_HEIGHT
}

/// Copies a source rectangle of `source` into a bounded, aspect-preserving RGBA picture.
fn scaled(source: HDC, x: i32, y: i32, width: i32, height: i32) -> Option<image::RgbaImage> {
	if !within_bounds(width, height) {
		return None;
	}
	let (target_width, target_height) = fit(width as u32, height as u32);
	let (target_width, target_height) = (target_width as i32, target_height as i32);
	let memory = MemoryDc::new(source)?;
	let info = BITMAPINFO {
		bmiHeader: BITMAPINFOHEADER {
			biSize: size_of::<BITMAPINFOHEADER>() as u32,
			biWidth: target_width,
			// Negative height: rows run top to bottom.
			biHeight: -target_height,
			biPlanes: 1,
			biBitCount: 32,
			biCompression: BI_RGB.0,
			..Default::default()
		},
		..Default::default()
	};
	let mut bits = std::ptr::null_mut();
	let bitmap =
		unsafe { CreateDIBSection(Some(memory.0), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
			.ok()?;
	let bitmap = Object(bitmap.into());
	if bits.is_null() {
		return None;
	}
	let previous = unsafe { SelectObject(memory.0, bitmap.0) };
	let copied = unsafe {
		SetStretchBltMode(memory.0, HALFTONE);
		let _ = SetBrushOrgEx(memory.0, 0, 0, None);
		StretchBlt(
			memory.0,
			0,
			0,
			target_width,
			target_height,
			Some(source),
			x,
			y,
			width,
			height,
			SRCCOPY,
		)
	}
	.as_bool();
	let picture = copied.then(|| {
		let length = target_width as usize * target_height as usize * 4;
		// The DIB section owns `length` bytes of 32-bit BGRX pixels until it is deleted below.
		let bgrx = unsafe { std::slice::from_raw_parts(bits as *const u8, length) };
		image::RgbaImage::from_raw(
			target_width as u32,
			target_height as u32,
			bgrx_to_rgba(bgrx),
		)
	});
	unsafe { SelectObject(memory.0, previous) };
	picture.flatten()
}

/// The largest size within the thumbnail box with the source's aspect ratio, never upscaled.
fn fit(width: u32, height: u32) -> (u32, u32) {
	let scale = (THUMBNAIL_WIDTH as f64 / width as f64)
		.min(THUMBNAIL_HEIGHT as f64 / height as f64)
		.min(1.0);
	(
		((width as f64 * scale).round() as u32).max(1),
		((height as f64 * scale).round() as u32).max(1),
	)
}

/// GDI leaves the fourth byte undefined; the preview is always opaque.
fn bgrx_to_rgba(bgrx: &[u8]) -> Vec<u8> {
	bgrx.as_chunks::<4>()
		.0
		.iter()
		.flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
		.collect()
}

struct ScreenDc(HDC);
impl ScreenDc {
	fn new() -> Option<Self> {
		let dc = unsafe { GetDC(None) };
		(!dc.is_invalid()).then_some(Self(dc))
	}
}
impl Drop for ScreenDc {
	fn drop(&mut self) {
		unsafe { ReleaseDC(None, self.0) };
	}
}

struct MemoryDc(HDC);
impl MemoryDc {
	fn new(compatible: HDC) -> Option<Self> {
		let dc = unsafe { CreateCompatibleDC(Some(compatible)) };
		(!dc.is_invalid()).then_some(Self(dc))
	}
}
impl Drop for MemoryDc {
	fn drop(&mut self) {
		let _ = unsafe { DeleteDC(self.0) };
	}
}

struct Object(HGDIOBJ);
impl Drop for Object {
	fn drop(&mut self) {
		let _ = unsafe { DeleteObject(self.0) };
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Previews keep the source's shape inside the box and never grow a small window.
	#[test]
	fn thumbnails_fit_the_box_without_upscaling() {
		assert_eq!(fit(3840, 2160), (320, 180));
		assert_eq!(fit(1080, 1920), (101, 180));
		assert_eq!(fit(200, 100), (200, 100));
		assert_eq!(fit(7680, 1), (320, 1));
		assert_eq!(bgrx_to_rgba(&[1, 2, 3, 0]), vec![3, 2, 1, 255]);
	}
}
