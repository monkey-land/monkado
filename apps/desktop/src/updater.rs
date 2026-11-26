//! Credential-free release updates. Work and filesystem access stay off the UI thread.
use eframe::egui;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
		mpsc,
	},
	time::{Duration, Instant},
};
use tokio::runtime::Runtime;

#[path = "updater_delta.rs"]
mod delta;
#[path = "updater_install.rs"]
mod install;

const RELEASES: &str = "https://api.github.com/repos/monkey-land/monkado/releases";
/// Web page that redirects to the newest stable tag; not subject to the REST API quota.
const LATEST_PAGE: &str = "https://github.com/monkey-land/monkado/releases/latest";
const DOWNLOADS: &str = "https://github.com/monkey-land/monkado/releases/download";
const RATE_LIMITED: &str = "O limite de atualizações do GitHub foi atingido. Tente mais tarde.";
const MAX_METADATA: usize = 2 * 1024 * 1024;
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const LOG_RELEASES: usize = 30;
/// How long after launch a downloaded automatic update restarts the app by itself.
const AUTO_RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOG_NOTES: usize = 16 * 1024;
const LOG_ITEMS: usize = 60;

#[derive(Clone, Deserialize)]
struct Asset {
	name: String,
	browser_download_url: String,
	size: u64,
}
#[derive(Clone, Deserialize)]
struct Release {
	tag_name: String,
	draft: bool,
	prerelease: bool,
	assets: Vec<Asset>,
	#[serde(default)]
	published_at: Option<String>,
	#[serde(default)]
	body: Option<String>,
}
#[derive(Clone)]
struct Package {
	version: String,
	// `None` on an installation with no in-app installer: the version is still reported,
	// but there is nothing here to download.
	archive: Option<Asset>,
	checksums: Option<Asset>,
	zsync: Option<Asset>,
}
struct Staged {
	directory: PathBuf,
	installation: PathBuf,
}
enum Outcome {
	Checked(Option<Package>, Vec<ui::updates::LogEntry>),
	Downloaded(Staged),
	Prepared(install::Prepared),
	Cleaned,
}
struct Job {
	receiver: mpsc::Receiver<Result<Outcome, String>>,
	cancel: Arc<AtomicBool>,
	progress: Arc<AtomicU64>,
	total: u64,
}
pub struct Updater {
	demo: bool,
	channel: Option<bool>,
	next_check: Instant,
	last_check: Option<Instant>,
	job: Option<Job>,
	package: Option<Package>,
	staged: Option<Staged>,
	cleanup_pending: Option<Staged>,
	status: String,
	armed: bool,
	helper: Option<install::Prepared>,
	close_requested: bool,
	auto_download: bool,
	demo_available: bool,
	demo_ready: bool,
	log: Option<Vec<ui::updates::LogEntry>>,
	/// Launch time: an automatic update downloaded soon after it restarts into the new version.
	launched: Instant,
}
impl Updater {
	pub fn new(demo: bool) -> Self {
		Self {
			demo,
			channel: None,
			next_check: Instant::now(),
			last_check: None,
			job: None,
			package: None,
			staged: None,
			cleanup_pending: None,
			status: "Ainda não houve verificação de atualizações.".into(),
			armed: false,
			helper: None,
			close_requested: false,
			auto_download: false,
			demo_available: false,
			demo_ready: false,
			log: None,
			launched: Instant::now(),
		}
	}

