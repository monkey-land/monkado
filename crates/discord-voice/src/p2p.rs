//! Monkado-to-Monkado screen share and camera over P2P, outside Discord's media servers.
//!
//! Unofficial. Discord refuses camera and Go Live video from unofficial clients, so Monkado
//! clients in the same call form a small mesh of iroh QUIC connections: direct after NAT hole
//! punching, otherwise through n0's public relay, end-to-end encrypted by QUIC TLS either way.
//!
//! Each client's endpoint key derives from the call channel, its user ID and a hash of its
//! voice session ID. Only clients that receive this call's voice states learn that hash, so
//! only they can find an endpoint; no message is posted anywhere. One side knowing the other
//! is enough: whoever was in the call first sees the newcomer's voice state and dials.
//!
//! Each connection carries a control stream (a hello with the dialer's user ID, then one
//! five-byte status per change: media flags and the share's age in seconds) and one stream per subscribed track: the subscriber writes the
//! track ID, then keyframe requests; the publisher writes length-prefixed H.264 access units.
use crate::{
	screen::Video,
	video_codec::Codec,
	video_receive::{self, DecoderQueue, Encoded, Lost, MAX_FRAME_BYTES, VideoSink},
};
use iroh::{
	Endpoint, EndpointAddr, EndpointId, SecretKey,
	endpoint::{Connection, Incoming, RecvStream, SendStream, presets},
};
use sha2::{Digest, Sha256};
use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant},
};
use tokio::{
	sync::{
		broadcast::{self, error::RecvError},
		mpsc,
	},
	task::{JoinHandle, JoinSet},
};

const ALPN: &[u8] = b"monkado/p2p/2";

/// Opt-in log line with fixed text and counts only; no IDs or addresses.
fn count(value: usize) -> diagnostics::Value {
	diagnostics::Value::Unsigned(value as u64)
}
fn note(label: &'static str, fields: &[(&'static str, diagnostics::Value)]) {
	diagnostics::record_fields(diagnostics::Category::Voice, diagnostics::Level::Info, label, fields);
}
const SCREEN: u8 = 1;
const CAMERA: u8 = 2;
/// Frames buffered per subscriber; a slower one skips to the next keyframe instead.
const BACKLOG: usize = 8;
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
const STATUS_INTERVAL: Duration = Duration::from_millis(250);
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
/// A newcomer publishes its address a few seconds after joining, so early dials often fail.
const REDIAL_INTERVAL: Duration = Duration::from_secs(5);
/// Matches Discord's call participant limit.
const MAX_PEERS: usize = 64;

/// Endpoint key of `user` in `channel`, from the SHA-256 of their voice session ID.
pub fn secret_key(channel: u64, user: u64, tag: &[u8; 32]) -> SecretKey {
	let digest = Sha256::new()
		.chain_update(b"monkado-p2p/1")
		.chain_update(channel.to_le_bytes())
		.chain_update(user.to_le_bytes())
		.chain_update(tag)
		.finalize();
	SecretKey::from_bytes(&digest.into())
}

