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
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;

/// `MONKADO_P2P=0` turns P2P off entirely: screen share and camera go to Discord as upstream.
pub(super) fn enabled() -> bool {
	std::env::var_os("MONKADO_P2P").is_none_or(|value| value != "0")
}

/// Voice session tags kept across calls; one call needs at most its 64 participants.
const MAX_TAGS: usize = 128;

type Cameras = Arc<Mutex<Option<Arc<Mutex<crate::voice::RemotePictures>>>>>;

struct Running {
	channel: Id,
	tag: [u8; 32],
	mesh: Mesh,
	members: Vec<u64>,
	watching: Option<u64>,
}

#[derive(Default)]
pub(super) struct P2p {
	screen: Track,
	camera: Track,
	running: Option<Running>,
	tags: Vec<(Id, Id, [u8; 32])>,
	peers: Vec<P2pMedia>,
	stream: Arc<Mutex<crate::watch::Slot>>,
	texture: crate::gpu_video::VideoTexture,
	/// Where peers' camera pictures go: the live call's remote pictures, which change on rejoin.
	cameras: Cameras,
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
			return;
		}
		voice.use_p2p(&self.screen, &self.camera);
		if let Ok(mut cameras) = self.cameras.lock() {
			*cameras = voice.remote_video();
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
		let watching = call.and_then(|call| call.watching).map(|user| user.0);
		if self.running.as_ref().map(|r| (r.channel, r.tag))
			!= wanted.map(|(channel, _, tag)| (channel, tag))
		{
			self.running = None;
			self.peers.clear();
			if let Some((channel, me, tag)) = wanted {
				self.start(runtime, channel, me, tag, ctx);
			}
		}
		let Some(running) = &mut self.running else {
			state.set_p2p_media(Vec::new());
			return;
		};
		if running.members != members {
			running.mesh.members(members.clone());
			running.members = members;
		}
		for update in running.mesh.updates() {
			self.peers.retain(|peer| peer.user.0 != update.user);
			if (update.screen || update.camera) && self.peers.len() < voice::MAX_PARTICIPANTS {
				self.peers.push(P2pMedia {
					user: Id(update.user),
					streaming: update.screen,
					video: update.camera,
				});
			}
		}
		// This device's own share and camera show as for Discord-announced media, too.
		let mut media = self.peers.clone();
		if let Some(me) = state.user.as_ref().map(|user| user.id)
			&& (self.screen.live() || self.camera.live())
		{
			media.push(P2pMedia {
				user: me,
				streaming: self.screen.live(),
				video: self.camera.live(),
			});
		}
		state.set_p2p_media(media);
		let watching = watching.filter(|user| {
			self.peers
				.iter()
				.any(|peer| peer.user.0 == *user && peer.streaming)
		});
		if running.watching != watching {
			running.mesh.watch(watching);
			running.watching = watching;
			if let Ok(mut slot) = self.stream.lock() {
				*slot = None;
			}
			self.texture = Default::default();
		}
		if watching.is_none() {
			return;
		}
		if let Ok(mut slot) = self.stream.try_lock()
			&& let Some((picture, fresh)) = slot.as_mut()
			&& std::mem::take(fresh)
			&& let Some(texture) = self.texture.upload(ctx, "p2p-stream", picture.borrow())
		{
			ui.voice_stream_view = Some(texture);
		}
		if let Some(texture) = self.texture.texture() {
			ui.voice_stream_view = Some(texture);
			ui.voice_stream_status = "Watching the stream";
		} else {
			ui.voice_stream_status = "Connecting to the stream…";
		}
	}

	fn start(
		&mut self,
		runtime: &Runtime,
		channel: Id,
		me: Id,
		tag: [u8; 32],
		ctx: &egui::Context,
	) {
		let (slot, wake) = (self.stream.clone(), ctx.clone());
		let screen_sink: discord_voice::VideoSink = Arc::new(move |frame| {
			if slot
				.lock()
				.is_ok_and(|mut slot| crate::watch::store_frame(&mut slot, frame))
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
		let wake = ctx.clone();
		let config = MeshConfig {
			channel: channel.0,
			user: me.0,
			tag,
			screen: self.screen.clone(),
			camera: self.camera.clone(),
			screen_sink,
			camera_sink,
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
			watching: None,
		});
	}
}
