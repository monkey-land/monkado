//! Bounded Media Foundation hardware H.264 encoder for Windows screen sharing and camera video.
#![allow(unsafe_code)]

use super::{Config, Profile};
use crate::screen::i420_to_nv12;
use client_core::screen::Encoder as Preference;
use std::{
	marker::PhantomData,
	rc::Rc,
	time::{Duration, Instant},
};
use windows::{
	Win32::{
		Media::MediaFoundation::*,
		System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
		System::Variant::VARIANT,
	},
	core::Interface,
};

const UNAVAILABLE: &str = "Windows hardware video encoding is unavailable";
const FAILED: &str = "Windows hardware video encoding failed";

struct Runtime(PhantomData<Rc<()>>);

impl Runtime {
	fn open() -> Result<Self, &'static str> {
		// SAFETY: The screen encoder owns this worker thread and balances both calls in Drop.
		unsafe {
			CoInitializeEx(None, COINIT_MULTITHREADED)
				.ok()
				.map_err(|_| UNAVAILABLE)?;
			if MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_err() {
				CoUninitialize();
				return Err(UNAVAILABLE);
			}
		}
		Ok(Self(PhantomData))
	}
}

impl Drop for Runtime {
	fn drop(&mut self) {
		// SAFETY: Balanced with Runtime::open on the same worker thread.
		unsafe {
			let _ = MFShutdown();
			CoUninitialize();
		}
	}
}

pub(crate) struct Encoder {
	transform: IMFTransform,
	events: IMFMediaEventGenerator,
	codec: ICodecAPI,
	_activate: Activated,
	_runtime: Runtime,
	frame: i64,
	duration: i64,
	need_input: usize,
	have_output: usize,
	provides_samples: bool,
	max_bytes: usize,
	max_buffer_bytes: usize,
}

struct Activated(IMFActivate);

impl Drop for Activated {
	fn drop(&mut self) {
		// SAFETY: The activation and its object stay on the Media Foundation worker.
		unsafe {
			let _ = self.0.ShutdownObject();
		}
	}
}