	/// Returns true only when a prepared restart should enter the normal close/draft gate.
	pub fn sync(
		&mut self,
		ctx: &egui::Context,
		runtime: &Runtime,
		view: &mut ui::updates::Updates,
		enabled: bool,
	) -> bool {
		let check = std::mem::take(&mut view.check_requested);
		let download = std::mem::take(&mut view.download_requested);
		let restart = std::mem::take(&mut view.restart_requested);
		// Update on launch: an automatic update found right after launch installs at once.
		let restart = restart
			|| (view.auto_update
				&& self.staged.is_some()
				&& !self.armed
				&& self.launched.elapsed() < AUTO_RESTART_WINDOW);
		if self.demo {
			if self.channel != Some(view.nightly) {
				self.demo_available = false;
				self.demo_ready = false;
			}
			self.channel = Some(view.nightly);
			if check {
				self.demo_available = true;
				view.log = demo_log();
				self.status =
					"Synthetic preview: Monkado 99.0.0 is available. No network request was made."
						.into();
			}
			if download && self.demo_available {
				self.demo_ready = true;
				self.status =
					"Synthetic preview: update ready to restart. No files were downloaded.".into();
			}
			if restart && self.demo_ready {
				self.status =
					"Synthetic preview: restart simulated. No installation was changed.".into();
			}
			view.status.clone_from(&self.status);
			view.available = self.demo_available;
			view.ready = self.demo_ready;
			view.busy = false;
			view.progress = None;
			view.supported = true;
			return false;
		}
		let supported = (cfg!(any(target_os = "macos", windows)) && !install::nix_session())
			|| install::appimage_session();
		view.supported = supported;
		view.flatpak = install::flatpak_session();
		view.linux_update_cmd = install::linux_package_manager_update_command().map(str::to_owned);
		if !enabled {
			if let Some(job) = &self.job {
				job.cancel.store(true, Ordering::Relaxed);
			}
			view.busy = self.job.is_some();
			view.available = false;
			view.ready = false;
			view.progress = None;
			view.status = if cfg!(debug_assertions) {
				"A verificação de atualizações fica desativada em builds de depuração."
			} else {
				"Carregue as preferências de atualização ou escolha suas configurações de atualização para ativar a verificação."
			}
			.into();
			return false;
		}
		if self.channel != Some(view.nightly) {
			if let Some(job) = &self.job {
				job.cancel.store(true, Ordering::Relaxed);
			}
			self.channel = Some(view.nightly);
			self.package = None;
			self.auto_download = false;
			self.cancel_restart();
			self.discard_stage();
			self.next_check = Instant::now();
			self.last_check = None;
			self.status = "Ainda não houve verificação de atualizações neste canal.".into();
			view.log.clear();
		}
		if let Some(job) = &self.job {
			match job.receiver.try_recv() {
				Ok(result) => {
					let cancelled = job.cancel.load(Ordering::Relaxed);
					self.job = None;
					if cancelled {
						match result {
							Ok(Outcome::Downloaded(stage)) => {
								self.cleanup_pending = Some(stage);
							}
							Ok(Outcome::Prepared(helper)) => {
								self.helper = Some(helper);
							}
							_ => {}
						}
					} else {
						match result {
							Ok(Outcome::Checked(package, log)) => {
								self.log = Some(log);
								self.status = package.as_ref().map_or_else(
									|| "O Monkado está atualizado neste canal.".into(),
									|p| {
										if install::flatpak_session() {
											format!(
												"Monkado {} está disponível. Atualize com `flatpak update` ou pela sua central de software.",
												p.version,
											)
										} else if install::nix_session() {
											format!(
												"Monkado {} está disponível. Atualize pelo Nix.",
												p.version,
											)
										} else if let Some(cmd) =
											install::linux_package_manager_update_command()
										{
											format!(
												"Monkado {} está disponível. Execute `{cmd}` para atualizar.",
												p.version,
											)
										} else {
											format!("Monkado {} está disponível.", p.version)
										}
									},
								);
								self.package = package;
								self.auto_download = true;
							}
							Ok(Outcome::Downloaded(stage)) => {
								self.staged = Some(stage);
								self.status =
									"Atualização baixada e verificada. Reinicie quando quiser."
										.into();
							}
							Ok(Outcome::Cleaned) => {}
							Ok(Outcome::Prepared(helper)) => {
								self.helper = Some(helper);
								self.armed = true;
								self.close_requested = true;
								self.status =
									"Atualização pronta. Feche o Monkado para instalar e reiniciar.".into();
							}
							Err(error) => {
								self.auto_download = false;
								self.status = error;
								self.next_check = Instant::now() + CHECK_INTERVAL;
							}
						}
					}
				}
				Err(mpsc::TryRecvError::Disconnected) => {
					self.job = None;
					self.status = "O processo de atualização parou. Tente novamente.".into();
				}
				Err(mpsc::TryRecvError::Empty) => {}
			}
		}
		if self.job.is_none() {
			if let Some(stage) = self.cleanup_pending.take() {
				let helper = self.helper.take();
				self.start(runtime, ctx, 0, move |_, _| async move {
					tokio::task::spawn_blocking(move || {
						if let Some(helper) = helper {
							helper.stop();
						}
						install::cleanup(&stage.directory);
					})
					.await
					.map_err(|_| {
						"Não foi possível limpar os arquivos de atualização antigos.".to_owned()
					})?;
					Ok(Outcome::Cleaned)
				});
			} else if restart && self.staged.is_some() && !self.armed {
				let stage = self.staged.as_ref().expect("checked stage");
				let directory = stage.directory.clone();
				let installation = stage.installation.clone();
				let version = self.package.as_ref().map(|p| p.version.clone());
				let old_helper = self.helper.take();
				self.start(runtime, ctx, 0, move |cancel, _| async move {
					let helper = tokio::task::spawn_blocking(move || {
						if let Some(helper) = old_helper {
							helper.stop();
						}
						if cancel.load(Ordering::Relaxed) {
							return Err("Atualização cancelada.".into());
						}
						install::prepare_restart(&directory, &installation, version.as_deref())
					})
					.await
					.map_err(|_| {
						"Não foi possível preparar o reinício da atualização.".to_owned()
					})??;
					Ok(Outcome::Prepared(helper))
				});
				self.status = "Preparando para reiniciar…".into();
			} else if supported
				&& (download || (view.auto_update && self.auto_download))
				&& self.package.is_some()
				&& self.staged.is_none()
			{
				let package = self.package.as_ref().expect("checked package").clone();
				let total = package
					.archive
					.as_ref()
					.expect("a supported platform's checked package has a downloadable archive")
					.size;
				self.auto_download = false;
				self.status = format!("Baixando o Monkado {}…", package.version);
				self.start(runtime, ctx, total, move |cancel, progress| {
					download_package(package, cancel, progress)
				});
			} else if self.staged.is_none() && (check || Instant::now() >= self.next_check) {
				if check
					&& self
						.last_check
						.is_some_and(|last| last.elapsed() < Duration::from_secs(60))
				{
					self.status = "Aguarde um minuto entre as verificações de atualização.".into();
				} else {
					self.last_check = Some(Instant::now());
					self.next_check = Instant::now() + CHECK_INTERVAL;
					let nightly = view.nightly;
					self.start(runtime, ctx, 0, move |cancel, _| async move {
						check_release(nightly, cancel)
							.await
							.map(|(package, log)| Outcome::Checked(package, log))
					});
					self.status = "Procurando atualizações…".into();
				}
			}
		}
		view.busy = self.job.is_some();
		view.progress = self.job.as_ref().filter(|job| job.total > 0).map(|job| {
			(job.progress.load(Ordering::Relaxed) as f64 / job.total as f64).min(1.0) as f32
		});
		view.available = self.package.is_some()
			|| self.staged.is_some()
			|| self.job.as_ref().is_some_and(|job| job.total > 0);
		view.ready = self.staged.is_some() && self.job.is_none();
		view.status.clone_from(&self.status);
		if let Some(log) = self.log.take() {
			view.log = log;
		}
		if self.job.is_some() {
			ctx.request_repaint_after(Duration::from_millis(200));
		} else if self.staged.is_none() {
			ctx.request_repaint_after(
				self.next_check
					.saturating_duration_since(Instant::now())
					.max(Duration::from_secs(1)),
			);
		}
		std::mem::take(&mut self.close_requested)
	}
	fn start<F, Fut>(&mut self, runtime: &Runtime, ctx: &egui::Context, total: u64, work: F)
	where
		F: FnOnce(Arc<AtomicBool>, Arc<AtomicU64>) -> Fut + Send + 'static,
		Fut: std::future::Future<Output = Result<Outcome, String>> + Send + 'static,
	{
		let (sender, receiver) = mpsc::sync_channel(1);
		let cancel = Arc::new(AtomicBool::new(false));
		let progress = Arc::new(AtomicU64::new(0));
		let worker_cancel = Arc::clone(&cancel);
		let worker_progress = Arc::clone(&progress);
		let ctx = ctx.clone();
		runtime.spawn(async move {
			let result = work(worker_cancel, worker_progress)
				.await
				.map_err(|error| format!("Falha na atualização: {error}"));
			if let Err(mpsc::SendError(result)) = sender.send(result) {
				tokio::task::spawn_blocking(move || match result {
					Ok(Outcome::Downloaded(stage)) => install::cleanup(&stage.directory),
					Ok(Outcome::Prepared(helper)) => helper.stop(),
					_ => {}
				});
			}
			ctx.request_repaint();
		});
		self.job = Some(Job {
			receiver,
			cancel,
			progress,
			total,
		});
	}
	fn discard_stage(&mut self) {
		if let Some(stage) = self.staged.take() {
			self.cleanup_pending = Some(stage);
		}
	}

