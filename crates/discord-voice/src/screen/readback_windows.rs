//! Asynchronous GPU readback for Windows Graphics Capture frames.
//!
//! The capture frame pool holds a single buffer, so the frame callback must return it quickly:
//! while a callback runs, Windows cannot deliver the next picture. Reading a frame on the
//! callback (a new staging texture per frame, then a blocking `Map`) made the callback last as
//! long as the GPU copy, page-locking and row copy together. That cost is largest when the
//! capture device and the display GPU differ (integrated graphics next to a discrete card) and
//! showed up as uneven, stuttering shares.
//!
//! Here the callback only queues a GPU copy into a reused staging texture and returns. A
//! readback thread waits for that copy without holding the device lock and hands the packed
//! pixels to the encoder.
#![allow(unsafe_code)] // Narrow Direct3D 11 boundary; every texture stays owned by one slot.

use super::{MAX_RAW_BYTES, RawFrame};
use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
	mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::{
	Direct3D11::{
		D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ,
		D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
		ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
	},
	Dxgi::{
		Common::DXGI_FORMAT, Common::DXGI_SAMPLE_DESC, DXGI_ERROR_WAS_STILL_DRAWING, IDXGIDevice,
	},
};
use windows::core::Interface;

/// Staging textures in rotation: one being copied, one being read, one spare for a resize.
const SLOTS: usize = 3;
/// How long the readback waits for one GPU copy before dropping that picture.
const COPY_DEADLINE: Duration = Duration::from_millis(250);

/// A reused CPU-readable texture and the picture size it was created for.
struct Slot {
	texture: Option<ID3D11Texture2D>,
	width: u32,
	height: u32,
	format: DXGI_FORMAT,
}

// SAFETY: Direct3D 11 resources are free-threaded; the device context they are used with is
// multithread-protected before any slot crosses threads, and each slot has one owner at a time.
unsafe impl Send for Slot {}

/// The capture device, borrowed from windows-capture as this crate's Direct3D types.
struct Device {
	device: ID3D11Device,
	context: ID3D11DeviceContext,
}

// SAFETY: See `Slot`: the context is multithread-protected before it is shared.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// Callback-side half: owns free slots and queues copies to the readback thread.
pub(super) struct Readback {
	device: Arc<Device>,
	free: Receiver<Slot>,
	filled: SyncSender<Slot>,
}

impl Readback {
	/// Starts readback on the device that delivers `frame`. `pending` is cleared whenever a
	/// queued picture is dropped instead of reaching `frames`.
	pub(super) fn start(
		frame: &windows_capture::frame::Frame,
		frames: SyncSender<RawFrame>,
		pending: Arc<AtomicBool>,
		stop: Arc<AtomicBool>,
	) -> Result<Self, &'static str> {
		let device = Arc::new(borrow_device(frame)?);
		let (free_send, free) = mpsc::sync_channel(SLOTS);
		for _ in 0..SLOTS {
			let _ = free_send.try_send(Slot {
				texture: None,
				width: 0,
				height: 0,
				format: DXGI_FORMAT(0),
			});
		}
		let (filled, queued) = mpsc::sync_channel::<Slot>(SLOTS);
		let worker = device.clone();
		std::thread::Builder::new()
			.name("screen-readback".into())
			.spawn(move || read_loop(&worker, &queued, &free_send, &frames, &pending, &stop))
			.map_err(|_| "Screen capture could not be started")?;
		if crate::diagnostics::enabled() {
			eprintln!(
				"[monkado] screen capture adapter: {}",
				adapter_name(&device.device)
			);
		}
		Ok(Self {
			device,
			free,
			filled,
		})
	}

	/// Queues a GPU copy of `frame`. `Ok(false)` means every slot is busy and the picture was
	/// skipped; the caller keeps the next one.
	pub(super) fn queue(
		&mut self,
		frame: &windows_capture::frame::Frame,
	) -> Result<bool, &'static str> {
		let mut slot = match self.free.try_recv() {
			Ok(slot) => slot,
			Err(TryRecvError::Empty) => return Ok(false),
			Err(TryRecvError::Disconnected) => return Err("Screen capture readback stopped"),
		};
		let source = borrow::<ID3D11Texture2D>(frame.as_raw_texture())?;
		let (width, height) = (frame.width(), frame.height());
		let format = DXGI_FORMAT(frame.desc().Format.0);
		if slot.texture.is_none()
			|| (slot.width, slot.height, slot.format) != (width, height, format)
		{
			slot.texture = Some(self.staging(width, height, format)?);
			(slot.width, slot.height, slot.format) = (width, height, format);
		}
		let texture = slot.texture.as_ref().expect("staging texture");
		// SAFETY: Both textures belong to the capture device and share size and format.
		unsafe {
			self.device.context.CopyResource(texture, &source);
			self.device.context.Flush();
		}
		self.filled
			.try_send(slot)
			.map_err(|_| "Screen capture readback stopped")?;
		Ok(true)
	}

	fn staging(
		&self,
		width: u32,
		height: u32,
		format: DXGI_FORMAT,
	) -> Result<ID3D11Texture2D, &'static str> {
		let desc = D3D11_TEXTURE2D_DESC {
			Width: width,
			Height: height,
			MipLevels: 1,
			ArraySize: 1,
			Format: format,
			SampleDesc: DXGI_SAMPLE_DESC {
				Count: 1,
				Quality: 0,
			},
			Usage: D3D11_USAGE_STAGING,
			BindFlags: 0,
			CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
			MiscFlags: 0,
		};
		let mut texture = None;
		// SAFETY: The description is fully initialized and the output slot outlives the call.
		unsafe {
			self.device
				.device
				.CreateTexture2D(&desc, None, Some(&mut texture))
				.map_err(|_| "The graphics driver could not read the captured screen")?;
		}
		texture.ok_or("The graphics driver could not read the captured screen")
	}
}