impl Encoder {
	pub(crate) fn new(config: Config) -> Result<Self, &'static str> {
		Self::with_preference(config, Preference::Auto)
	}

	/// Opens the hardware encoder `preference` names. `Auto` and a vendor that is missing both
	/// fall back to the best-ranked hardware encoder; `Software` never opens one.
	pub(crate) fn with_preference(
		config: Config,
		preference: Preference,
	) -> Result<Self, &'static str> {
		if preference == Preference::Software {
			return Err(UNAVAILABLE);
		}
		let runtime = Runtime::open()?;
		// SAFETY: Media Foundation owns returned COM objects; the activation array is cleared
		// before its CoTaskMem allocation is released.
		unsafe {
			let activate = Activated(hardware_encoder(preference)?);
			let transform: IMFTransform = activate.0.ActivateObject().map_err(|_| UNAVAILABLE)?;
			let attributes = transform.GetAttributes().map_err(|_| UNAVAILABLE)?;
			if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
				return Err(UNAVAILABLE);
			}
			attributes
				.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
				.map_err(|_| UNAVAILABLE)?;
			let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
			let codec: ICodecAPI = transform.cast().map_err(|_| UNAVAILABLE)?;
			let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
			let _ = codec.SetValue(
				&CODECAPI_AVEncCommonRateControlMode,
				&VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
			);
			let _ = codec.SetValue(
				&CODECAPI_AVEncCommonMeanBitRate,
				&VARIANT::from(config.bit_rate),
			);
			let _ = codec.SetValue(
				&CODECAPI_AVEncMPVDefaultBPictureCount,
				&VARIANT::from(0_u32),
			);
			let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(config.fps * 2));

			let output = video_type(config, MFVideoFormat_H264)?;
			output
				.SetUINT32(&MF_MT_AVG_BITRATE, config.bit_rate)
				.map_err(|_| UNAVAILABLE)?;
			// Advisory: an encoder that rejects the attribute keeps its own default profile.
			let _ = output.SetUINT32(
				&MF_MT_MPEG2_PROFILE,
				match config.profile {
					Profile::Baseline => eAVEncH264VProfile_Base.0 as u32,
					Profile::Main => eAVEncH264VProfile_Main.0 as u32,
				},
			);
			transform
				.SetOutputType(0, &output, 0)
				.map_err(|_| UNAVAILABLE)?;

			let input = video_type(config, MFVideoFormat_NV12)?;
			input
				.SetUINT32(&MF_MT_DEFAULT_STRIDE, config.width)
				.map_err(|_| UNAVAILABLE)?;
			transform
				.SetInputType(0, &input, 0)
				.map_err(|_| UNAVAILABLE)?;

			let info = transform.GetOutputStreamInfo(0).map_err(|_| UNAVAILABLE)?;
			// cbSize is minimum output-buffer capacity, not the encoded sample length.
			// Permit a bounded raw-picture-sized allocation while keeping the transport's
			// compressed frame cap enforced by sample_bytes after ProcessOutput.
			let max_buffer_bytes = (config.width as usize)
				.checked_mul(config.height as usize)
				.and_then(|pixels| pixels.checked_mul(4))
				.filter(|bytes| *bytes <= crate::screen::MAX_RAW_BYTES)
				.ok_or(UNAVAILABLE)?
				.max(config.max_bytes);
			let provides_samples = info.dwFlags
				& (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
					| MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
				!= 0;
			if !provides_samples {
				output_buffer_size(&info, max_buffer_bytes)?;
			}
			let events = transform.cast().map_err(|_| UNAVAILABLE)?;
			transform
				.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
				.map_err(|_| UNAVAILABLE)?;
			transform
				.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
				.map_err(|_| UNAVAILABLE)?;
			Ok(Self {
				transform,
				events,
				codec,
				_activate: activate,
				_runtime: runtime,
				frame: 0,
				duration: 10_000_000 / i64::from(config.fps),
				max_bytes: config.max_bytes,
				max_buffer_bytes,
				need_input: 0,
				have_output: 0,
				provides_samples,
			})
		}
	}

	pub(crate) fn set_bitrate(&mut self, bitrate: u32) -> Result<(), &'static str> {
		// SAFETY: Dynamic codec control stays on the encoder's owning worker.
		unsafe {
			self.codec
				.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate))
				.map_err(|_| FAILED)
		}
	}

	pub(crate) fn encode(
		&mut self,
		y: &[u8],
		u: &[u8],
		v: &[u8],
		force_keyframe: bool,
	) -> Result<(Vec<u8>, bool), &'static str> {
		let length = y
			.len()
			.checked_add(u.len())
			.and_then(|length| length.checked_add(v.len()))
			.filter(|_| u.len() == v.len() && y.len() == u.len() * 4)
			.ok_or(FAILED)?;
		self.encode_with(length, force_keyframe, |picture| {
			i420_to_nv12(y, u, v, picture)
		})
	}

	/// Encode one `length`-byte NV12 picture that `fill` writes into the native buffer.
	pub(crate) fn encode_with(
		&mut self,
		length: usize,
		force_keyframe: bool,
		fill: impl FnOnce(&mut [u8]) -> Result<(), &'static str>,
	) -> Result<(Vec<u8>, bool), &'static str> {
		self.wait_for_input()?;
		if force_keyframe {
			// SAFETY: Codec control is called on the owning worker before this input sample.
			unsafe {
				self.codec
					// Microsoft specifies ULONG (VT_UI4), not VARIANT_BOOL. A rejected
					// command otherwise silently sends camera and screen share to software.
					.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &force_keyframe_value())
					.map_err(|_| FAILED)?;
			}
		}
		// SAFETY: The allocation is exactly the validated NV12 size; Lock is balanced by Unlock.
		unsafe {
			let native_length = u32::try_from(length).map_err(|_| FAILED)?;
			let buffer = MFCreateMemoryBuffer(native_length).map_err(|_| FAILED)?;
			let mut data = std::ptr::null_mut();
			let mut capacity = 0;
			buffer
				.Lock(&mut data, Some(&mut capacity), None)
				.map_err(|_| FAILED)?;
			if data.is_null() || capacity < native_length {
				let _ = buffer.Unlock();
				return Err(FAILED);
			}
			let destination = std::slice::from_raw_parts_mut(data, length);
			if fill(destination).is_err() {
				let _ = buffer.Unlock();
				return Err(FAILED);
			}
			buffer.Unlock().map_err(|_| FAILED)?;
			buffer.SetCurrentLength(native_length).map_err(|_| FAILED)?;
			let sample = MFCreateSample().map_err(|_| FAILED)?;
			sample.AddBuffer(&buffer).map_err(|_| FAILED)?;
			sample
				.SetSampleTime(self.frame * self.duration)
				.map_err(|_| FAILED)?;
			sample
				.SetSampleDuration(self.duration)
				.map_err(|_| FAILED)?;
			self.frame += 1;
			self.transform
				.ProcessInput(0, &sample, 0)
				.map_err(|_| FAILED)?;
		}
		self.wait_for_output()?;
		self.take_output()
	}

	fn wait_for_input(&mut self) -> Result<(), &'static str> {
		self.wait_until(|encoder| encoder.need_input != 0)?;
		self.need_input -= 1;
		Ok(())
	}

	fn wait_for_output(&mut self) -> Result<(), &'static str> {
		self.wait_until(|encoder| encoder.have_output != 0)?;
		self.have_output -= 1;
		Ok(())
	}

	fn wait_until(&mut self, ready: impl Fn(&Self) -> bool) -> Result<(), &'static str> {
		let deadline = Instant::now() + Duration::from_millis(250);
		while !ready(self) {
			// SAFETY: Event polling stays on the worker that owns this MFT.
			let event = unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
			match event {
				Ok(event) => unsafe {
					if event.GetStatus().map_err(|_| FAILED)?.is_err() {
						return Err(FAILED);
					}
					match event.GetType().map_err(|_| FAILED)? as i32 {
						kind if kind == METransformNeedInput.0 => self.need_input += 1,
						kind if kind == METransformHaveOutput.0 => self.have_output += 1,
						kind if kind == MEError.0 => return Err(FAILED),
						_ => {}
					}
				},
				Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
					if Instant::now() >= deadline {
						return Err(FAILED);
					}
					std::thread::sleep(Duration::from_millis(1));
				}
				Err(_) => return Err(FAILED),
			}
		}
		Ok(())
	}

	fn take_output(&self) -> Result<(Vec<u8>, bool), &'static str> {
		// SAFETY: The output struct is fully initialized. We take ownership of either the
		// transform-provided sample or our cloned sample before releasing its native fields.
		unsafe {
			let own = if self.provides_samples {
				None
			} else {
				let info = self.transform.GetOutputStreamInfo(0).map_err(|_| FAILED)?;
				let size = output_buffer_size(&info, self.max_buffer_bytes)?;
				let buffer = MFCreateMemoryBuffer(size).map_err(|_| FAILED)?;
				let sample = MFCreateSample().map_err(|_| FAILED)?;
				sample.AddBuffer(&buffer).map_err(|_| FAILED)?;
				Some(sample)
			};
			let mut output = MFT_OUTPUT_DATA_BUFFER {
				dwStreamID: 0,
				pSample: std::mem::ManuallyDrop::new(own.clone()),
				dwStatus: 0,
				pEvents: std::mem::ManuallyDrop::new(None),
			};
			let mut status = 0;
			let result =
				self.transform
					.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
			let sample = std::mem::ManuallyDrop::into_inner(output.pSample).or(own);
			drop(std::mem::ManuallyDrop::into_inner(output.pEvents));
			result.map_err(|_| FAILED)?;
			let sample = sample.ok_or(FAILED)?;
			let data = sample_bytes(&sample, self.max_bytes)?;
			crate::video::validate_source(&data).map_err(|_| FAILED)?;
			let keyframe = crate::video_receive::is_keyframe(&data);
			Ok((data, keyframe))
		}
	}
}