	pub fn cancel_restart(&mut self) {
		self.armed = false;
		self.close_requested = false;
		if let Some(job) = &self.job {
			job.cancel.store(true, Ordering::Relaxed);
		}
	}

	/// Called only during the application's approved shutdown, after draft/close gates.
	pub fn finish_restart(&mut self) -> Result<(), String> {
		if self.demo || !self.armed {
			return Ok(());
		}
		if self.staged.is_some() {
			std::fs::write(
				&self
					.helper
					.as_ref()
					.ok_or("Nenhum assistente de atualização está pronto.")?
					.marker,
				[],
			)
			.map_err(|_| "Não foi possível passar a atualização para o instalador.".to_owned())?;
			self.staged = None; // The helper owns cleanup after this handoff.
			self.helper = None;
		}
		self.armed = false;
		Ok(())
	}
}
impl Drop for Updater {
	fn drop(&mut self) {
		if let Some(job) = &self.job {
			job.cancel.store(true, Ordering::Relaxed);
		}
		let helper = self.helper.take();
		let stage = self.staged.take().or_else(|| self.cleanup_pending.take());
		if helper.is_some() || stage.is_some() {
			std::thread::spawn(move || {
				if let Some(helper) = helper {
					helper.stop();
				}
				if let Some(stage) = stage {
					install::cleanup(&stage.directory);
				}
			});
		}
	}
}

