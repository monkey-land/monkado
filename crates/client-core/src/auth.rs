use std::{
	fmt,
	sync::{Arc, OnceLock},
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthState {
	Unauthenticated,
	Authenticating,
	Challenged,
	Authenticated,
	Expired,
	Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
	#[error("Session expired; authenticated traffic stopped")]
	Expired,
	#[error("Verification challenge unsupported; complete it in an official flow")]
	Challenged,
	#[error(
		"O Discord restringiu esta conta; o tráfego autenticado foi interrompido; verifique o cliente oficial"
	)]
	AccountRestricted,
	#[error("Discord blocked this client request; connection stopped; check the official client")]
	ServiceBlocked,
	#[error("Permissão negada")]
	Forbidden,
	#[error("Rate limited; wait for the service cooldown before retrying")]
	RateLimited,
	#[error("Connection failed; no automatic write retry")]
	Network,
	#[error("Outcome unknown; inspect the conversation before a deliberate retry")]
	Ambiguous,
	#[error("Service response rejected or incompatible")]
	Protocol,
	// Only compile-time labels are allowed here, never remote text, IDs or decode errors.
	#[error("{0}")]
	ProtocolAt(&'static str),
	#[error("Safe session capacity exceeded; connection stopped")]
	Capacity,
	// Fixed local labels only; never include remote payloads or account identifiers.
	#[error("{0}")]
	CapacityAt(&'static str),
	#[error("Only an owner-supplied normal-user session is supported")]
	InvalidCredential,
}
impl Failure {
	pub fn protocol_at(self, stage: &'static str) -> Self {
		if self == Self::Protocol {
			Self::ProtocolAt(stage)
		} else {
			self
		}
	}
	pub fn label(self) -> &'static str {
		match self {
			Self::Expired => {
				"Sessão expirada · reconecte manualmente; os rascunhos continuam na memória"
			}
			Self::Challenged => "Desafio de segurança · não suportado aqui; conexão interrompida",
			Self::Forbidden => "Permissão negada",
			Self::AccountRestricted => {
				"O Discord exige atenção na conta · conexão interrompida; verifique o cliente oficial"
			}
			Self::ServiceBlocked => {
				"O Discord bloqueou esta solicitação · conexão interrompida; verifique o cliente oficial"
			}
			Self::RateLimited => "Limite de requisições · aguarde antes de tentar de novo",
			Self::Network => "Falha na conexão",
			Self::Ambiguous => {
				"Resultado desconhecido · verifique o cliente oficial antes de tentar de novo"
			}
			Self::Protocol => "Resposta do serviço não suportada",
			Self::ProtocolAt(stage) | Self::CapacityAt(stage) => stage,
			Self::Capacity => "Capacidade segura excedida · conexão interrompida",
			Self::InvalidCredential => "Dados de sessão inválidos ou conta de bot rejeitada",
		}
	}
	pub fn ends_session(self) -> bool {
		matches!(
			self,
			Self::Expired
				| Self::AccountRestricted
				| Self::ServiceBlocked
				| Self::Challenged
				| Self::Capacity
				| Self::CapacityAt(_)
				| Self::InvalidCredential
		)
	}
}
/// Which response ended a session as expired, for the copyable failure report. `source` is a
/// fixed label; `route` keeps only static path words, never IDs, codes, queries or remote text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expiry {
	pub source: &'static str,
	pub route: Option<Box<str>>,
}
impl fmt::Display for Expiry {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.route {
			Some(route) => write!(f, "{} on {route}", self.source),
			None => f.write_str(self.source),
		}
	}
}
/// Shared between the transports and the app; the first recorded cause wins.
pub type ExpirySlot = Arc<OnceLock<Expiry>>;
/// Intentionally neither Clone nor Serialize. Exposure is limited to transport adapters.
pub struct SessionSecret(Zeroizing<String>);
impl SessionSecret {
	pub fn from_owner_input(value: String) -> Result<Self, Failure> {
		let input = Zeroizing::new(value);
		// Browser storage shows the token JSON-quoted; pasting it verbatim would be rejected as expired.
		let trimmed = input.trim();
		let value = trimmed
			.strip_prefix('"')
			.and_then(|inner| inner.strip_suffix('"'))
			.unwrap_or(trimmed);
		if value.len() < 16
			|| value.len() > 2048
			|| value.starts_with("Bot ")
			|| value.starts_with("Bearer ")
			|| !value.bytes().all(|b| b.is_ascii_graphic())
		{
			return Err(Failure::InvalidCredential);
		}
		Ok(Self(Zeroizing::new(value.to_owned())))
	}
	pub fn expose(&self) -> &str {
		&self.0
	}
}
impl fmt::Debug for SessionSecret {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("SessionSecret([REDACTED])")
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn secret_is_redacted_and_header_injection_rejected() {
		let contextual = Failure::Protocol.protocol_at("Synthetic stage");
		assert_eq!(contextual.label(), "Synthetic stage");
		let capacity = Failure::CapacityAt("Synthetic capacity limit");
		assert_eq!(capacity.label(), "Synthetic capacity limit");
		assert!(capacity.ends_session());
		for failure in [
			Failure::Expired,
			Failure::Challenged,
			Failure::Forbidden,
			Failure::RateLimited,
			Failure::Network,
			Failure::Capacity,
			capacity,
			contextual,
		] {
			assert_eq!(failure.protocol_at("Other stage"), failure);
		}
		let secret = SessionSecret::from_owner_input("SYNTHETIC_SECRET_MARKER".into()).unwrap();
		assert!(!format!("{secret:?}").contains("SYNTHETIC"));
		for pasted in ["\"SYNTHETIC_SECRET_MARKER\"", " SYNTHETIC_SECRET_MARKER\n"] {
			let secret = SessionSecret::from_owner_input(pasted.into()).unwrap();
			assert_eq!(secret.expose(), "SYNTHETIC_SECRET_MARKER");
		}
		for value in [
			"Bot SYNTHETIC_SECRET",
			"Bearer SYNTHETIC_SECRET",
			"SYNTHETIC\r\nAuthorization: injected",
		] {
			assert!(SessionSecret::from_owner_input(value.into()).is_err());
		}
	}
}
