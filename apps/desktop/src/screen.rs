//! Desktop ownership for one explicitly requested screen share.
use client_core::{
	Command, Event, State,
	screen::{self, Settings},
	voice,
};
use discord_voice::{Identity, Status, screen::Worker};
use eframe::egui;
use model::Id;
use std::{
	borrow::Cow,
	sync::{Arc, mpsc},
	time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};
use zeroize::Zeroizing;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Names why a share ended, under the same opt-in variable as the voice reports. A share
/// that stops by itself otherwise leaves only the latest status, which the stop overwrites.
fn note(event: &str, reason: &str) {
	if std::env::var_os("MONKADO_VOICE_DIAGNOSTICS").is_some_and(|value| value == "1") {
		eprintln!("[Monkado voice Screen] {event}={reason}");
	}
}

pub(super) struct Call<'a> {
	pub generation: u64,
	pub channel: Id,
	pub request: u64,
	pub user: Id,
	pub peer: Option<Id>,
	pub session: &'a str,
	pub identity: Arc<Identity>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Context {
	generation: u64,
	channel: Id,
	request: u64,
	stream_request: u64,
}
struct Pending {
	context: Context,
	settings: Settings,
	user: Id,
	peer: Option<Id>,
	session: Zeroizing<String>,
	identity: Arc<Identity>,
	rtc: Option<(Id, Id)>,
	server: Option<(voice::Secret, String)>,
	started: Instant,
}
#[derive(Clone, Copy)]
enum Notice {
	Status(&'static str),
	Failed(&'static str),
}
struct Live {
	context: Context,
	worker: Worker,
	task: JoinHandle<()>,
	events: watch::Receiver<Option<Notice>>,
}
struct Closing {
	context: Context,
	started: Instant,
	timed_out: bool,
	/// The stop never reached Discord, as when signaling was down; it is sent again once the
	/// Gateway is back, since only Discord's stream deletion ends the LIVE state.
	unsent: bool,
}
struct SourceScan {
	generation: u64,
	context: Option<(u64, Id, u64)>,
	receive: mpsc::Receiver<Result<Vec<screen::Source>, &'static str>>,
}

#[derive(Default)]
pub(super) struct Screen {
	pending: Option<Pending>,
	live: Option<Live>,
	closing: Option<Closing>,
	retiring: Option<mpsc::Receiver<Result<(), Cow<'static, str>>>>,
	source_scan: Option<SourceScan>,
	/// Picker previews arriving one by one from the scan thread, for its current sources.
	thumbnails: Option<mpsc::Receiver<(screen::SourceId, image::RgbaImage)>>,
	/// Encoders and audio-playing applications for the picker, from the same scan thread.
	choices: Option<mpsc::Receiver<(Vec<screen::Encoder>, Vec<String>)>>,
	command: Option<Command>,
	sequence: u64,
	/// Owned when it carries a run-time detail such as a Windows error code.
	status: Cow<'static, str>,
	/// Outcome to state once as a toast on the next poll; the stage keeps no status line.
	toast: Option<(ui::design::Level, Cow<'static, str>)>,
	/// Last viewer roster Discord reported for the own stream, and a cue waiting for `poll`.
	viewers: Option<(Context, screen::Viewers)>,
	viewer_cue: bool,
	share_cued: bool,
	/// Monkado P2P screen and share-audio tracks; when set, sharing publishes there and
	/// Discord never carries it.
	p2p: Option<(discord_voice::p2p::Track, discord_voice::p2p::Track)>,
	/// The live share is on the P2P track, so stopping it needs no Discord acknowledgement.
	p2p_live: bool,
}
impl Screen {
	pub fn stop(&mut self) {
		self.pending = None;
		self.command = None;
		self.closing = None;
		self.viewers = None;
		self.viewer_cue = false;
		// Keep one in-flight OS enumeration tracked until it completes.
		self.retire_live();
		self.status = "Compartilhamento de tela encerrado".into();
	}

	pub fn use_p2p(&mut self, track: discord_voice::p2p::Track, audio: discord_voice::p2p::Track) {
		self.p2p = Some((track, audio));
	}

	/// Uploads the previews that arrived since the last frame, one texture per source.
	fn receive_thumbnails(&mut self, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		let Some(previews) = &self.thumbnails else {
			return;
		};
		loop {
			match previews.try_recv() {
				Ok((id, preview)) => {
					let image = egui::ColorImage::from_rgba_unmultiplied(
						[preview.width() as usize, preview.height() as usize],
						preview.as_raw(),
					);
					let texture = ctx.load_texture(
						format!("screen-source-{id:?}"),
						image,
						egui::TextureOptions::LINEAR,
					);
					ui.screen.thumbnails.retain(|(known, _)| *known != id);
					ui.screen.thumbnails.push((id, texture));
				}
				Err(mpsc::TryRecvError::Empty) => break,
				Err(mpsc::TryRecvError::Disconnected) => {
					self.thumbnails = None;
					break;
				}
			}
		}
	}

	fn retire_live(&mut self) {
		self.p2p_live = false;
		if let Some(live) = self.live.take() {
			live.task.abort();
			self.retiring = Some(live.worker.shutdown());
		}
	}

	fn context(&self) -> Option<Context> {
		self.pending
			.as_ref()
			.map(|pending| pending.context)
			.or_else(|| self.live.as_ref().map(|live| live.context))
			.or_else(|| self.closing.as_ref().map(|closing| closing.context))
	}

	/// Stops sharing because something went wrong, and says so once.
	fn fail(&mut self, message: impl Into<Cow<'static, str>>) {
		let message = message.into();
		// The share dialog already shows this reason; the log keeps it for a report too. The
		// log takes fixed text only, so a reason built at run time is recorded by a label.
		let reason: &'static str = match &message {
			Cow::Borrowed(reason) => reason,
			Cow::Owned(_) => "O compartilhamento de tela parou por um erro do sistema",
		};
		diagnostics::record_fields(
			diagnostics::Category::Voice,
			diagnostics::Level::Error,
			"share_failed",
			&[("reason", diagnostics::Value::Text(reason))],
		);
		self.toast = Some((ui::design::Level::Error, message.clone()));
		self.request_stop(message);
	}

	/// Starts closing the share, or only records the reason when no call owns it.
	fn request_stop(&mut self, message: impl Into<Cow<'static, str>>) {
		let message = message.into();
		note("share_stopped", &message);
		let Some(context) = self.context() else {
			self.status = message;
			return;
		};
		self.close(context, message);
	}

	/// Retires the live share, asks Discord to stop the stream, and shows `message`.
	fn close(&mut self, context: Context, message: impl Into<Cow<'static, str>>) {
		self.pending = None;
		if self.p2p_live {
			self.retire_live();
			self.status = message.into();
			return;
		}
		self.retire_live();
		if self.closing.is_none() {
			self.closing = Some(Closing {
				context,
				started: Instant::now(),
				timed_out: false,
				unsent: false,
			});
			self.command = Some(Command::Voice(voice::Command::StopStream {
				channel: context.channel,
				request: context.request,
				stream_request: context.stream_request,
			}));
		}
		self.status = message.into();
	}

	/// Take matching stream secrets before core reduction. Nothing is persisted.
	pub fn observe(&mut self, state: &State, event: &mut Event) {
		let Event::Voice(voice::Event::Stream {
			channel,
			request,
			stream_request,
			event,
		}) = event
		else {
			return;
		};
		let incoming = Context {
			generation: state.generation,
			channel: *channel,
			request: *request,
			stream_request: *stream_request,
		};
		if self.context() != Some(incoming) {
			return;
		}
		match event {
			screen::Event::Created {
				rtc_server,
				rtc_channel,
			} => {
				if let Some(pending) = &mut self.pending {
					pending.rtc = Some((*rtc_server, *rtc_channel));
				}
			}
			screen::Event::Server { token, endpoint } => {
				if self.live.is_some() {
					let _ = token.take();
					let _ = endpoint.take();
					self.fail("O Discord trocou o servidor do compartilhamento de tela");
					return;
				}
				let Some(pending) = &mut self.pending else {
					return;
				};
				let Some(endpoint) = endpoint.take() else {
					let _ = token.take();
					self.fail(
						"O servidor de compartilhamento de tela do Discord está indisponível",
					);
					return;
				};
				let Some(token) = token.take() else {
					self.fail("O Discord não enviou o token do compartilhamento de tela");
					return;
				};
				pending.server = Some((token, endpoint));
			}
			screen::Event::Deleted { reason } => {
				self.pending = None;
				self.retire_live();
				self.closing = None;
				// A server-side end names its cause, so it is never mistaken for a local stop.
				self.status = reason
					.unwrap_or("Compartilhamento de tela encerrado")
					.into();
				if let Some(reason) = reason {
					self.toast = Some((ui::design::Level::Info, (*reason).into()));
				}
			}
			screen::Event::Failed(message) => match &mut self.closing {
				// Only the stop is in flight, so this is that stop being refused.
				Some(closing) if self.pending.is_none() && self.live.is_none() => {
					note("stop_unsent", message);
					closing.unsent = true;
				}
				_ => self.fail(*message),
			},
			screen::Event::Viewers(viewers) => {
				// As in Discord: chime when this stream gains a viewer while it had at most 25.
				let previous = self
					.viewers
					.as_ref()
					.filter(|(context, _)| *context == incoming)
					.map_or(0, |(_, viewers)| viewers.count);
				let count = viewers.count;
				self.viewer_cue |= self.live.is_some() && count > previous && previous <= 25;
				self.viewers = Some((incoming, std::mem::take(viewers)));
			}
		}
	}

	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		call: Option<Call<'_>>,
	) -> Option<Command> {
		if self
			.retiring
			.as_ref()
			.is_some_and(|done| !matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)))
		{
			self.retiring = None;
		}
		if state.demo {
			if self.context().is_some() || self.live.is_some() {
				self.stop();
			}
			ui.screen.refresh_requested = false;
			ui.screen.request = None;
			ui.screen.busy = false;
			ui.screen.sharing = false;
			ui.screen.supported = false;
			ui.screen.preview = None;
			ui.screen.preview_paused = false;
			ui.screen.capture_status = None;
			return None;
		}
		ui.screen.supported = discord_voice::screen::supported();
		ui.screen.switchable = self.p2p_live;
		let cue = model::notification_preferences::Sound::ViewerJoin;
		if std::mem::take(&mut self.viewer_cue) && ui.notification_options.allows(cue) {
			ui.notification_preview = Some(cue);
			ctx.request_repaint();
		}
		if !ui.screen.open && ui.screen.request.is_none() && self.context().is_none() {
			ui.screen.sources.clear();
			ui.screen.selected = None;
		}
		if !ui.screen.open {
			self.thumbnails = None;
			self.choices = None;
			ui.screen.thumbnails.clear();
		}
		self.receive_thumbnails(ui, ctx);
		if let Some(choices) = &self.choices {
			match choices.try_recv() {
				Ok((encoders, applications)) => {
					ui.screen.encoders = encoders;
					ui.screen.audio_applications = applications;
					self.choices = None;
				}
				Err(mpsc::TryRecvError::Empty) => {}
				Err(mpsc::TryRecvError::Disconnected) => self.choices = None,
			}
		}