/// Reinterprets a windows-capture COM pointer as this crate's interface without changing its
/// reference count, then takes a counted reference of our own.
fn borrow<T: Interface + Clone>(
	interface: &impl capture_windows_core::Interface,
) -> Result<T, &'static str> {
	let raw = interface.as_raw();
	// SAFETY: Both `windows` releases describe the same Direct3D 11 COM interface, and `raw`
	// stays alive for this borrow because `interface` owns a reference to it.
	unsafe { T::from_raw_borrowed(&raw).cloned() }
		.ok_or("The graphics driver could not read the captured screen")
}

fn borrow_device(frame: &windows_capture::frame::Frame) -> Result<Device, &'static str> {
	let device = borrow::<ID3D11Device>(frame.device())?;
	let context = borrow::<ID3D11DeviceContext>(frame.device_context())?;
	// The callback and the readback thread now share the immediate context.
	let multithread: ID3D11Multithread = context
		.cast()
		.map_err(|_| "The graphics driver could not read the captured screen")?;
	// SAFETY: Enabling protection only serializes later context calls.
	unsafe {
		let _ = multithread.SetMultithreadProtected(true);
	}
	Ok(Device { device, context })
}

fn read_loop(
	device: &Device,
	queued: &Receiver<Slot>,
	free: &SyncSender<Slot>,
	frames: &SyncSender<RawFrame>,
	pending: &AtomicBool,
	stop: &AtomicBool,
) {
	let mut metrics = crate::diagnostics::Metrics::new(crate::diagnostics::Scope::ScreenVideo);
	for slot in queued {
		let start = metrics.start();
		let read = read(device, &slot, stop);
		metrics.finish(crate::diagnostics::Stage::CaptureRead, start);
		let delivered = match read {
			Ok(frame) => frames.try_send(frame).is_ok(),
			Err(()) => false,
		};
		if !delivered {
			pending.store(false, Ordering::Release);
		}
		metrics.poll(false, u64::from(!delivered), false, 0);
		if free.try_send(slot).is_err() || stop.load(Ordering::Acquire) {
			return;
		}
	}
}

/// Waits for the slot's copy, polling so the device lock is never held while the GPU works,
/// then packs its rows. A device fault stops the capture.
fn read(device: &Device, slot: &Slot, stop: &AtomicBool) -> Result<RawFrame, ()> {
	let texture = slot.texture.as_ref().ok_or(())?;
	let deadline = Instant::now() + COPY_DEADLINE;
	let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
	loop {
		// SAFETY: The staging texture is CPU-readable and unmapped; `mapped` outlives the call.
		let result = unsafe {
			device.context.Map(
				texture,
				0,
				D3D11_MAP_READ,
				D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
				Some(&mut mapped),
			)
		};
		match result {
			Ok(()) => break,
			Err(error)
				if error.code() == DXGI_ERROR_WAS_STILL_DRAWING
					&& Instant::now() < deadline
					&& !stop.load(Ordering::Acquire) =>
			{
				std::thread::sleep(Duration::from_millis(1));
			}
			Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => return Err(()),
			Err(_) => {
				stop.store(true, Ordering::Release);
				return Err(());
			}
		}
	}
	let packed = pack(&mapped, slot.width, slot.height);
	// SAFETY: Balances the successful Map above.
	unsafe { device.context.Unmap(texture, 0) };
	packed.ok_or_else(|| stop.store(true, Ordering::Release))
}

fn pack(mapped: &D3D11_MAPPED_SUBRESOURCE, width: u32, height: u32) -> Option<RawFrame> {
	let row_bytes = (width as usize).checked_mul(4)?;
	let stride = mapped.RowPitch as usize;
	let source_len = stride.checked_mul(height as usize)?;
	if mapped.pData.is_null() || stride < row_bytes || source_len > MAX_RAW_BYTES {
		return None;
	}
	// SAFETY: A successful Map exposes RowPitch × height readable bytes until Unmap.
	let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), source_len) };
	Some(pack_rows(source, stride, row_bytes, width, height))
}

/// Drops each row's padding so the encoder sees tightly packed BGRA.
fn pack_rows(source: &[u8], stride: usize, row_bytes: usize, width: u32, height: u32) -> RawFrame {
	let mut data = Vec::with_capacity(row_bytes * height as usize);
	for row in source.chunks_exact(stride) {
		data.extend_from_slice(&row[..row_bytes]);
	}
	RawFrame {
		width,
		height,
		stride: row_bytes,
		data,
	}
}

/// The capture adapter's name for opt-in diagnostics, bounded to 128 characters.
fn adapter_name(device: &ID3D11Device) -> String {
	let name = device
		.cast::<IDXGIDevice>()
		.ok()
		// SAFETY: Read-only adapter queries on a live device.
		.and_then(|dxgi| unsafe { dxgi.GetAdapter() }.ok())
		.and_then(|adapter| unsafe { adapter.GetDesc() }.ok())
		.map(|desc| {
			let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
			String::from_utf16_lossy(&desc.Description[..end.min(128)])
		});
	name.unwrap_or_else(|| "unknown".to_owned())
}
