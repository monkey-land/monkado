//! Monkado call invites: a direct message carrying a server invite plus the voice channel's
//! link. Discord shows both links; Monkado rings the friend and joins that channel on answer.
use model::Id;

/// Discord voice and stage channel kinds.
pub fn is_voice(kind: u8) -> bool {
	matches!(kind, 2 | 13)
}

/// The invite message: the server invite, then the call's channel link when there is one.
pub fn message(code: &str, call: Option<(Id, Id)>) -> String {
	match call {
		Some((guild, channel)) => format!(
			"https://discord.gg/{code}\n📞 Entra na call: https://discord.com/channels/{}/{}",
			guild.0, channel.0
		),
		None => format!("https://discord.gg/{code}"),
	}
}

/// The first `discord.com/channels/{guild}/{channel}` link in untrusted message text.
pub fn parse(content: &str) -> Option<(Id, Id)> {
	const MARK: &str = "discord.com/channels/";
	let start = content.find(MARK)? + MARK.len();
	let mut parts = content[start..].split('/');
	let guild = parts.next()?;
	let channel: String = parts
		.next()?
		.chars()
		.take_while(char::is_ascii_digit)
		.collect();
	let id = |text: &str| {
		(!text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()))
			.then(|| text.parse::<u64>().ok())
			.flatten()
			.filter(|id| *id != 0)
			.map(Id)
	};
	Some((id(guild)?, id(&channel)?))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn call_invites_round_trip_and_reject_foreign_or_malformed_links() {
		let text = message("abc", Some((Id(10), Id(20))));
		assert_eq!(parse(&text), Some((Id(10), Id(20))));
		assert_eq!(parse(&message("abc", None)), None);
		assert_eq!(parse("https://discord.com/channels/@me/20"), None);
		assert_eq!(
			parse("https://discord.com/channels/10/20/30"),
			Some((Id(10), Id(20)))
		);
		assert_eq!(parse("https://discord.com/channels/10/"), None);
		assert_eq!(parse("https://discord.com/channels/0/20"), None);
		assert_eq!(
			parse("https://discord.com/channels/123456789012345678901/2"),
			None
		);
	}
}
