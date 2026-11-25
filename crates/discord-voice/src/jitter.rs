//! Adaptive reorder window for 2.5–120ms Opus packets with in-band FEC recovery.
//!
//! Playout starts `target` ticks after the first packet. A packet that arrives after its
//! slot was already concealed proves the network jitter is larger than the buffer, so the
//! target grows by one tick (up to 160ms) and playout stretches by one concealed tick. After
//! ten seconds without late packets the target shrinks again, dropping only Opus silence so
//! no speech is cut. A lost packet is rebuilt from the next packet's in-band FEC data when
//! that packet is already buffered, and concealed otherwise.
const SLOTS: usize = 16;
const MIN_TARGET: u8 = 2;
const MAX_TARGET: u8 = 8;
/// Ten seconds of 20ms ticks without a late packet.
const SHRINK_AFTER: u16 = 500;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Packet {
	Audio(Vec<u8>),
	/// The next packet, decoded with FEC to rebuild the missing one before it.
	Fec(Vec<u8>),
	/// Opus packet-loss concealment.
	Conceal,
}

pub(crate) struct Jitter {
	packets: Vec<(u16, Vec<u8>)>,
	next: Option<u16>,
	wait: u8,
	missing: u8,
	/// Startup and steady playout depth in ticks; kept across talk spurts.
	target: u8,
	/// Concealed ticks still owed to a target that just grew.
	stretch: u8,
	/// Silence packets still to drop after the target shrank.
	shrink: u8,
	clean: u16,
}
impl Default for Jitter {
	fn default() -> Self {
		Self {
			packets: Vec::new(),
			next: None,
			wait: 0,
			missing: 0,
			target: MIN_TARGET,
			stretch: 0,
			shrink: 0,
			clean: 0,
		}
	}
}
impl Jitter {
	/// Ends the current stream but keeps the learned playout depth.
	pub fn clear(&mut self) {
		*self = Self {
			target: self.target,
			clean: self.clean,
			..Self::default()
		};
	}
	pub fn push(&mut self, sequence: u16, opus: Vec<u8>) {
		if opus.len() > 1275 {
			return;
		}
		if self.next.is_none() {
			self.next = Some(sequence);
			self.wait = self.target;
		}
		let next = self.next.unwrap();
		let distance = sequence.wrapping_sub(next);
		if distance >= 32768 {
			// Arrived after its slot played. Only a recent slot counts as jitter.
			if next.wrapping_sub(sequence) <= SLOTS as u16 {
				self.late();
			}
			return;
		}
		if distance >= SLOTS as u16 {
			self.packets.clear();
			self.next = Some(sequence);
			self.wait = self.target;
			self.missing = 0;
			self.stretch = 0;
		}
		if self.packets.len() < SLOTS && !self.packets.iter().any(|(id, _)| *id == sequence) {
			self.packets.push((sequence, opus));
		}
	}
	fn late(&mut self) {
		self.clean = 0;
		self.shrink = 0;
		if self.target < MAX_TARGET {
			self.target += 1;
			self.stretch = self.stretch.saturating_add(1).min(MAX_TARGET);
		}
	}
	fn take(&mut self, sequence: u16) -> Option<Vec<u8>> {
		let index = self.packets.iter().position(|(id, _)| *id == sequence)?;
		Some(self.packets.swap_remove(index).1)
	}
	/// `None` while waiting to start or after the stream ended; at most three consecutive
	/// packets are rebuilt or concealed.
	pub fn pop(&mut self) -> Option<Packet> {
		if self.wait > 0 {
			// Short packets can fill the window before the target elapses. Start before
			// the next arrival would repeatedly reset a full window.
			if self.packets.len() < SLOTS {
				self.wait -= 1;
				return None;
			}
			self.wait = 0;
		}
		let next = self.next?;
		if self.stretch > 0 && !self.packets.is_empty() {
			// Hold the sequence for one concealed tick so the deeper target takes effect.
			self.stretch -= 1;
			return Some(Packet::Conceal);
		}
		self.next = Some(next.wrapping_add(1));
		if let Some(opus) = self.take(next) {
			self.missing = 0;
			self.clean = self.clean.saturating_add(1);
			if self.clean >= SHRINK_AFTER && self.target > MIN_TARGET {
				self.target -= 1;
				self.shrink = self.shrink.saturating_add(1);
				self.clean = 0;
			}
			if self.shrink > 0
				&& opus == davey::OPUS_SILENCE_PACKET
				&& let Some(following) = self.take(next.wrapping_add(1))
			{
				// Drop one silent tick to give the shallower target its latency back.
				self.shrink -= 1;
				self.next = Some(next.wrapping_add(2));
				return Some(Packet::Audio(following));
			}
			return Some(Packet::Audio(opus));
		}
		self.missing += 1;
		let following = next.wrapping_add(1);
		if let Some((_, opus)) = self.packets.iter().find(|(id, _)| *id == following) {
			return Some(Packet::Fec(opus.clone()));
		}
		if self.missing > 3 {
			// Stop concealing the gap, but retain speech that already arrived after it.
			if let Some((index, (sequence, _))) = self
				.packets
				.iter()
				.enumerate()
				.min_by_key(|(_, (sequence, _))| sequence.wrapping_sub(next))
			{
				self.next = Some(sequence.wrapping_add(1));
				self.missing = 0;
				return Some(Packet::Audio(self.packets.swap_remove(index).1));
			}
			self.clear();
			return None;
		}
		Some(Packet::Conceal)
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	fn audio(byte: u8) -> Option<Packet> {
		Some(Packet::Audio(vec![byte]))
	}
	#[test]
	fn burst_loss_rebuilds_from_fec_preserves_recovery_and_resets_on_far_jump() {
		for first in [0u16, u16::MAX - 2] {
			let mut jitter = Jitter::default();
			jitter.push(first, vec![0]);
			for offset in [7, 5, 6] {
				jitter.push(first.wrapping_add(offset), vec![offset as u8]);
			}
			assert!(jitter.pop().is_none());
			assert!(jitter.pop().is_none());
			assert_eq!(jitter.pop(), audio(0));
			for _ in 0..3 {
				assert_eq!(jitter.pop(), Some(Packet::Conceal));
			}
			// Packet 4 is lost, but packet 5 already carries its FEC copy.
			assert_eq!(jitter.pop(), Some(Packet::Fec(vec![5])));
			for offset in [5, 6, 7] {
				assert_eq!(jitter.pop(), audio(offset));
			}
			for _ in 0..3 {
				assert_eq!(jitter.pop(), Some(Packet::Conceal));
			}
			assert!(jitter.pop().is_none());
			jitter.push(first.wrapping_add(100), vec![100]);
			assert_eq!(jitter.missing, 0);
			assert!(jitter.pop().is_none());
			assert!(jitter.pop().is_none());
			assert_eq!(jitter.pop(), audio(100));
		}
	}
	#[test]
	fn bounded_reordering_loss_duplicates_and_wrap() {
		let mut jitter = Jitter::default();
		jitter.push(u16::MAX, vec![1]);
		jitter.push(1, vec![3]);
		jitter.push(0, vec![2]);
		jitter.push(0, vec![9]);
		assert!(jitter.pop().is_none());
		assert!(jitter.pop().is_none());
		assert_eq!(jitter.pop(), audio(1));
		assert_eq!(jitter.pop(), audio(2));
		assert_eq!(jitter.pop(), audio(3));
		for _ in 0..3 {
			assert_eq!(jitter.pop(), Some(Packet::Conceal));
		}
		assert!(jitter.pop().is_none());
		for sequence in 0..1000 {
			jitter.push(sequence, vec![1; 1275]);
			assert!(jitter.packets.len() <= SLOTS);
		}
		jitter.push(1000, vec![0; 1276]);
		assert!(jitter.packets.len() <= SLOTS);
	}
	#[test]
	fn late_packets_deepen_playout_and_quiet_periods_drain_only_silence() {
		let mut jitter = Jitter::default();
		let mut sequence = 0u16;
		// Every fourth packet arrives four ticks (80ms) late; the rest on time.
		let mut delayed = std::collections::VecDeque::new();
		let mut run = |jitter: &mut Jitter, sequence: &mut u16, ticks: u16| {
			let mut played = 0;
			for tick in 0..ticks {
				while delayed.front().is_some_and(|(due, _)| *due == tick) {
					let (_, late) = delayed.pop_front().unwrap();
					jitter.push(late, vec![2]);
				}
				if *sequence % 4 == 3 {
					delayed.push_back((tick + 4, *sequence));
				} else {
					jitter.push(*sequence, vec![1]);
				}
				*sequence = sequence.wrapping_add(1);
				played += usize::from(matches!(jitter.pop(), Some(Packet::Audio(_))));
			}
			delayed.clear();
			played
		};
		run(&mut jitter, &mut sequence, 200);
		assert!(jitter.target > MIN_TARGET + 1, "{}", jitter.target);
		assert!(jitter.target <= MAX_TARGET);
		// Once deep enough, delayed packets land before their slot and play normally.
		let learned = jitter.target;
		jitter.clear();
		let played = run(&mut jitter, &mut sequence, 200);
		assert_eq!(jitter.target, learned);
		assert!(played >= 190, "{played}");

		// A clean stretch shrinks the target, which drops silence but never speech.
		let deep = jitter.target;
		let mut dropped_speech = false;
		for tick in 0..(u32::from(SHRINK_AFTER) * 2) {
			let silent = tick % 50 >= 40;
			let packet = if silent {
				davey::OPUS_SILENCE_PACKET.to_vec()
			} else {
				vec![7]
			};
			jitter.push(sequence, packet);
			sequence = sequence.wrapping_add(1);
			dropped_speech |= matches!(jitter.pop(), Some(Packet::Conceal) | None) && !silent;
		}
		assert!(jitter.target < deep);
		assert!(!dropped_speech);
		assert!(jitter.packets.len() <= usize::from(jitter.target) + 1);
	}
}