fn client() -> Result<wreq::Client, String> {
	wreq::Client::builder()
		.https_only(true)
		.no_proxy()
		.user_agent(concat!("Monkado/", env!("CARGO_PKG_VERSION")))
		.connect_timeout(Duration::from_secs(10))
		.read_timeout(Duration::from_secs(30))
		.timeout(Duration::from_secs(600))
		.redirect(wreq::redirect::Policy::custom(|attempt| {
			if attempt.previous.len() < 5
				&& url::Url::parse(&attempt.uri.to_string()).is_ok_and(|url| trusted_url(&url))
			{
				attempt.follow()
			} else {
				attempt.error("Untrusted update redirect")
			}
		}))
		.build()
		.map_err(|_| "Não foi possível iniciar a conexão segura de atualização.".into())
}
fn trusted_url(url: &url::Url) -> bool {
	url.scheme() == "https"
		&& url.port_or_known_default() == Some(443)
		&& url.username().is_empty()
		&& url.password().is_none()
		&& url.fragment().is_none()
		&& matches!(
			url.host_str(),
			Some(
				"api.github.com"
					| "github.com" | "release-assets.githubusercontent.com"
					| "objects.githubusercontent.com"
			)
		)
}
async fn response(client: &wreq::Client, url: &str) -> Result<wreq::Response, String> {
	if !url::Url::parse(url).is_ok_and(|url| trusted_url(&url)) {
		return Err("A versão contém um endereço de download não confiável.".into());
	}
	let response = client
		.get(url)
		.header(wreq::header::ACCEPT, "application/vnd.github+json")
		.send()
		.await
		.map_err(|_| {
			"Não foi possível acessar o GitHub. Verifique sua conexão e tente novamente.".to_owned()
		})?;
	match response.status().as_u16() {
		200 => Ok(response),
		403 | 429 => Err(RATE_LIMITED.into()),
		404 => Err("Ainda não há versão publicada neste canal.".into()),
		_ => Err("O GitHub não conseguiu fornecer a atualização. Tente mais tarde.".into()),
	}
}
async fn bounded_body(
	client: &wreq::Client,
	url: &str,
	limit: usize,
	cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
	let response = response(client, url).await?;
	if response
		.content_length()
		.is_some_and(|length| length > limit as u64)
	{
		return Err("Os metadados da versão excedem o limite de tamanho.".into());
	}
	let mut body = Vec::new();
	let mut response_body = std::pin::pin!(response.bytes_stream());
	while let Some(chunk) = futures_util::StreamExt::next(&mut response_body)
		.await
		.transpose()
		.map_err(|_| "A resposta da atualização foi interrompida.".to_owned())?
	{
		if cancel.load(Ordering::Relaxed) {
			return Err("Atualização cancelada.".into());
		}
		if chunk.len() > limit.saturating_sub(body.len()) {
			return Err("Os metadados da versão excedem o limite de tamanho.".into());
		}
		body.extend_from_slice(&chunk);
	}
	Ok(body)
}
fn release_version(tag: &str) -> Option<semver::Version> {
	if tag.len() > 96
		|| !tag
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
	{
		return None;
	}
	semver::Version::parse(tag.strip_prefix('v')?).ok()
}
fn asset_name(tag: &str) -> Option<String> {
	if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
		return install::appimage_session().then(|| format!("monkado-{tag}-Linux-X64.AppImage"));
	}
	let os = if cfg!(target_os = "macos") {
		"macOS"
	} else if cfg!(windows) {
		"Windows"
	} else {
		return None;
	};
	let arch = if cfg!(target_arch = "aarch64") {
		"ARM64"
	} else if cfg!(target_arch = "x86_64") {
		"X64"
	} else {
		return None;
	};
	Some(format!("monkado-{tag}-{os}-{arch}.zip"))
}
fn on_channel(release: &Release, version: &semver::Version, nightly: bool) -> bool {
	let is_nightly = version.pre.as_str().starts_with("nightly.");
	// Monkado publishes stable releases only; the nightly channel still receives them.
	let stable = !release.prerelease && version.pre.is_empty();
	!release.draft && (stable || (nightly && release.prerelease && is_nightly))
}
/// Releases between the running build and the newest one, notes reduced to plain bullets.
fn release_log(
	releases: &[Release],
	nightly: bool,
	current: &semver::Version,
) -> Vec<ui::updates::LogEntry> {
	let mut entries = releases
		.iter()
		.filter_map(|release| {
			let version = release_version(&release.tag_name)?;
			(version > *current && on_channel(release, &version, nightly))
				.then_some((version, release))
		})
		.collect::<Vec<_>>();
	entries.sort_by(|a, b| b.0.cmp(&a.0));
	entries
		.into_iter()
		.take(LOG_RELEASES)
		.map(|(version, release)| ui::updates::LogEntry {
			version: version.to_string(),
			date: release
				.published_at
				.as_deref()
				.and_then(|date| date.get(..10))
				.filter(|date| date.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
				.unwrap_or_default()
				.to_owned(),
			sections: release_notes(release.body.as_deref().unwrap_or_default()),
		})
		.collect()
}
/// Release-please markdown to `(heading, bullets)`: links keep their text, commit hashes go.
fn release_notes(body: &str) -> Vec<(String, Vec<String>)> {
	let body = body
		.get(..body.floor_char_boundary(LOG_NOTES))
		.unwrap_or(body);
	let mut sections: Vec<(String, Vec<String>)> = Vec::new();
	let mut items = 0;
	for line in body.lines().map(str::trim) {
		if let Some(heading) = line.strip_prefix("### ") {
			sections.push((plain_markdown(heading), Vec::new()));
		} else if let Some(item) = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")) {
			let item = plain_markdown(item);
			if item.is_empty() || items == LOG_ITEMS {
				continue;
			}
			items += 1;
			if sections.is_empty() {
				sections.push((String::new(), Vec::new()));
			}
			sections.last_mut().expect("pushed").1.push(item);
		}
	}
	sections.retain(|(_, items)| !items.is_empty());
	sections
}
fn plain_markdown(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let mut rest = text;
	while let Some(open) = rest.find('[') {
		let Some(mid) = rest[open..].find("](").map(|i| open + i) else {
			break;
		};
		let Some(close) = rest[mid..].find(')').map(|i| mid + i) else {
			break;
		};
		out.push_str(&rest[..open]);
		out.push_str(&rest[open + 1..mid]);
		rest = &rest[close + 1..];
	}
	out.push_str(rest);
	let mut out = out.replace("**", "").replace('`', "");
	// Release-please closes every bullet with its commit, ` (abc1234)` once unlinked.
	if out.ends_with(')')
		&& let Some(start) = out.rfind(" (")
		&& out[start + 2..out.len() - 1]
			.bytes()
			.all(|b| b.is_ascii_hexdigit())
	{
		out.truncate(start);
	}
	out.trim().to_owned()
}
fn select_release(
	releases: Vec<Release>,
	nightly: bool,
	current: &semver::Version,
) -> Result<Option<Package>, String> {
	if releases.len() > 100 {
		return Err("A lista de versões excede o limite.".into());
	}
	let candidate = releases
		.into_iter()
		.filter_map(|release| {
			let version = release_version(&release.tag_name)?;
			on_channel(&release, &version, nightly).then_some((version, release))
		})
		.max_by(|a, b| a.0.cmp(&b.0));
	let Some((version, release)) = candidate else {
		return Err("Ainda não há versão publicada neste canal.".into());
	};
	// Channel changes never silently downgrade an installed build.
	if version <= *current {
		return Ok(None);
	}
	if release.assets.len() > 32 {
		return Err("A versão contém arquivos demais.".into());
	}
	// No in-app installer for this installation: still report the newer version so the
	// UI can show it, but there is no asset to look up or download.
	let Some(wanted) = asset_name(&release.tag_name) else {
		return Ok(Some(Package {
			version: version.to_string(),
			archive: None,
			checksums: None,
			zsync: None,
		}));
	};
	let find = |name: &str| -> Result<Asset, String> {
		let mut found = release.assets.iter().filter(|asset| asset.name == name);
		let asset = found.next().ok_or_else(|| {
			format!("Esta versão não tem o pacote {name}. Tente uma versão mais recente.")
		})?;
		if found.next().is_some()
			|| asset.size == 0
			|| asset.size > MAX_DOWNLOAD
			|| asset.browser_download_url.len() > 4096
		{
			return Err("Os metadados dos arquivos da versão são inválidos.".into());
		}
		let expected = format!(
			"https://github.com/monkey-land/monkado/releases/download/{}/{name}",
			release.tag_name
		);
		if asset.browser_download_url != expected {
			return Err("O arquivo não vem do repositório de versões do Monkado.".into());
		}
		Ok(asset.clone())
	};
	let archive = find(&wanted)?;
	let checksums = find("SHA256SUMS.txt")?;
	let zsync = wanted
		.ends_with(".AppImage")
		.then(|| find(&format!("{wanted}.zsync")).ok())
		.flatten()
		.filter(|asset| asset.size <= delta::MAX_CONTROL as u64);
	if checksums.size > 64 * 1024 {
		return Err("A lista de checksums excede o limite de tamanho.".into());
	}
	Ok(Some(Package {
		version: version.to_string(),
		archive: Some(archive),
		checksums: Some(checksums),
		zsync,
	}))
}
async fn check_release(
	nightly: bool,
	cancel: Arc<AtomicBool>,
) -> Result<(Option<Package>, Vec<ui::updates::LogEntry>), String> {
	let client = client()?;
	let endpoint = if nightly {
		format!("{RELEASES}?per_page=100")
	} else {
		format!("{RELEASES}/latest")
	};
	let body = match bounded_body(&client, &endpoint, MAX_METADATA, &cancel).await {
		Err(error) if error == RATE_LIMITED && !nightly => {
			return latest_without_api(&client)
				.await
				.map(|package| (package, Vec::new()));
		}
		result => result?,
	};
	let releases = if nightly {
		serde_json::from_slice::<Vec<Release>>(&body)
	} else {
		serde_json::from_slice::<Release>(&body).map(|r| vec![r])
	}
	.map_err(|_| "O GitHub retornou metadados de versão inválidos.".to_owned())?;
	let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
		.map_err(|_| "Esta build tem uma versão inválida.".to_owned())?;
	let log = release_log(&releases, nightly, &current);
	select_release(releases, nightly, &current).map(|package| (package, log))
}
/// Stable check for when the REST API quota of this IP is spent: the newest tag comes from the
/// `releases/latest` redirect and asset sizes from the download responses. The package is still
/// verified against `SHA256SUMS.txt` and its size before installing. No release notes.
async fn latest_without_api(client: &wreq::Client) -> Result<Option<Package>, String> {
	let fallback_error = || RATE_LIMITED.to_owned();
	let probe = wreq::Client::builder()
		.https_only(true)
		.no_proxy()
		.user_agent(concat!("Monkado/", env!("CARGO_PKG_VERSION")))
		.connect_timeout(Duration::from_secs(10))
		.timeout(Duration::from_secs(30))
		.redirect(wreq::redirect::Policy::none())
		.build()
		.map_err(|_| fallback_error())?;
	let response = probe
		.get(LATEST_PAGE)
		.send()
		.await
		.map_err(|_| fallback_error())?;
	let tag = response
		.headers()
		.get(wreq::header::LOCATION)
		.and_then(|value| value.to_str().ok())
		.and_then(latest_tag)
		.ok_or_else(fallback_error)?;
	let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
		.map_err(|_| "Esta build tem uma versão inválida.".to_owned())?;
	if release_version(&tag).is_none_or(|version| version <= current) {
		return Ok(None);
	}
	let mut names = vec!["SHA256SUMS.txt".to_owned()];
	if let Some(archive) = asset_name(&tag) {
		if archive.ends_with(".AppImage") {
			names.push(format!("{archive}.zsync"));
		}
		names.push(archive);
	}
	let mut assets = Vec::with_capacity(names.len());
	for name in names {
		let url = format!("{DOWNLOADS}/{tag}/{name}");
		// A missing asset is left out; `select_release` reports the required ones.
		let Ok(response) = client.head(&url).send().await else {
			continue;
		};
		if let (200, Some(size)) = (response.status().as_u16(), response.content_length()) {
			assets.push(Asset {
				name,
				browser_download_url: url,
				size,
			});
		}
	}
	let release = Release {
		tag_name: tag,
		draft: false,
		prerelease: false,
		assets,
		published_at: None,
		body: None,
	};
	select_release(vec![release], false, &current)
}
/// The tag of a `releases/latest` redirect, only when it points at this repository's tag page.
fn latest_tag(location: &str) -> Option<String> {
	let url = url::Url::parse(location).ok()?;
	if !trusted_url(&url) || url.host_str() != Some("github.com") || url.query().is_some() {
		return None;
	}
	let tag = url
		.path()
		.strip_prefix("/monkey-land/monkado/releases/tag/")?;
	release_version(tag)?;
	Some(tag.to_owned())
}
fn demo_log() -> Vec<ui::updates::LogEntry> {
	[
		(
			"99.0.0",
			"2026-09-30",
			"Message search filters",
			"Fixed voice reconnects after sleep",
		),
		(
			"98.2.0",
			"2026-09-20",
			"Forum gallery view",
			"Reply previews keep their attachment icon",
		),
	]
	.into_iter()
	.map(|(version, date, feature, fix)| ui::updates::LogEntry {
		version: version.into(),
		date: date.into(),
		sections: vec![
			(
				"Features".into(),
				vec![format!("Synthetic preview: {feature}")],
			),
			(
				"Bug Fixes".into(),
				vec![format!("Synthetic preview: {fix}")],
			),
		],
	})
	.collect()
}
fn checksum(body: &[u8], name: &str) -> Result<[u8; 32], String> {
	let body =
		std::str::from_utf8(body).map_err(|_| "O arquivo de checksums é inválido.".to_owned())?;
	let mut result = None;
	for line in body.lines() {
		let mut fields = line.split_whitespace();
		let (Some(hash), Some(file)) = (fields.next(), fields.next()) else {
			continue;
		};
		if file
			.trim_start_matches('*')
			.strip_prefix("./")
			.unwrap_or(file.trim_start_matches('*'))
			!= name
		{
			continue;
		}
		if result.is_some()
			|| fields.next().is_some()
			|| hash.len() != 64
			|| !hash.bytes().all(|b| b.is_ascii_hexdigit())
		{
			return Err("O checksum do pacote é inválido ou está duplicado.".into());
		}
		let mut bytes = [0; 32];
		for (i, byte) in bytes.iter_mut().enumerate() {
			*byte = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16)
				.map_err(|_| "Checksum inválido.".to_owned())?;
		}
		result = Some(bytes);
	}
	result.ok_or_else(|| "A versão não tem checksum para este pacote.".into())
}
async fn download_package(
	package: Package,
	cancel: Arc<AtomicBool>,
	progress: Arc<AtomicU64>,
) -> Result<Outcome, String> {
	// Only reachable on a platform with an in-app installer; `select_release` only omits
	// these when there is nothing to download, and that path never reaches this function.
	let archive = package
		.archive
		.ok_or("Esta plataforma não pode instalar atualizações pelo app.")?;
	let checksums = package
		.checksums
		.ok_or("Esta plataforma não pode instalar atualizações pelo app.")?;
	let client = client()?;
	let checksum_body =
		bounded_body(&client, &checksums.browser_download_url, 64 * 1024, &cancel).await?;
	let expected = checksum(&checksum_body, &archive.name)?;
	let stage = tokio::task::spawn_blocking(install::create_stage)
		.await
		.map_err(|_| "Não foi possível preparar o armazenamento da atualização.".to_owned())??;
	let directory = stage.directory.clone();
	let result = async {
		let reused = if let Some(control) = package.zsync.filter(|_| install::appimage_session()) {
			delta::download(
				&client,
				&archive,
				&control,
				&checksum_body,
				&stage,
				&cancel,
				&progress,
			)
			.await
			.is_ok()
		} else {
			false
		};
		if cancel.load(Ordering::Relaxed) {
			return Err("Atualização cancelada.".into());
		}
		if !reused {
			let partial = directory.join("package.zip");
			match tokio::fs::remove_file(&partial).await {
				Ok(()) => {}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
				Err(_) => return Err("Não foi possível descartar a atualização parcial.".into()),
			}
			progress.store(0, Ordering::Relaxed);
			download_full(&client, &archive, &directory, &expected, &cancel, &progress).await?;
		}
		let path = directory.clone();
		let installation = stage.installation.clone();
		tokio::task::spawn_blocking(move || install::unpack(&path, &installation, &cancel))
			.await
			.map_err(|_| "Não foi possível descompactar a atualização.".to_owned())??;
		Ok(())
	}
	.await;
	match result {
		Ok(()) => Ok(Outcome::Downloaded(stage)),
		Err(error) => {
			tokio::task::spawn_blocking(move || install::cleanup(&directory))
				.await
				.ok();
			Err(error)
		}
	}
}

