//! Bounded local emoji preparation. Only the settings confirmation sends an upload.
use eframe::egui;
use image::{AnimationDecoder, ImageDecoder};
use model::Id;
use std::{
	io::{Cursor, Read},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
		mpsc,
	},
};

type Scope = (u64, Id, u64);
type Prepared = (String, String, bool, egui::ColorImage);
type Selected = Result<Vec<Prepared>, &'static str>;
struct Choosing {
	scope: Scope,
	result: mpsc::Receiver<Selected>,
	cancelled: Arc<AtomicBool>,
}
#[derive(Default)]
pub struct EmojiUpload {
	choosing: Option<Choosing>,
}
impl EmojiUpload {
	pub fn start(
		&mut self,
		scope: Scope,
		paths: Vec<PathBuf>,
		runtime: &tokio::runtime::Handle,
		context: &egui::Context,
		parent: Arc<winit::window::Window>,
	) -> Result<(), &'static str> {
		if self.choosing.is_some() {
			return Err("Feche primeiro o seletor de emojis anterior");
		}
		if paths.len() > 10 {
			return Err("Escolha no máximo 10 emojis por vez");
		}
		let dialog = paths
			.is_empty()
			.then(|| platform::save::emoji_sources(parent));
		let (send, result) = mpsc::sync_channel(1);
		let cancelled = Arc::new(AtomicBool::new(false));
		let flag = cancelled.clone();
		let context = context.clone();
		runtime.spawn(async move {
			let paths = match dialog {
				Some(dialog) => dialog.await.unwrap_or_default(),
				None => paths,
			};
			let decode_flag = flag.clone();
			let result = tokio::task::spawn_blocking(move || {
				if paths.len() > 10 {
					return Err("Escolha no máximo 10 emojis por vez");
				}
				let mut prepared = Vec::new();
				for path in paths {
					if decode_flag.load(Ordering::Acquire) {
						return Ok(Vec::new());
					}
					prepared.push(read(&path, &decode_flag)?);
				}
				Ok(prepared)
			})
			.await
			.unwrap_or(Err(
				"Preparação dos emojis interrompida; escolha as imagens de novo",
			));
			let _ = send.send(if flag.load(Ordering::Acquire) {
				Ok(Vec::new())
			} else {
				result
			});
			context.request_repaint();
		});
		self.choosing = Some(Choosing {
			scope,
			result,
			cancelled,
		});
		Ok(())
	}
	pub fn cancel(&self) {
		if let Some(job) = &self.choosing {
			job.cancelled.store(true, Ordering::Release);
		}
	}
	pub fn poll(
		&mut self,
		generation: u64,
		valid: impl FnOnce(Id) -> bool,
	) -> Option<(Scope, Selected)> {
		let job = self.choosing.as_ref()?;
		if job.scope.0 != generation || !valid(job.scope.1) {
			self.cancel();
		}
		let result = match job.result.try_recv() {
			Ok(result) => result,
			Err(mpsc::TryRecvError::Empty) => return None,
			Err(mpsc::TryRecvError::Disconnected) => {
				Err("Preparação dos emojis interrompida; escolha as imagens de novo")
			}
		};
		let job = self.choosing.take()?;
		(!job.cancelled.load(Ordering::Acquire)).then_some((job.scope, result))
	}
}
impl Drop for EmojiUpload {
	fn drop(&mut self) {
		self.cancel();
	}
}

fn read(path: &Path, cancelled: &AtomicBool) -> Result<Prepared, &'static str> {
	const MAX_INPUT: u64 = 8 * 1024 * 1024;
	if !path.is_absolute() || path.as_os_str().as_encoded_bytes().len() > 4096 {
		return Err("Escolha uma imagem local com um caminho compatível");
	}
	let metadata =
		std::fs::symlink_metadata(path).map_err(|_| "Não foi possível abrir o emoji escolhido")?;
	if !metadata.is_file()
		|| metadata.file_type().is_symlink()
		|| metadata.len() == 0
		|| metadata.len() > MAX_INPUT
	{
		return Err("Escolha arquivos de imagem comuns de até 8 MB");
	}
	let mut bytes = Vec::with_capacity(metadata.len() as usize);
	std::fs::File::open(path)
		.map_err(|_| "Não foi possível abrir o emoji escolhido")?
		.take(MAX_INPUT + 1)
		.read_to_end(&mut bytes)
		.map_err(|_| "Não foi possível ler o emoji escolhido")?;
	if bytes.len() as u64 > MAX_INPUT {
		return Err("Escolha imagens de até 8 MB");
	}
	let gif = bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a");
	let (uri, preview) = if gif {
		if bytes.len() > 256 * 1024 {
			return Err("Emojis animados devem ter no máximo 256 KB");
		}
		let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes))
			.map_err(|_| "Imagem GIF inválida")?;
		let mut limits = image::Limits::default();
		limits.max_image_width = Some(4096);
		limits.max_image_height = Some(4096);
		limits.max_alloc = Some(64 * 1024 * 1024);
		decoder
			.set_limits(limits)
			.map_err(|_| "A imagem GIF é grande demais")?;
		let mut preview = None;
		let mut total = 0usize;
		for (index, frame) in decoder.into_frames().enumerate() {
			if cancelled.load(Ordering::Acquire) {
				return Err("Preparação dos emojis cancelada");
			}
			if index >= 120 {
				return Err("Escolha um GIF com no máximo 120 quadros");
			}
			let frame = frame.map_err(|_| "Animação GIF inválida")?.into_buffer();
			total = total.saturating_add(frame.as_raw().len());
			if total > 64 * 1024 * 1024 {
				return Err("Escolha uma animação GIF menor");
			}
			if preview.is_none() {
				let pixels = image::imageops::thumbnail(&frame, 128, 128);
				preview = Some(egui::ColorImage::from_rgba_unmultiplied(
					[pixels.width() as usize, pixels.height() as usize],
					pixels.as_raw(),
				));
			}
		}
		(
			discord_protocol::server_admin::emoji_data_uri(&bytes, true)
				.ok_or("Imagem de emoji inválida")?,
			preview.ok_or("O GIF não tem quadros")?,
		)
	} else {
		crate::group_icon::decode_image(&bytes, 128, false)?
	};
	let mut name: String = path
		.file_stem()
		.and_then(|name| name.to_str())
		.unwrap_or("emoji")
		.chars()
		.filter(|c| c.is_ascii_alphanumeric() || *c == '_')
		.take(32)
		.collect();
	if name.len() < 2 {
		name = "emoji".into();
	}
	Ok((name, uri, gif, preview))
}