struct Frame {
	data: Vec<u8>,
	keyframe: bool,
}
type KeyframeHook = Arc<dyn Fn() + Send + Sync>;
/// Shared by every subscription of one kind; `offer` never blocks, so the lock is brief.
type Decoder = Arc<Mutex<DecoderQueue>>;
struct TrackInner {
	frames: broadcast::Sender<Arc<Frame>>,
	live: AtomicBool,
	/// When the source went live, so peers can show how long a share has run.
	since: Mutex<Option<Instant>>,
	keyframe: Mutex<Option<KeyframeHook>>,
}
/// One local media source peers can subscribe to. Cheap to clone; frames published while
/// nobody subscribes are dropped.
#[derive(Clone)]
pub struct Track(Arc<TrackInner>);
impl Default for Track {
	fn default() -> Self {
		Self(Arc::new(TrackInner {
			frames: broadcast::channel(BACKLOG).0,
			live: AtomicBool::new(false),
			since: Mutex::new(None),
			keyframe: Mutex::new(None),
		}))
	}
}
impl Track {
	pub fn publish(&self, data: Vec<u8>, keyframe: bool) {
		if self.has_viewers() && data.len() <= MAX_FRAME_BYTES {
			let _ = self.0.frames.send(Arc::new(Frame { data, keyframe }));
		}
	}
	/// Announces the source to peers; they see it as a live share or a camera tile.
	pub fn set_live(&self, live: bool) {
		let was = self.0.live.swap(live, Ordering::AcqRel);
		if was != live
			&& let Ok(mut since) = self.0.since.lock()
		{
			*since = live.then(Instant::now);
		}
	}
	/// When the source went live; `None` while off.
	pub fn live_since(&self) -> Option<Instant> {
		self.0.since.lock().ok().and_then(|since| *since)
	}
	pub fn live(&self) -> bool {
		self.0.live.load(Ordering::Acquire)
	}
	/// How a subscriber's keyframe request reaches the encoder.
	pub fn set_keyframe_hook(&self, hook: Option<KeyframeHook>) {
		if let Ok(mut slot) = self.0.keyframe.lock() {
			*slot = hook;
		}
	}
	fn request_keyframe(&self) {
		if let Some(hook) = self.0.keyframe.lock().ok().and_then(|hook| hook.clone()) {
			hook();
		}
	}
	pub fn has_viewers(&self) -> bool {
		self.0.frames.receiver_count() > 0
	}
}

/// Publishes the screen encoder's frames on `track` until capture stops or the future is
/// dropped. The encoder only works while someone watches.
pub async fn publish_screen(track: Track, mut video: Video) {
	struct Offline(Track);
	impl Drop for Offline {
		fn drop(&mut self) {
			self.0.set_live(false);
			self.0.set_keyframe_hook(None);
		}
	}
	let keyframe = video.keyframe.clone();
	track.set_keyframe_hook(Some(Arc::new(move || {
		keyframe.store(true, Ordering::Release)
	})));
	track.set_live(true);
	let _offline = Offline(track.clone());
	let mut tick = tokio::time::interval(STATUS_INTERVAL);
	loop {
		video.ready.store(track.has_viewers(), Ordering::Release);
		tokio::select! {
			frame = video.frames.recv() => {
				let Some(frame) = frame else { break };
				track.publish(frame.data, frame.keyframe);
			}
			_ = tick.tick() => {}
		}
	}
}

/// A peer's announced media changed; `linked` false (and no media) when its connection closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update {
	pub user: u64,
	/// A Monkado connection to this peer is open.
	pub linked: bool,
	pub screen: bool,
	pub camera: bool,
	/// When the peer's screen share started, in this device's clock.
	pub screen_since: Option<Instant>,
}
impl Update {
	fn gone(user: u64) -> Self {
		Self {
			user,
			linked: false,
			screen: false,
			camera: false,
			screen_since: None,
		}
	}
}

enum Control {
	Peer(u64, [u8; 32]),
	Members(Vec<u64>),
	Watch(Vec<u64>),
}