async fn download_full(
	client: &wreq::Client,
	archive: &Asset,
	directory: &std::path::Path,
	expected: &[u8; 32],
	cancel: &AtomicBool,
	progress: &AtomicU64,
) -> Result<(), String> {
	use tokio::io::AsyncWriteExt;
	let response = response(client, &archive.browser_download_url).await?;
	if response
		.content_length()
		.is_some_and(|size| size != archive.size)
	{
		return Err("O tamanho do pacote não confere com os metadados da versão.".into());
	}
	let mut file = tokio::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(directory.join("package.zip"))
		.await
		.map_err(|_| "Não foi possível criar o download da atualização.".to_owned())?;
	let mut hasher = Sha256::new();
	let mut received = 0_u64;
	let mut response_body = std::pin::pin!(response.bytes_stream());
	while let Some(chunk) = futures_util::StreamExt::next(&mut response_body)
		.await
		.transpose()
		.map_err(|_| "O download da atualização foi interrompido.".to_owned())?
	{
		if cancel.load(Ordering::Relaxed) {
			return Err("Atualização cancelada.".into());
		}
		received = received
			.checked_add(chunk.len() as u64)
			.ok_or("Tamanho da atualização excedido.")?;
		if received > archive.size || received > MAX_DOWNLOAD {
			return Err("O pacote baixado excede o limite de tamanho.".into());
		}
		file.write_all(&chunk).await.map_err(|_| {
			"Não foi possível gravar a atualização. Verifique o espaço livre em disco.".to_owned()
		})?;
		hasher.update(&chunk);
		progress.store(received, Ordering::Relaxed);
	}
	if received != archive.size || hasher.finalize().as_slice() != expected.as_slice() {
		return Err(
			"O checksum ou o tamanho da atualização não conferem. Nada foi instalado.".into(),
		);
	}
	file.sync_all()
		.await
		.map_err(|_| "Não foi possível salvar o pacote de atualização.".to_owned())?;
	drop(file);
	Ok(())
}

