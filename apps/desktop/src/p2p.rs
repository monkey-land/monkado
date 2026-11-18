//! Desktop ownership of this device's Monkado P2P presence in the current call
//! (`discord_voice::p2p`). Peers' shares and cameras appear through the ordinary call UI:
//! the live badge, camera tiles and the stream stage. Pictures are never stored.
use client_core::{
	Event, State,
	voice::{self, P2pMedia, Phase},
};
use discord_voice::p2p::{Mesh, MeshConfig, Track};
use eframe::egui;
use model::Id;
use std::{
	sync::{Arc, Mutex},
	time::Instant,
};
use tokio::runtime::Runtime;

/// `MONKADO_P2P=0` turns P2P off entirely: screen share and camera go to Discord as upstream.
pub(super) fn enabled() -> bool {
	std::env::var_os("MONKADO_P2P").is_none_or(|value| value != "0")
}

/// Voice session tags kept across calls; one call needs at most its 64 participants.
const MAX_TAGS: usize = 128;

type Cameras = Arc<Mutex<Option<Arc<Mutex<crate::voice::RemotePictures>>>>>;
type StreamAudio = Arc<Mutex<Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>>>;

struct Running {
	channel: Id,
	tag: [u8; 32],
	mesh: Mesh,
	members: Vec<u64>,
	watching: Vec<u64>,
}

/// One connected Monkado peer and what it announces.
#[derive(Clone, Copy)]
struct Peer {
	user: Id,
	screen: bool,
	camera: bool,
	screen_since: Option<Instant>,
}