/// This client's P2P presence in one call. Dropping it closes every connection.
pub struct Mesh {
	control: mpsc::UnboundedSender<Control>,
	updates: std::sync::mpsc::Receiver<Update>,
	task: JoinHandle<()>,
}
pub struct MeshConfig {
	pub channel: u64,
	pub user: u64,
	pub tag: [u8; 32],
	pub screen: Track,
	pub camera: Track,
	/// Decoded screen pictures of the watched peers, tagged with each user ID.
	pub screen_sink: VideoSink,
	/// Decoded camera pictures, tagged with each peer's user ID.
	pub camera_sink: VideoSink,
	pub wake: Arc<dyn Fn() + Send + Sync>,
}
impl Mesh {
	pub fn start(
		runtime: &tokio::runtime::Handle,
		config: MeshConfig,
	) -> Result<Self, &'static str> {
		let (control, receive) = mpsc::unbounded_channel();
		let (send, updates) = std::sync::mpsc::channel();
		let (screen_decoder, screen_lost) =
			video_receive::spawn_decoder(config.screen_sink.clone())?;
		let (camera_decoder, camera_lost) =
			video_receive::spawn_decoder(config.camera_sink.clone())?;
		let shared = Arc::new(Shared {
			channel: config.channel,
			user: config.user,
			screen: config.screen,
			camera: config.camera,
			decoders: [
				(Arc::new(Mutex::new(screen_decoder)), screen_lost),
				(Arc::new(Mutex::new(camera_decoder)), camera_lost),
			],
			updates: send,
			wake: config.wake,
		});
		let key = secret_key(config.channel, config.user, &config.tag);
		let task = runtime.spawn(run(shared, key, receive));
		Ok(Self {
			control,
			updates,
			task,
		})
	}
	/// A participant's voice session tag, from this call's voice states.
	pub fn peer(&self, user: u64, tag: [u8; 32]) {
		let _ = self.control.send(Control::Peer(user, tag));
	}
	/// Current call participants; connections to anyone else close.
	pub fn members(&self, users: Vec<u64>) {
		let _ = self.control.send(Control::Members(users));
	}
	/// The peers whose screens this device shows.
	pub fn watch(&self, users: Vec<u64>) {
		let _ = self.control.send(Control::Watch(users));
	}
	pub fn updates(&self) -> impl Iterator<Item = Update> + '_ {
		self.updates.try_iter()
	}
}
impl Drop for Mesh {
	fn drop(&mut self) {
		self.task.abort();
	}
}

struct Shared {
	channel: u64,
	user: u64,
	screen: Track,
	camera: Track,
	/// Screen and camera decoder threads with their broken-decoder reports.
	decoders: [(Decoder, Lost); 2],
	updates: std::sync::mpsc::Sender<Update>,
	wake: Arc<dyn Fn() + Send + Sync>,
}
impl Shared {
	fn emit(&self, update: Update) {
		if self.updates.send(update).is_ok() {
			(self.wake)();
		}
	}
	fn status(&self) -> u8 {
		u8::from(self.screen.live()) | u8::from(self.camera.live()) << 1
	}
	/// Wire status: media flags, then the screen share's age in whole seconds (big-endian).
	fn status_message(&self) -> [u8; 5] {
		let age = self.screen.live_since().map_or(0, |since| {
			u32::try_from(since.elapsed().as_secs()).unwrap_or(u32::MAX)
		});
		let mut message = [self.status(), 0, 0, 0, 0];
		message[1..].copy_from_slice(&age.to_be_bytes());
		message
	}
	fn track(&self, id: u8) -> Option<&Track> {
		match id {
			SCREEN => Some(&self.screen),
			CAMERA => Some(&self.camera),
			_ => None,
		}
	}
}