fn force_keyframe_value() -> VARIANT {
	VARIANT::from(1_u32)
}

fn output_buffer_size(info: &MFT_OUTPUT_STREAM_INFO, limit: usize) -> Result<u32, &'static str> {
	if info.cbSize == 0 || info.cbSize as usize > limit {
		return Err(FAILED);
	}
	Ok(info.cbSize)
}

impl Drop for Encoder {
	fn drop(&mut self) {
		// SAFETY: Stop immediately; unsent video is disposable and flushing avoids blocking teardown.
		unsafe {
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
			let _ = self
				.transform
				.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
		}
	}
}

/// Hardware H.264 vendors on this machine, best-ranked first. Empty when Media Foundation
/// lists none or cannot start.
pub(crate) fn vendors() -> Vec<Preference> {
	let Ok(_runtime) = Runtime::open() else {
		return Vec::new();
	};
	// SAFETY: The runtime is open on this thread; entries are released by `enumerate`.
	let mut found: Vec<Preference> = unsafe { enumerate() }
		.unwrap_or_default()
		.into_iter()
		.filter_map(|activate| unsafe { vendor_of(&activate) })
		.collect();
	found.sort_by_key(|vendor| rank(*vendor));
	found.dedup();
	found
}

/// Auto's order: dedicated GPUs first, so a laptop or desktop with integrated graphics does not
/// encode on the iGPU while a discrete NVIDIA or AMD card sits idle.
fn rank(vendor: Preference) -> u8 {
	match vendor {
		Preference::Nvidia => 0,
		Preference::Amd => 1,
		Preference::Intel => 3,
		Preference::Auto | Preference::Software => 2,
	}
}