		if ui.screen.refresh_requested {
			ui.screen.refresh_requested = false;
			if !state.demo && self.source_scan.is_none() && ui.screen.supported {
				let (send, receive) = mpsc::sync_channel(1);
				// A few previews in flight; the thread waits for the UI to take them.
				let (preview_send, previews) = mpsc::sync_channel(4);
				let (choices_send, choices) = mpsc::sync_channel(1);
				self.choices = Some(choices);
				let wake = ctx.clone();
				self.status = "Procurando telas e janelas…".into();
				self.thumbnails = Some(previews);
				ui.screen.thumbnails.clear();
				match std::thread::Builder::new()
					.name("screen-sources".into())
					.spawn(move || {
						let sources = discord_voice::screen::sources();
						let ids: Vec<screen::SourceId> = sources
							.as_ref()
							.map(|sources| sources.iter().map(|source| source.id).collect())
							.unwrap_or_default();
						let _ = send.send(sources);
						wake.request_repaint();
						let _ = choices_send.send((
							discord_voice::screen::encoders(),
							discord_voice::screen::audio_applications(),
						));
						wake.request_repaint();
						for id in ids {
							let Some(preview) = discord_voice::screen::thumbnail(id) else {
								continue;
							};
							// The picker closed or rescanned: stop drawing previews.
							if preview_send.send((id, preview)).is_err() {
								break;
							}
							wake.request_repaint();
						}
					}) {
					Ok(_) => {
						self.source_scan = Some(SourceScan {
							generation: state.generation,
							context: ui.screen.context,
							receive,
						})
					}
					Err(_) => self.status = "Não foi possível começar a procurar telas".into(),
				}
			} else if !ui.screen.supported {
				self.status =
					"O compartilhamento de tela não está disponível nesta plataforma".into();
			}
		}
		if let Some(scan) = &self.source_scan {
			match scan.receive.try_recv() {
				Ok(_)
					if !ui.screen.open
						|| scan.generation != state.generation
						|| scan.context != ui.screen.context =>
				{
					self.source_scan = None;
					if ui.screen.open {
						ui.screen.refresh_requested = true;
						ctx.request_repaint();
					}
				}
				Ok(Ok(sources)) => {
					ui.screen.sources = sources;
					ui.screen.selected = ui
						.screen
						.selected
						.filter(|selected| {
							ui.screen
								.sources
								.iter()
								.any(|source| source.id == *selected)
						})
						.or_else(|| ui.screen.sources.first().map(|source| source.id));
					self.status = if ui.screen.sources.is_empty() {
						"Nenhuma tela ou janela compartilhável foi encontrada".into()
					} else if cfg!(target_os = "linux") {
						"Use o seletor do sistema ou, no X11, compartilhe a área de trabalho inteira"
							.into()
					} else {
						"Escolha uma tela ou janela".into()
					};
					self.source_scan = None;
				}
				Ok(Err(error)) => {
					self.status = error.into();
					self.source_scan = None;
				}
				Err(mpsc::TryRecvError::Disconnected) => {
					self.status = "A busca por telas parou".into();
					self.source_scan = None;
				}
				Err(mpsc::TryRecvError::Empty) => {}
			}
		}