enum Internal {
	Connected {
		user: u64,
		connection: Connection,
		control: (SendStream, RecvStream),
		/// The endpoint that dialed; duplicate connections keep the lower one on both sides.
		dialer: EndpointId,
	},
	DialFailed(u64, &'static str),
	Status {
		user: u64,
		link: u64,
		status: u8,
		/// Local clock time the peer's screen share started.
		screen_since: Option<Instant>,
	},
	Closed {
		user: u64,
		link: u64,
	},
}

struct Link {
	id: u64,
	connection: Connection,
	dialer: EndpointId,
	status: u8,
	task: JoinHandle<()>,
	subscriptions: [Option<JoinHandle<()>>; 2],
}
impl Drop for Link {
	fn drop(&mut self) {
		self.task.abort();
		for subscription in self.subscriptions.iter().flatten() {
			subscription.abort();
		}
		self.connection.close(0u32.into(), b"");
	}
}

async fn run(shared: Arc<Shared>, key: SecretKey, mut control: mpsc::UnboundedReceiver<Control>) {
	let Ok(endpoint) = Endpoint::builder(presets::N0)
		.secret_key(key)
		.alpns(vec![ALPN.to_vec()])
		.bind()
		.await
	else {
		note("p2p_bind_failed", &[]);
		return;
	};
	note("p2p_online", &[]);
	let (internal, mut events) = mpsc::unbounded_channel();
	let mut tags: HashMap<u64, [u8; 32]> = HashMap::new();
	let mut members: Vec<u64> = Vec::new();
	let mut links: HashMap<u64, Link> = HashMap::new();
	let mut dialing: HashMap<u64, Instant> = HashMap::new();
	let mut watching: Vec<u64> = Vec::new();
	let mut sequence: u64 = 0;
	let mut redial = tokio::time::interval(REDIAL_INTERVAL);
	loop {
		tokio::select! {
			command = control.recv() => match command {
				None => break,
				Some(Control::Peer(user, tag)) => {
					if user != shared.user && (tags.len() < MAX_PEERS || tags.contains_key(&user)) {
						tags.insert(user, tag);
						note("p2p_peer_session", &[("known", count(tags.len()))]);
						dial(&shared, &endpoint, &internal, &tags, &members, &links, &mut dialing, user);
					}
				}
				Some(Control::Members(users)) => {
					members = users;
					members.truncate(MAX_PEERS);
					note(
						"p2p_members",
						&[("members", count(members.len())), ("sessions", count(tags.len()))],
					);
					links.retain(|user, _| members.contains(user));
					tags.retain(|user, _| members.contains(user));
					for user in members.clone() {
						dial(&shared, &endpoint, &internal, &tags, &members, &links, &mut dialing, user);
					}
				}
				Some(Control::Watch(users)) => {
					watching = users;
					watching.truncate(MAX_PEERS);
					for (user, link) in &mut links {
						subscribe(&shared, *user, link, &watching);
					}
				}
			},
			incoming = endpoint.accept() => {
				let Some(incoming) = incoming else { break };
				if links.len() >= MAX_PEERS {
					incoming.refuse();
					continue;
				}
				tokio::spawn(accept(incoming, internal.clone()));
			}
			Some(event) = events.recv() => match event {
				Internal::Connected { user, connection, control, dialer } => {
					dialing.remove(&user);
					let remote = connection.remote_id();
					// A dialer is a call member, and its key matches its session when known.
					let genuine = user != shared.user
						&& members.contains(&user)
						&& tags.get(&user).is_none_or(|tag| {
							secret_key(shared.channel, user, tag).public() == remote
						});
					if !genuine {
						let reason = if user == shared.user {
							"self"
						} else if !members.contains(&user) {
							"not_member"
						} else {
							"key_mismatch"
						};
						note("p2p_link_rejected", &[("reason", diagnostics::Value::Text(reason))]);
						connection.close(0u32.into(), b"");
						continue;
					}
					if links.get(&user).is_some_and(|link| link.dialer < dialer) {
						note("p2p_link_duplicate", &[]);
						connection.close(0u32.into(), b"");
						continue;
					}
					let outgoing = u8::from(dialer == endpoint.id());
					note("p2p_link", &[("outgoing", diagnostics::Value::from(outgoing))]);
					let id = sequence;
					sequence += 1;
					let task = tokio::spawn(serve(
						shared.clone(),
						connection.clone(),
						control,
						user,
						id,
						internal.clone(),
					));
					// Replacing a link drops the old one, which closes its connection.
					if let Some(old) = links.insert(user, Link {
						id,
						connection,
						dialer,
						status: 0,
						task,
						subscriptions: [None, None],
					}) && old.status != 0 {
						shared.emit(Update::gone(user));
					}
				}
				Internal::DialFailed(user, stage) => {
					note("p2p_dial_failed", &[("stage", diagnostics::Value::Text(stage))]);
					dialing.remove(&user);
				}
				Internal::Status { user, link, status, screen_since } => {
					if let Some(current) = links.get_mut(&user).filter(|current| current.id == link) {
						note(
							"p2p_status",
							&[
								("screen", diagnostics::Value::from(status & 1)),
								("camera", diagnostics::Value::from(status >> 1 & 1)),
							],
						);
						current.status = status;
						subscribe(&shared, user, current, &watching);
						shared.emit(Update {
							user,
							linked: true,
							screen: status & 1 != 0,
							camera: status & 2 != 0,
							screen_since: screen_since.filter(|_| status & 1 != 0),
						});
					}
				}
				Internal::Closed { user, link } => {
					if links.get(&user).is_some_and(|current| current.id == link) {
						note("p2p_link_closed", &[]);
						links.remove(&user);
						shared.emit(Update::gone(user));
					}
				}
			},
			_ = redial.tick() => {
				for user in tags.keys().copied().collect::<Vec<_>>() {
					dial(&shared, &endpoint, &internal, &tags, &members, &links, &mut dialing, user);
				}
			}
		}
	}
	drop(links);
	endpoint.close().await;
}

#[allow(clippy::too_many_arguments)]
fn dial(
	shared: &Arc<Shared>,
	endpoint: &Endpoint,
	internal: &mpsc::UnboundedSender<Internal>,
	tags: &HashMap<u64, [u8; 32]>,
	members: &[u64],
	links: &HashMap<u64, Link>,
	dialing: &mut HashMap<u64, Instant>,
	user: u64,
) {
	let Some(tag) = tags.get(&user) else { return };
	if !members.contains(&user)
		|| links.contains_key(&user)
		|| dialing
			.get(&user)
			.is_some_and(|started| started.elapsed() < DIAL_TIMEOUT)
	{
		return;
	}
	dialing.insert(user, Instant::now());
	note("p2p_dial", &[]);
	let address = EndpointAddr::from(secret_key(shared.channel, user, tag).public());
	let (endpoint, internal, me) = (endpoint.clone(), internal.clone(), shared.user);
	tokio::spawn(async move {
		let dialer = endpoint.id();
		let connected = async {
			let connection = endpoint
				.connect(address, ALPN)
				.await
				.map_err(|_| "connect")?;
			let (mut send, recv) = connection.open_bi().await.map_err(|_| "stream")?;
			send.write_all(&me.to_le_bytes())
				.await
				.map_err(|_| "hello")?;
			Ok(Internal::Connected {
				user,
				connection,
				control: (send, recv),
				dialer,
			})
		};
		let event = match tokio::time::timeout(DIAL_TIMEOUT, connected).await {
			Ok(Ok(event)) => event,
			Ok(Err(stage)) => Internal::DialFailed(user, stage),
			Err(_) => Internal::DialFailed(user, "timeout"),
		};
		let _ = internal.send(event);
	});
}

async fn accept(incoming: Incoming, internal: mpsc::UnboundedSender<Internal>) {
	let accepted = async {
		let connection = incoming.accept().ok()?.await.ok()?;
		let (send, mut recv) = connection.accept_bi().await.ok()?;
		let mut user = [0u8; 8];
		recv.read_exact(&mut user).await.ok()?;
		let dialer = connection.remote_id();
		Some(Internal::Connected {
			user: u64::from_le_bytes(user),
			connection,
			control: (send, recv),
			dialer,
		})
	};
	if let Ok(Some(event)) = tokio::time::timeout(DIAL_TIMEOUT, accepted).await {
		let _ = internal.send(event);
	}
}

/// Starts or stops this link's subscriptions to match the peer's media and the watch choice.
fn subscribe(shared: &Arc<Shared>, user: u64, link: &mut Link, watching: &[u64]) {
	let wanted = [
		link.status & 1 != 0 && watching.contains(&user),
		link.status & 2 != 0,
	];
	for (index, (wanted, track)) in wanted.into_iter().zip([SCREEN, CAMERA]).enumerate() {
		let running = link.subscriptions[index]
			.as_ref()
			.is_some_and(|task| !task.is_finished());
		if wanted && !running {
			let (decoder, lost) = shared.decoders[index].clone();
			link.subscriptions[index] = Some(tokio::spawn(receive(
				link.connection.clone(),
				track,
				user,
				decoder,
				lost,
			)));
		} else if !wanted && let Some(task) = link.subscriptions[index].take() {
			task.abort();
			if let Ok(decoder) = shared.decoders[index].0.lock() {
				video_receive::remove(&decoder, user);
			}
		}
	}
}

/// One link: exchanges status messages on the control stream and serves the peer's track
/// subscriptions until either side closes.
async fn serve(
	shared: Arc<Shared>,
	connection: Connection,
	(mut send, mut recv): (SendStream, RecvStream),
	user: u64,
	link: u64,
	internal: mpsc::UnboundedSender<Internal>,
) {
	let mut subscribers = JoinSet::new();
	let mut sent = None;
	let mut tick = tokio::time::interval(STATUS_INTERVAL);
	let mut status = [0u8; 5];
	loop {
		tokio::select! {
			_ = tick.tick() => {
				let now = shared.status();
				if sent != Some(now) {
					if send.write_all(&shared.status_message()).await.is_err() {
						break;
					}
					sent = Some(now);
				}
			}
			read = recv.read_exact(&mut status) => {
				let Ok(()) = read else { break };
				let Some((flags, screen_since)) = parse_status(status, Instant::now()) else {
					break;
				};
				let _ = internal.send(Internal::Status { user, link, status: flags, screen_since });
			}
			stream = connection.accept_bi() => {
				let Ok((send, recv)) = stream else { break };
				if subscribers.len() < 4 {
					subscribers.spawn(publish(shared.clone(), send, recv));
				}
			}
			Some(_) = subscribers.join_next(), if !subscribers.is_empty() => {}
		}
	}
	let _ = internal.send(Internal::Closed { user, link });
}

/// Sends one local track to a subscriber, starting at a keyframe.
async fn publish(
	shared: Arc<Shared>,
	mut send: SendStream,
	mut recv: RecvStream,
) -> Result<(), ()> {
	let mut request = [0u8; 1];
	recv.read_exact(&mut request).await.map_err(drop)?;
	let track = shared.track(request[0]).ok_or(())?.clone();
	let mut frames = track.0.frames.subscribe();
	track.request_keyframe();
	let mut asked = Instant::now();
	let mut waiting = true;
	loop {
		tokio::select! {
			read = recv.read_exact(&mut request) => {
				read.map_err(drop)?;
				// A subscriber cannot force more than one keyframe per interval.
				if asked.elapsed() >= KEYFRAME_INTERVAL {
					asked = Instant::now();
					track.request_keyframe();
				}
			}
			frame = frames.recv() => match frame {
				Ok(frame) => {
					if waiting && !frame.keyframe {
						continue;
					}
					waiting = false;
					write_frame(&mut send, &frame).await?;
				}
				Err(RecvError::Lagged(_)) => {
					waiting = true;
					track.request_keyframe();
				}
				Err(RecvError::Closed) => return Ok(()),
			},
		}
	}
}

async fn write_frame(send: &mut SendStream, frame: &Frame) -> Result<(), ()> {
	let length = u32::try_from(frame.data.len()).map_err(drop)?;
	let mut header = [0u8; 5];
	header[0] = u8::from(frame.keyframe);
	header[1..].copy_from_slice(&length.to_be_bytes());
	send.write_all(&header).await.map_err(drop)?;
	send.write_all(&frame.data).await.map_err(drop)
}

/// Subscribes to one of a peer's tracks and feeds it to the matching decoder as `user`.
async fn receive(connection: Connection, track: u8, user: u64, decoder: Decoder, lost: Lost) {
	let _ = receive_frames(connection, track, user, &decoder, &lost).await;
	if let Ok(decoder) = decoder.lock() {
		video_receive::remove(&decoder, user);
	}
}
async fn receive_frames(
	connection: Connection,
	track: u8,
	user: u64,
	decoder: &Mutex<DecoderQueue>,
	lost: &Lost,
) -> Result<(), &'static str> {
	const ENDED: &str = "The P2P stream ended";
	let (mut send, mut recv) = connection.open_bi().await.map_err(|_| ENDED)?;
	send.write_all(&[track]).await.map_err(|_| ENDED)?;
	let mut asked = Instant::now();
	let mut waiting = true;
	let mut header = [0u8; 5];
	loop {
		recv.read_exact(&mut header).await.map_err(|_| ENDED)?;
		let (keyframe, length) = parse_header(header)?;
		let mut data = vec![0; length];
		recv.read_exact(&mut data).await.map_err(|_| ENDED)?;
		if let Ok(mut lost) = lost.lock()
			&& lost.contains(&user)
		{
			lost.retain(|lost| *lost != user);
			waiting = true;
		}
		if !waiting || keyframe {
			let frame = Encoded {
				codec: Codec::H264,
				user,
				data,
				keyframe,
			};
			let decoder = decoder
				.lock()
				.map_err(|_| "Video decoder state unavailable")?;
			waiting = !video_receive::offer(&decoder, frame)?;
		}
		if waiting && asked.elapsed() >= KEYFRAME_INTERVAL {
			asked = Instant::now();
			send.write_all(&[1]).await.map_err(|_| ENDED)?;
		}
	}
}