#[derive(Default)]
pub(super) struct P2p {
	screen: Track,
	screen_audio: Track,
	camera: Track,
	running: Option<Running>,
	tags: Vec<(Id, Id, [u8; 32])>,
	peers: Vec<Peer>,
	/// Latest decoded picture of every watched P2P share.
	streams: Arc<Mutex<crate::voice::RemotePictures>>,
	textures: Vec<(Id, crate::gpu_video::VideoTexture)>,
	/// Where peers' camera pictures go: the live call's remote pictures, which change on rejoin.
	cameras: Cameras,
	/// Where the watched peer's share audio goes: the live call's stream playout.
	stream_audio: StreamAudio,
	/// First time this device saw a Discord-only share live, for its elapsed-time label.
	seen_live: Vec<(Id, Instant)>,
}
impl P2p {
	/// Records a participant's voice session tag before core reduction.
	pub fn event(&mut self, event: &Event) {
		let Event::Voice(voice::Event::PeerSession { channel, user, tag }) = event else {
			return;
		};
		self.tags
			.retain(|(known, peer, _)| (known, peer) != (channel, user));
		if self.tags.len() >= MAX_TAGS {
			self.tags.remove(0);
		}
		self.tags.push((*channel, *user, *tag));
		if let Some(running) = self.running.as_ref().filter(|r| r.channel == *channel) {
			running.mesh.peer(user.0, *tag);
		}
	}

	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		voice: &mut crate::voice::Voice,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
	) {
		if !enabled() {
			ui.voice_monkado_users.clear();
			ui.voice_stream_views.clear();
			ui.voice_live_since.clear();
			return;
		}
		voice.use_p2p(&self.screen, &self.screen_audio, &self.camera);
		if let Ok(mut cameras) = self.cameras.lock() {
			*cameras = voice.remote_video();
		}
		if let Ok(mut stream_audio) = self.stream_audio.lock() {
			*stream_audio = voice.stream_audio();
		}
		let call = state
			.voice
			.active
			.as_ref()
			.filter(|call| matches!(call.phase, Phase::Connected | Phase::Waiting));
		self.camera.set_live(call.is_some_and(|call| call.camera));
		let wanted = call.zip(state.user.as_ref()).and_then(|(call, me)| {
			let (_, _, tag) = self
				.tags
				.iter()
				.find(|(channel, user, _)| *channel == call.channel && *user == me.id)?;
			Some((call.channel, me.id, *tag))
		});
		let members: Vec<u64> = call
			.map(|call| call.participants.iter().map(|p| p.user.0).collect())
			.unwrap_or_default();
		if self.running.as_ref().map(|r| (r.channel, r.tag))
			!= wanted.map(|(channel, _, tag)| (channel, tag))
		{
			self.running = None;
			self.peers.clear();
			if let Some((channel, me, tag)) = wanted {
				self.start(runtime, channel, me, tag, ctx);
			}
		}
		let me = state.user.as_ref().map(|user| user.id);
		let Some(running) = &mut self.running else {
			state.set_p2p_media(Vec::new());
			ui.voice_monkado_users.clear();
			ui.voice_stream_views.clear();
			ui.screen.viewers.clear();
			self.textures.clear();
			self.live_times(state, ui, me);
			return;
		};
		ui.screen.viewers = if self.screen.live() {
			running.mesh.screen_viewers().into_iter().map(Id).collect()
		} else {
			Vec::new()
		};
		if running.members != members {
			running.mesh.members(members.clone());
			running.members = members;
		}
		for update in running.mesh.updates() {
			self.peers.retain(|peer| peer.user.0 != update.user);
			if update.linked && self.peers.len() < voice::MAX_PARTICIPANTS {
				self.peers.push(Peer {
					user: Id(update.user),
					screen: update.screen,
					camera: update.camera,
					screen_since: update.screen_since,
				});
			}
		}
		// This device's own share and camera show as for Discord-announced media, too.
		let mut media: Vec<P2pMedia> = self
			.peers
			.iter()
			.filter(|peer| peer.screen || peer.camera)
			.map(|peer| P2pMedia {
				user: peer.user,
				streaming: peer.screen,
				video: peer.camera,
			})
			.collect();
		if let Some(me) = me
			&& (self.screen.live() || self.camera.live())
		{
			media.push(P2pMedia {
				user: me,
				streaming: self.screen.live(),
				video: self.camera.live(),
			});
		}
		state.set_p2p_media(media);
		ui.voice_monkado_users = self.peers.iter().map(|peer| peer.user).chain(me).collect();
		// Every P2P share chosen on the stage: the main one and the extras beside it.
		let peers = &self.peers;
		let p2p_streamer = |user: &Id| peers.iter().any(|peer| peer.user == *user && peer.screen);
		let primary = state.voice.active.as_ref().and_then(|call| call.watching);
		let watching: Vec<u64> = primary
			.into_iter()
			.chain(ui.voice_also_watching.iter().copied())
			.filter(p2p_streamer)
			.map(|user| user.0)
			.collect();
		if running.watching != watching {
			running.mesh.watch(watching.clone());
			running.watching = watching.clone();
			if let Ok(mut streams) = self.streams.lock() {
				streams.retain(|(user, _, _)| watching.contains(user));
			}
			self.textures.retain(|(user, _)| watching.contains(&user.0));
			ui.voice_stream_views
				.retain(|(user, _)| watching.contains(&user.0));
		}
		self.upload_streams(ui, ctx);
		if let Some(primary) = primary.filter(|user| watching.contains(&user.0)) {
			let shown = ui
				.voice_stream_views
				.iter()
				.any(|(user, _)| *user == primary);
			ui.voice_stream_status = if shown {
				"Assistindo à transmissão"
			} else {
				"Conectando à transmissão…"
			};
		}
		self.live_times(state, ui, me);
	}

	/// Uploads changed pictures of watched P2P shares into their textures.
	fn upload_streams(&mut self, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		let Ok(mut streams) = self.streams.try_lock() else {
			return;
		};
		for (user, picture, dirty) in streams.iter_mut() {
			if !std::mem::take(dirty) {
				continue;
			}
			let user = Id(*user);
			let index = match self.textures.iter().position(|(id, _)| *id == user) {
				Some(index) => index,
				None => {
					self.textures.push((user, Default::default()));
					self.textures.len() - 1
				}
			};
			let name = format!("p2p-stream-{}", user.0);
			let Some(texture) = self.textures[index].1.upload(ctx, &name, picture.borrow()) else {
				continue;
			};
			match ui.voice_stream_views.iter_mut().find(|(id, _)| *id == user) {
				Some((_, shown)) => *shown = texture,
				None => ui.voice_stream_views.push((user, texture)),
			}
		}
	}

	/// When each live share in the call started: P2P peers report it, this device knows its
	/// own, and Discord-only shares count from when this device first saw them.
	fn live_times(&mut self, state: &State, ui: &mut ui::MessagingUi, me: Option<Id>) {
		let streaming: Vec<Id> = state
			.voice
			.active
			.as_ref()
			.map(|call| {
				call.participants
					.iter()
					.filter(|participant| participant.streaming)
					.map(|participant| participant.user)
					.collect()
			})
			.unwrap_or_default();
		self.seen_live.retain(|(user, _)| streaming.contains(user));
		let now = Instant::now();
		let mut since = Vec::with_capacity(streaming.len());
		for user in streaming {
			let known = if Some(user) == me {
				self.screen.live_since()
			} else {
				self.peers
					.iter()
					.find(|peer| peer.user == user)
					.and_then(|peer| peer.screen_since)
			};
			let started = known.unwrap_or_else(|| {
				match self.seen_live.iter().find(|(seen, _)| *seen == user) {
					Some((_, seen)) => *seen,
					None => {
						self.seen_live.push((user, now));
						now
					}
				}
			});
			since.push((user, started));
		}
		ui.voice_live_since = since;
	}

	fn start(
		&mut self,
		runtime: &Runtime,
		channel: Id,
		me: Id,
		tag: [u8; 32],
		ctx: &egui::Context,
	) {
		let (streams, wake) = (self.streams.clone(), ctx.clone());
		let screen_sink: discord_voice::VideoSink = Arc::new(move |frame| {
			if streams
				.lock()
				.is_ok_and(|mut streams| crate::voice::store_remote_frame(&mut streams, frame))
			{
				wake.request_repaint();
			}
		});
		let (cameras, wake) = (self.cameras.clone(), ctx.clone());
		let camera_sink: discord_voice::VideoSink = Arc::new(move |frame| {
			let Some(pictures) = cameras.lock().ok().and_then(|cameras| cameras.clone()) else {
				return;
			};
			if pictures
				.lock()
				.is_ok_and(|mut pictures| crate::voice::store_remote_frame(&mut pictures, frame))
			{
				wake.request_repaint();
			}
		});
		let stream_audio = self.stream_audio.clone();
		// Bounded playout queue: a full one drops the frame rather than blocking the mesh.
		let audio_sink: discord_voice::p2p::AudioSink = Arc::new(move |samples| {
			if let Some(send) = stream_audio.lock().ok().and_then(|send| send.clone()) {
				let _ = send.try_send(samples.into());
			}
		});
		let wake = ctx.clone();
		let config = MeshConfig {
			channel: channel.0,
			user: me.0,
			tag,
			screen: self.screen.clone(),
			camera: self.camera.clone(),
			screen_audio: self.screen_audio.clone(),
			screen_sink,
			camera_sink,
			audio_sink,
			wake: Arc::new(move || wake.request_repaint()),
		};
		let Ok(mesh) = Mesh::start(runtime.handle(), config) else {
			return;
		};
		for (_, user, tag) in self.tags.iter().filter(|(known, ..)| *known == channel) {
			mesh.peer(user.0, *tag);
		}
		self.running = Some(Running {
			channel,
			tag,
			mesh,
			members: Vec::new(),
			watching: Vec::new(),
		});
	}
}