		let call_context = call
			.as_ref()
			.map(|call| (call.generation, call.channel, call.request));
		if let Some(current) = self.context()
			&& call_context != Some((current.generation, current.channel, current.request))
		{
			self.stop();
		}
		if let Some(context) = self
			.pending
			.as_ref()
			.map(|pending| pending.context)
			.or_else(|| self.live.as_ref().map(|live| live.context))
			&& !state.has_stream_access(context.channel)
		{
			self.fail(
				"A permissão de compartilhar tela foi removida; encerrando o compartilhamento",
			);
		}

		if let Some(request) = ui.screen.request.take() {
			match request {
				ui::screen::Request::Start(settings) => {
					let requested = ui.screen.context;
					let valid = settings
						.valid()
						.then_some(())
						.and(call.as_ref())
						.filter(|call| {
							requested == Some((call.generation, call.channel, call.request))
								&& state.generation == call.generation
								&& state.can_stream(call.channel)
								&& state.voice.active.as_ref().is_some_and(|active| {
									active.channel == call.channel
										&& active.request == call.request
										&& matches!(
											active.phase,
											voice::Phase::Connected | voice::Phase::Waiting
										)
								})
						});
					// A live P2P share switches source in place: viewers keep watching.
					let switching = self.p2p_live && self.live.is_some();
					if !switching && (self.context().is_some() || self.retiring.is_some()) {
						self.status =
							"O compartilhamento de tela anterior ainda está fechando".into();
						self.toast = Some((ui::design::Level::Warning, self.status.clone()));
					} else if let Some(call) = valid {
						self.sequence = self.sequence.wrapping_add(1).max(1);
						let context = Context {
							generation: call.generation,
							channel: call.channel,
							request: call.request,
							stream_request: self.sequence,
						};
						if let Some(track) = self.p2p.clone() {
							if switching {
								self.retire_live();
							}
							if let Err(error) =
								self.start_p2p(runtime, context, settings, track, ctx)
							{
								self.status = error.into();
								self.toast = Some((ui::design::Level::Error, self.status.clone()));
							}
						} else {
							self.pending = Some(Pending {
								context,
								settings,
								user: call.user,
								peer: call.peer,
								session: Zeroizing::new(call.session.to_owned()),
								identity: call.identity.clone(),
								rtc: None,
								server: None,
								started: Instant::now(),
							});
							self.status = "Solicitando conexão para compartilhar a tela…".into();
							self.command = Some(Command::Voice(voice::Command::StartStream {
								channel: context.channel,
								request: context.request,
								stream_request: context.stream_request,
							}));
						}
					} else {
						self.status =
							"Para compartilhar a tela é preciso estar conectado à chamada".into();
						self.toast = Some((ui::design::Level::Error, self.status.clone()));
					}
				}
				ui::screen::Request::Stop => {
					// Discord cues the end of this device's own share, like its start.
					let cue = model::notification_preferences::Sound::ScreenShareOff;
					if self.live.is_some() && ui.notification_options.allows(cue) {
						ui.notification_preview = Some(cue);
						ctx.request_repaint();
					}
					self.request_stop("Encerrando o compartilhamento de tela…")
				}
			}
		}