/// Untrusted status message: flags must be known; an impossible age is dropped.
fn parse_status(message: [u8; 5], now: Instant) -> Option<(u8, Option<Instant>)> {
	if message[0] > 3 {
		return None;
	}
	let age = u32::from_be_bytes([message[1], message[2], message[3], message[4]]);
	let since = (message[0] & 1 != 0)
		.then(|| now.checked_sub(Duration::from_secs(u64::from(age))))
		.flatten();
	Some((message[0], since))
}

fn parse_header(header: [u8; 5]) -> Result<(bool, usize), &'static str> {
	let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
	if length == 0 || length > MAX_FRAME_BYTES || header[0] > 1 {
		return Err("The peer sent an invalid frame");
	}
	Ok((header[0] == 1, length))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn endpoint_keys_need_the_session_and_differ_per_call() {
		let key = secret_key(20, 7, &[1; 32]).public();
		assert_eq!(key, secret_key(20, 7, &[1; 32]).public());
		assert_ne!(key, secret_key(20, 7, &[2; 32]).public());
		assert_ne!(key, secret_key(21, 7, &[1; 32]).public());
		assert_ne!(key, secret_key(20, 8, &[1; 32]).public());
	}

	/// Two meshes over the real network (n0 address lookup and relays): the newcomer is dialed
	/// by the peer that learned its session, sees its camera announced and subscribes to it.
	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "needs internet access"]
	async fn mesh_announces_and_subscribes_across_two_clients() {
		let mesh = |user, camera: &Track| {
			Mesh::start(
				&tokio::runtime::Handle::current(),
				MeshConfig {
					channel: 9,
					user,
					tag: [user as u8; 32],
					screen: Track::default(),
					camera: camera.clone(),
					screen_sink: Arc::new(|_| {}),
					camera_sink: Arc::new(|_| {}),
					wake: Arc::new(|| {}),
				},
			)
			.unwrap()
		};
		let (first_camera, second_camera) = (Track::default(), Track::default());
		let first = mesh(1, &first_camera);
		let second = mesh(2, &second_camera);
		second_camera.set_live(true);
		for mesh in [&first, &second] {
			mesh.members(vec![1, 2]);
		}
		// Only the earlier participant learned the other's session.
		first.peer(2, [2; 32]);
		let deadline = Instant::now() + Duration::from_secs(60);
		let mut announced = None;
		while Instant::now() < deadline && !(announced.is_some() && second_camera.has_viewers()) {
			announced = announced.or(first.updates().find(|update| update.camera));
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
		assert_eq!(
			announced,
			Some(Update {
				user: 2,
				linked: true,
				screen: false,
				camera: true,
				screen_since: None,
			})
		);
		assert!(
			second_camera.has_viewers(),
			"the camera subscription reached the publisher"
		);
	}

	#[test]
	fn status_carries_share_age_and_rejects_unknown_flags() {
		let now = Instant::now() + Duration::from_secs(3600);
		let (flags, since) = parse_status([1, 0, 0, 0, 90], now).unwrap();
		assert_eq!(flags, 1);
		assert_eq!(since, Some(now - Duration::from_secs(90)));
		assert_eq!(parse_status([2, 0, 0, 0, 90], now), Some((2, None)));
		assert!(parse_status([4, 0, 0, 0, 0], now).is_none());
	}

	#[test]
	fn frame_header_bounds_untrusted_lengths() {
		assert_eq!(parse_header([1, 0, 0, 0, 9]), Ok((true, 9)));
		assert!(parse_header([0, 0, 0, 0, 0]).is_err());
		assert!(parse_header([2, 0, 0, 0, 9]).is_err());
		assert!(parse_header([0, 0xff, 0xff, 0xff, 0xff]).is_err());
	}
}
