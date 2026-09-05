pub mod bashparse;
pub mod cloud;
pub mod config;
pub mod daemon;
pub mod hook;
pub mod memory;
pub mod mls;
pub mod patch;
pub mod proto;
pub mod relay;
pub mod rooms;
pub mod teams;
pub mod term;
pub mod watch;

/// Install the TLS backend before the first `wss://` handshake.
///
/// rustls 0.23 refuses to guess a crypto provider when the crate is compiled
/// with more or fewer than one provider feature, and the refusal is a *panic*
/// inside whichever task happens to dial first. In a fail-open system that is
/// the worst possible failure: the daemon's relay task died, every edit was
/// still allowed, and `knoot status` reported the relay as fine because a
/// stored token is all it could see. Coordination was off and nothing said so.
///
/// Idempotent, and safe to call from several tasks at once — losing the race
/// means someone else installed the same provider.
pub fn install_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