		if self
			.pending
			.as_ref()
			.is_some_and(|pending| pending.started.elapsed() >= SIGNAL_TIMEOUT)
		{
			self.fail(
				"O Discord não enviou os dados da conexão de compartilhamento de tela em 30 segundos",
			);
		}
		if let Some(pending) = &self.pending {
			ctx.request_repaint_after(SIGNAL_TIMEOUT.saturating_sub(pending.started.elapsed()));
		}

		if self
			.pending
			.as_ref()
			.is_some_and(|pending| pending.rtc.is_some() && pending.server.is_some())
		{
			self.finish_start(runtime, ctx);
		}

		let mut failure: Option<Cow<'static, str>> = None;
		if let Some(live) = &mut self.live {
			match *live.events.borrow_and_update() {
				Some(Notice::Status(status)) => self.status = status.into(),
				Some(Notice::Failed(error)) => failure = Some(error.into()),
				None => {}
			}
			if failure.is_none() {
				failure = live.worker.result().map(|result| {
					result
						.err()
						.unwrap_or(Cow::Borrowed("A captura de tela parou"))
				});
			}
			if live.task.is_finished() && failure.is_none() {
				failure = Some("A conexão do compartilhamento de tela terminou".into());
			}
		}
		if let Some(error) = failure {
			self.fail(error);
		}
		ui.screen.sharing = self
			.live
			.as_ref()
			.is_some_and(|live| live.worker.captured());
		// The worker is live while the Linux system picker is still open.
		if self
			.live
			.as_ref()
			.is_some_and(|live| live.worker.captured())
			&& !self.share_cued
		{
			self.share_cued = true;
			let cue = model::notification_preferences::Sound::ScreenShareOn;
			if ui.notification_options.allows(cue) {
				ui.notification_preview = Some(cue);
				ctx.request_repaint();
			}
		}
		if self.live.is_none() {
			self.share_cued = false;
		}
		if let Some(live) = &self.live {
			let on_channel = state.selected == Some(live.context.channel);
			let shown = on_channel
				&& ctx.input(|input| input.focused && !input.viewport().minimized.unwrap_or(false));
			live.worker.set_preview_visible(shown);
			ui.screen.preview_paused = !shown;
			if !shown {
				// The pause message covers the tile. Keeping the texture stops the tile from
				// unmounting, and the worker republishes its retained frame when preview returns.
				let _ = live.worker.take_preview();
			} else if let Some(frame) = live.worker.take_preview() {
				let image = egui::ColorImage::from_rgba_unmultiplied(
					[frame.width() as usize, frame.height() as usize],
					frame.as_raw(),
				);
				if let Some(texture) = &mut ui.screen.preview {
					texture.set(image, egui::TextureOptions::LINEAR);
				} else {
					ui.screen.preview =
						Some(ctx.load_texture("local-screen", image, egui::TextureOptions::LINEAR));
				}
			}
		} else {
			ui.screen.preview = None;
			ui.screen.preview_paused = false;
		}