/// The vendor a hardware MFT reports through its PCI vendor id, `Auto` when unknown.
unsafe fn vendor_of(activate: &IMFActivate) -> Option<Preference> {
	unsafe {
		let mut text = windows::core::PWSTR::null();
		let mut length = 0;
		let vendor = activate
			.GetAllocatedString(
				&MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
				&mut text,
				&mut length,
			)
			.ok()
			.map(|()| {
				let value = text.to_string().unwrap_or_default();
				CoTaskMemFree(Some(text.0.cast()));
				value
			})?;
		Some(vendor_from_id(&vendor))
	}
}

fn vendor_from_id(id: &str) -> Preference {
	let id = id.to_ascii_uppercase();
	if id.contains("VEN_10DE") {
		Preference::Nvidia
	} else if id.contains("VEN_1002") || id.contains("VEN_1022") {
		Preference::Amd
	} else if id.contains("VEN_8086") {
		Preference::Intel
	} else {
		Preference::Auto
	}
}

/// Every async hardware NV12→H.264 encoder Media Foundation lists, in its own order.
unsafe fn enumerate() -> Result<Vec<IMFActivate>, &'static str> {
	unsafe {
		let input = MFT_REGISTER_TYPE_INFO {
			guidMajorType: MFMediaType_Video,
			guidSubtype: MFVideoFormat_NV12,
		};
		let output = MFT_REGISTER_TYPE_INFO {
			guidMajorType: MFMediaType_Video,
			guidSubtype: MFVideoFormat_H264,
		};
		let mut entries = std::ptr::null_mut();
		let mut count = 0;
		MFTEnumEx(
			MFT_CATEGORY_VIDEO_ENCODER,
			MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
			Some(&input),
			Some(&output),
			&mut entries,
			&mut count,
		)
		.map_err(|_| UNAVAILABLE)?;
		if entries.is_null() {
			return Err(UNAVAILABLE);
		}
		let entries_slice = std::slice::from_raw_parts_mut(entries, count as usize);
		let found = entries_slice.iter_mut().filter_map(Option::take).collect();
		CoTaskMemFree(Some(entries.cast()));
		Ok(found)
	}
}

/// Index of the encoder to open among rank-sorted vendors: the first for `Auto` (unknown
/// vendors also read as `Auto`, so they must not be matched), otherwise the named vendor,
/// falling back to the first.
fn pick(vendors: impl Iterator<Item = Preference>, preference: Preference) -> usize {
	if preference == Preference::Auto {
		return 0;
	}
	vendors
		.into_iter()
		.position(|vendor| vendor == preference)
		.unwrap_or(0)
}

/// The hardware encoder for `preference`: that vendor's when present, otherwise Auto's pick.
unsafe fn hardware_encoder(preference: Preference) -> Result<IMFActivate, &'static str> {
	unsafe {
		let mut found: Vec<(Preference, IMFActivate)> = enumerate()?
			.into_iter()
			.map(|activate| (vendor_of(&activate).unwrap_or(Preference::Auto), activate))
			.collect();
		// Stable, so Media Foundation's own order breaks ties within a vendor.
		found.sort_by_key(|(vendor, _)| rank(*vendor));
		let wanted = pick(found.iter().map(|(vendor, _)| *vendor), preference);
		if found.is_empty() {
			return Err(UNAVAILABLE);
		}
		Ok(found.swap_remove(wanted).1)
	}
}

unsafe fn video_type(
	config: Config,
	subtype: windows::core::GUID,
) -> Result<IMFMediaType, &'static str> {
	unsafe {
		let media_type = MFCreateMediaType().map_err(|_| UNAVAILABLE)?;
		media_type
			.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
			.map_err(|_| UNAVAILABLE)?;
		media_type
			.SetGUID(&MF_MT_SUBTYPE, &subtype)
			.map_err(|_| UNAVAILABLE)?;
		media_type
			.SetUINT64(
				&MF_MT_FRAME_SIZE,
				(u64::from(config.width) << 32) | u64::from(config.height),
			)
			.map_err(|_| UNAVAILABLE)?;
		media_type
			.SetUINT64(&MF_MT_FRAME_RATE, u64::from(config.fps) << 32 | 1)
			.map_err(|_| UNAVAILABLE)?;
		media_type
			.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1_u64 << 32 | 1)
			.map_err(|_| UNAVAILABLE)?;
		media_type
			.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
			.map_err(|_| UNAVAILABLE)?;
		Ok(media_type)
	}
}