/// Offline checks used by the explicit demo debug path, never by release startup.
#[cfg(feature = "demo")]
pub fn debug_check() -> Result<(), String> {
	delta::debug_check()?;
	let stable = release_version("v1.2.3").ok_or("stable version")?;
	let nightly = release_version("v1.2.3-nightly.9.1").ok_or("nightly version")?;
	if nightly >= stable || release_version("v1.2.3/../../bad").is_some() {
		return Err("Version ordering or tag validation failed.".into());
	}
	for address in [
		"http://github.com/x",
		"https://github.com.evil.test/x",
		"https://user@github.com/x",
	] {
		if trusted_url(&url::Url::parse(address).map_err(|_| "URL parse")?) {
			return Err("Untrusted origin accepted.".into());
		}
	}
	let hash = "00".repeat(32);
	if checksum(format!("{hash}  ./package.zip\n").as_bytes(), "package.zip")? != [0; 32]
		|| checksum(
			format!("{hash}  ./package.zip\n{hash}  ./package.zip\n").as_bytes(),
			"package.zip",
		)
		.is_ok()
	{
		return Err("Checksum validation failed.".into());
	}
	let notes = release_notes(
		"## [1.0.0](https://x) (2026-09-28)\n\n### Features\n\n* **voice:** add preview by [@a](https://x) in [#457](https://x) ([f15eac0](https://x))\n",
	);
	if notes
		!= [(
			"Features".to_owned(),
			vec!["voice: add preview by @a in #457".to_owned()],
		)] {
		return Err(format!("Release notes parsed as {notes:?}."));
	}
	install::debug_check()
}

#[cfg(test)]
mod tests {
	#[test]
	fn latest_redirect_accepts_only_this_repositorys_release_tag() {
		let tag = super::latest_tag;
		assert_eq!(
			tag("https://github.com/monkey-land/monkado/releases/tag/v1.6.0").as_deref(),
			Some("v1.6.0")
		);
		for rejected in [
			"https://github.com/monkey-land/monkado/releases",
			"https://github.com/other/monkado/releases/tag/v9.0.0",
			"https://evil.example/monkey-land/monkado/releases/tag/v9.0.0",
			"http://github.com/monkey-land/monkado/releases/tag/v9.0.0",
			"https://github.com/monkey-land/monkado/releases/tag/v9.0.0/../x",
			"https://github.com/monkey-land/monkado/releases/tag/latest",
			"https://github.com/monkey-land/monkado/releases/tag/v9.0.0?x=1",
			"/monkey-land/monkado/releases/tag/v9.0.0",
		] {
			assert_eq!(tag(rejected), None, "{rejected}");
		}
	}
}