		if let Some(closing) = &mut self.closing
			&& closing.unsent
			&& state.gateway_connected
			&& self.command.is_none()
		{
			closing.unsent = false;
			closing.started = Instant::now();
			closing.timed_out = false;
			self.command = Some(Command::Voice(voice::Command::StopStream {
				channel: closing.context.channel,
				request: closing.context.request,
				stream_request: closing.context.stream_request,
			}));
		}
		if let Some(closing) = &mut self.closing
			&& !closing.unsent
		{
			if !closing.timed_out && closing.started.elapsed() >= SIGNAL_TIMEOUT {
				closing.timed_out = true;
				self.status = "O Discord não confirmou o fim do compartilhamento de tela; saia da chamada antes de compartilhar de novo".into();
				self.toast = Some((ui::design::Level::Warning, self.status.clone()));
			}
			if !closing.timed_out {
				ctx.request_repaint_after(SIGNAL_TIMEOUT.saturating_sub(closing.started.elapsed()));
			}
		}
		ui.screen.busy = self.pending.is_some()
			|| self.live.is_some()
			|| self.closing.is_some()
			|| self.retiring.is_some();
		// Only the live share lists viewers; it changes rarely, so copy it only then.
		let live = self.live.as_ref().map(|live| live.context);
		let roster = self
			.viewers
			.as_ref()
			.filter(|(context, _)| Some(*context) == live)
			.map(|(_, viewers)| viewers);
		if ui.screen.viewers.as_ref() != roster {
			ui.screen.viewers = roster.cloned();
		}
		if let Some((level, message)) = self.toast.take() {
			ui.toasts.push(level, message);
		}
		ui.screen.status = self.status.clone();
		ui.screen.capture_status = self
			.live
			.as_ref()
			.and_then(|live| live.worker.capture_status());
		self.command.take()
	}
	fn finish_start(&mut self, runtime: &Runtime, ctx: &egui::Context) {
		let pending = self.pending.take().expect("complete screen negotiation");
		let context = pending.context;
		if let Err(error) = self.start(runtime, pending, ctx) {
			self.toast = Some((ui::design::Level::Error, error.into()));
			self.close(context, error);
		}
	}

	/// Captures and encodes as for Discord, but publishes the picture and the requested
	/// system audio on the P2P tracks.
	fn start_p2p(
		&mut self,
		runtime: &Runtime,
		context: Context,
		settings: Settings,
		(track, audio): (discord_voice::p2p::Track, discord_voice::p2p::Track),
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let wake = ctx.clone();
		let (worker, video) = Worker::start(settings, move || wake.request_repaint())?;
		let (send, events) = watch::channel(Some(Notice::Status("Compartilhando sua tela")));
		let task = runtime.spawn(async move {
			discord_voice::p2p::publish_screen(track, audio, video).await;
			drop(send);
		});
		self.status = "Compartilhando sua tela".into();
		self.p2p_live = true;
		self.live = Some(Live {
			context,
			worker,
			task,
			events,
		});
		Ok(())
	}

	fn start(
		&mut self,
		runtime: &Runtime,
		mut pending: Pending,
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let (rtc_server, rtc_channel) = pending.rtc.take().ok_or("Missing screen RTC identity")?;
		let (token, endpoint) = pending.server.take().ok_or("Missing screen server")?;
		let session = voice::Secret::new(pending.session.to_string())
			.map_err(|_| "Invalid screen voice session")?;
		let credentials = voice::VoiceConnection {
			channel: rtc_channel,
			guild: Some(rtc_server),
			user: pending.user,
			peer: pending.peer,
			session,
			token,
			endpoint,
			request: pending.context.stream_request,
		};
		let wake = ctx.clone();
		let (worker, video) = Worker::start(pending.settings, move || wake.request_repaint())?;
		let (send, events) = watch::channel(None);
		let wake = ctx.clone();
		let identity = pending.identity;
		let task = runtime.spawn(async move {
			let (status_send, status_wake) = (send.clone(), wake.clone());
			let result = discord_voice::run_stream(credentials, identity, video, move |event| {
				let status = match event {
					Status::Connecting => "Conectando o compartilhamento de tela…",
					Status::Discovering => "Verificando a rede do compartilhamento de tela…",
					Status::TransportReady | Status::Securing => "Protegendo o vídeo da tela…",
					Status::WaitingForPeer => "Prévia da tela · aguardando os outros",
					Status::Ready { .. } => "Compartilhando sua tela",
					Status::RemoteAudio
					| Status::Speaking(_)
					| Status::CameraAvailable(_)
					| Status::Latency(_) => {
						return Ok(());
					}
				};
				status_send.send_replace(Some(Notice::Status(status)));
				status_wake.request_repaint();
				Ok(())
			})
			.await;
			match result {
				Ok(()) => note("stream_transport_stopped", "ok"),
				Err(error) => {
					note("stream_transport_stopped", error);
					send.send_replace(Some(Notice::Failed(error)));
				}
			}
			wake.request_repaint();
		});
		self.status = "Conectando o compartilhamento de tela…".into();
		self.live = Some(Live {
			context: pending.context,
			worker,
			task,
			events,
		});
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn waiting_dm_and_guild_calls_can_request_sharing_without_opening_capture() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let ctx = egui::Context::default();
		for channel in [Id(22), Id(25)] {
			let mut state = test_support::demo_state();
			state.demo = false;
			let mut role = state.permissions.guilds[&Id(10)].roles.as_ref().unwrap()[0].clone();
			role.bits |= model::permissions::STREAM;
			state.apply(client_core::Envelope {
				generation: state.generation,
				event: Event::Permissions(client_core::permissions::Event::Role {
					guild: Id(10),
					role,
				}),
			});
			state.start_call(channel, false).unwrap();
			let active = state.voice.active.as_mut().unwrap();
			active.phase = voice::Phase::Waiting;
			let request = active.request;
			let mut ui = ui::MessagingUi::default();
			ui.screen.context = Some((state.generation, channel, request));
			ui.screen.request = Some(ui::screen::Request::Start(Settings {
				source: screen::SourceId::Display(1),
				width: 1280,
				height: 720,
				fps: 30,
				cursor: true,
				audio: false,
				audio_apps: Default::default(),
				encoder: Default::default(),
			}));
			let call = Call {
				generation: state.generation,
				channel,
				request,
				user: Id(1),
				peer: (channel == Id(22)).then_some(Id(2)),
				session: "synthetic-session",
				identity: Identity::generate(),
			};
			let mut screen = Screen::default();
			assert!(matches!(
				screen.poll(&runtime, &state, &mut ui, &ctx, Some(call)),
				Some(Command::Voice(voice::Command::StartStream { .. }))
			));
			assert!(screen.pending.is_some());
			assert!(screen.live.is_none());

			// A routine Gateway reconnect must not read as a revoked share permission.
			for event in [Event::Disconnected, Event::Resumed] {
				state.apply(client_core::Envelope {
					generation: state.generation,
					event,
				});
				let call = Call {
					generation: state.generation,
					channel,
					request,
					user: Id(1),
					peer: (channel == Id(22)).then_some(Id(2)),
					session: "synthetic-session",
					identity: Identity::generate(),
				};
				assert!(
					screen
						.poll(&runtime, &state, &mut ui, &ctx, Some(call))
						.is_none()
				);
				assert!(screen.pending.is_some() && screen.closing.is_none());
			}
		}
	}

	fn pending(context: Context, settings: Settings) -> Pending {
		Pending {
			context,
			settings,
			user: Id(1),
			peer: None,
			session: Zeroizing::new("synthetic-session".into()),
			identity: Identity::generate(),
			rtc: Some((Id(30), Id(31))),
			server: Some((
				voice::Secret::new("synthetic-token".into()).unwrap(),
				"voice.discord.media:443".into(),
			)),
			started: Instant::now(),
		}
	}

	#[test]
	fn startup_failure_and_permission_revocation_request_stream_cleanup() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let ctx = egui::Context::default();
		let context = Context {
			generation: State::default().generation,
			channel: Id(20),
			request: 7,
			stream_request: 8,
		};
		let mut screen = Screen {
			pending: Some(pending(
				context,
				Settings {
					source: screen::SourceId::Display(1),
					width: 1,
					height: 1,
					fps: 30,
					cursor: true,
					audio: false,
					audio_apps: Default::default(),
					encoder: Default::default(),
				},
			)),
			..Screen::default()
		};
		screen.finish_start(&runtime, &ctx);
		assert!(screen.pending.is_none() && screen.closing.is_some());
		assert!(matches!(
			screen.command.take(),
			Some(Command::Voice(voice::Command::StopStream {
				stream_request: 8,
				..
			}))
		));

		let mut state = State::default();
		state.voice.active = Some(voice::Call {
			channel: Id(20),
			guild: Some(Id(10)),
			connected_at: Some(Instant::now()),
			channel_started_at: None,
			server_muted: false,
			server_deafened: false,
			request: 7,
			phase: voice::Phase::Connected,
			muted: false,
			deafened: false,
			participants: vec![],
			error: None,
			reconnecting: None,
			camera: false,
			watching: None,
		});
		let mut screen = Screen {
			pending: Some(Pending {
				rtc: None,
				server: None,
				..pending(
					context,
					Settings {
						source: screen::SourceId::Display(1),
						width: 1280,
						height: 720,
						fps: 30,
						cursor: true,
						audio: false,
						audio_apps: Default::default(),
						encoder: Default::default(),
					},
				)
			}),
			..Screen::default()
		};
		let mut ui = ui::MessagingUi::default();
		let call = Call {
			generation: state.generation,
			channel: Id(20),
			request: 7,
			user: Id(1),
			peer: None,
			session: "synthetic-session",
			identity: Identity::generate(),
		};
		assert!(matches!(
			screen.poll(&runtime, &state, &mut ui, &ctx, Some(call)),
			Some(Command::Voice(voice::Command::StopStream {
				stream_request: 8,
				..
			}))
		));
		assert!(ui.screen.busy);
		assert!(ui.screen.status.contains("permissão"));

		// Signaling refused that stop: it must go out again after reconnecting, or Discord keeps
		// the stream LIVE and the share button stays stuck.
		let call = || Call {
			generation: state.generation,
			channel: Id(20),
			request: 7,
			user: Id(1),
			peer: None,
			session: "synthetic-session",
			identity: Identity::generate(),
		};
		let mut refused = Event::Voice(voice::Event::Stream {
			channel: Id(20),
			request: 7,
			stream_request: 8,
			event: screen::Event::Failed(
				"Voice signaling is disconnected; screen-share action was not sent",
			),
		});
		screen.observe(&state, &mut refused);
		assert!(!state.gateway_connected);
		assert!(
			screen
				.poll(&runtime, &state, &mut ui, &ctx, Some(call()))
				.is_none()
		);
		assert!(ui.screen.busy);
		state.gateway_connected = true;
		assert!(matches!(
			screen.poll(&runtime, &state, &mut ui, &ctx, Some(call())),
			Some(Command::Voice(voice::Command::StopStream {
				stream_request: 8,
				..
			}))
		));
		assert!(
			screen
				.poll(&runtime, &state, &mut ui, &ctx, Some(call()))
				.is_none()
		);
	}
}