fn sample_bytes(sample: &IMFSample, max_bytes: usize) -> Result<Vec<u8>, &'static str> {
	// SAFETY: Length is bounded before allocation; Lock's pointer is borrowed until Unlock.
	unsafe {
		if sample.GetTotalLength().map_err(|_| FAILED)? as usize > max_bytes {
			return Err(FAILED);
		}
		let buffer = sample.ConvertToContiguousBuffer().map_err(|_| FAILED)?;
		let mut data = std::ptr::null_mut();
		let mut capacity = 0;
		let mut length = 0;
		buffer
			.Lock(&mut data, Some(&mut capacity), Some(&mut length))
			.map_err(|_| FAILED)?;
		let result = if data.is_null() || length > capacity || length as usize > max_bytes {
			Err(FAILED)
		} else {
			Ok(std::slice::from_raw_parts(data, length as usize).to_vec())
		};
		buffer.Unlock().map_err(|_| FAILED)?;
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn larger_native_buffer_does_not_relax_encoded_sample_limit() {
		let _runtime = Runtime::open().unwrap();
		let limit = crate::camera::MAX_ENCODED_BYTES;
		// Synthetic memory only: no transform, GPU, capture device or transport.
		unsafe {
			let capacity = 640 * 480 * 3 / 2;
			assert!(capacity > limit);
			let buffer = MFCreateMemoryBuffer(capacity as u32).unwrap();
			let mut data = std::ptr::null_mut();
			buffer.Lock(&mut data, None, None).unwrap();
			std::ptr::write_bytes(data, 0x2a, capacity);
			buffer.Unlock().unwrap();
			buffer.SetCurrentLength(limit as u32).unwrap();
			let sample = MFCreateSample().unwrap();
			sample.AddBuffer(&buffer).unwrap();
			let encoded = sample_bytes(&sample, limit).unwrap();
			assert_eq!(encoded.len(), limit);
			assert!(encoded.iter().all(|&byte| byte == 0x2a));
			buffer.SetCurrentLength(limit as u32 + 1).unwrap();
			assert!(sample_bytes(&sample, limit).is_err());
		}

		{
			let mut info = MFT_OUTPUT_STREAM_INFO {
				dwFlags: 0,
				cbSize: 1280 * 720 * 3 / 2,
				cbAlignment: 0,
			};
			assert!(info.cbSize as usize > crate::camera::MAX_ENCODED_BYTES);
			assert_eq!(
				output_buffer_size(&info, 1280 * 720 * 4).unwrap(),
				info.cbSize
			);
			assert!(output_buffer_size(&info, 1024).is_err());
			info.cbSize = 0;
			assert!(output_buffer_size(&info, 1280 * 720 * 4).is_err());
			info.cbSize = u32::MAX;
			assert!(output_buffer_size(&info, 1280 * 720 * 4).is_err());
		}

		{
			let value = force_keyframe_value();
			assert_eq!(value.vt(), windows::Win32::System::Variant::VT_UI4);
			assert_eq!(u32::try_from(&value).unwrap(), 1);
		}
	}
}

#[cfg(test)]
mod vendor_tests {
	use super::*;

	#[test]
	fn auto_ranks_dedicated_gpus_ahead_of_intel_graphics() {
		let mut vendors = [
			vendor_from_id("VEN_8086"),
			vendor_from_id("ven_10de&dev_2484"),
			vendor_from_id("VEN_1002"),
		];
		vendors.sort_by_key(|vendor| rank(*vendor));
		assert_eq!(
			vendors,
			[Preference::Nvidia, Preference::Amd, Preference::Intel]
		);
	}

	#[test]
	fn auto_keeps_nvidia_ahead_of_an_unknown_vendor() {
		let mut vendors = [vendor_from_id("VEN_1234"), vendor_from_id("VEN_10DE")];
		vendors.sort_by_key(|vendor| rank(*vendor));
		assert_eq!(pick(vendors.into_iter(), Preference::Auto), 0);
		assert_eq!(vendors[0], Preference::Nvidia);
		assert_eq!(pick(vendors.into_iter(), Preference::Intel), 0);
	}
}
