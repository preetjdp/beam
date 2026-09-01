//! HTTP/3 WebTransport video datagram relay.
//!
//! Control and recovery remain on WSS. This task subscribes to the existing
//! bounded server media broadcast and sends video over QUIC datagrams; audio
//! stays on the reliable compatibility socket for the first transport slice.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use beam_protocol::{
    AgentCommand, DatagramHeader, DependencyClass, InputEvent, VideoFrameHeader, fragment_frame,
};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;
use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, ServerConfig};

use crate::auth;
use crate::signaling;
use crate::web::AppState;

const WEBTRANSPORT_PATH_PREFIX: &str = "/webtransport/";
const MAX_DATAGRAM_PAYLOAD: usize = 1200;

pub fn build_server(
    bind_port: u16,
    mut tls_config: rustls::ServerConfig,
) -> Result<Endpoint<wtransport::endpoint::endpoint_side::Server>> {
    tls_config.alpn_protocols = vec![wtransport::tls::WEBTRANSPORT_ALPN.to_vec()];
    let config = ServerConfig::builder()
        .with_bind_default(bind_port)
        .with_custom_tls(tls_config)
        .max_idle_timeout(Some(Duration::from_secs(30)))
        .context("invalid WebTransport idle timeout")?
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .allow_migration(true)
        .build();
    Endpoint::server(config).context("failed to bind HTTP/3/WebTransport UDP listener")
}

pub async fn run(
    endpoint: Endpoint<wtransport::endpoint::endpoint_side::Server>,
    state: Arc<AppState>,
) {
    info!(
        port = state.config.server.port,
        "HTTP/3 WebTransport listener ready"
    );
    loop {
        let incoming = endpoint.accept().await;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(error) = handle_incoming(incoming, state).await {
                debug!(%error, "WebTransport session ended");
            }
        });
    }
}

async fn handle_incoming(incoming: IncomingSession, state: Arc<AppState>) -> Result<()> {
    let request = incoming
        .await
        .context("QUIC/WebTransport handshake failed")?;
    let peer = request.remote_address();
    let (session_id, token) = match parse_session_path(request.path()) {
        Some(value) => value,
        None => {
            request.not_found().await;
            anyhow::bail!("invalid WebTransport path");
        }
    };

    let claims = match auth::validate_jwt(&token, &state.jwt_secret) {
        Ok(claims) => claims,
        Err(_) => {
            request.forbidden().await;
            anyhow::bail!("invalid WebTransport token");
        }
    };
    let Some(session) = state.session_manager.get_session(session_id).await else {
        request.not_found().await;
        anyhow::bail!("unknown WebTransport session");
    };
    if session.username != claims.sub {
        request.forbidden().await;
        anyhow::bail!("WebTransport session ownership mismatch");
    }
    if let Some(origin) = request.origin()
        && let Some(hostname) = state.config.server.hostname.as_deref()
        && origin != format!("https://{hostname}")
    {
        request.forbidden().await;
        anyhow::bail!("WebTransport origin mismatch");
    }

    let connection = request
        .accept_with_headers([("x-beam-media", "video-datagrams-v1")])
        .await
        .context("failed to accept WebTransport session")?;
    let channel = signaling::get_or_create_channel(&state.channels, session_id).await;
    let mut media = channel.video_frames.subscribe();
    let _ = channel
        .to_agent
        .send(AgentCommand::Input(InputEvent::RequestKeyframe {
            generation: 0,
            reason: "webtransport_started".to_string(),
        }));

    info!(%session_id, %peer, "WebTransport media session connected");
    let connection_id = u32::from_le_bytes(session_id.as_bytes()[..4].try_into().unwrap());
    let mut sequence = 0u64;
    let generation = 1u32;
    let mut last_recovery_request = std::time::Instant::now() - Duration::from_secs(1);
    let max_datagram = connection
        .max_datagram_size()
        .unwrap_or(MAX_DATAGRAM_PAYLOAD)
        .min(MAX_DATAGRAM_PAYLOAD);

    loop {
        tokio::select! {
            error = connection.closed() => {
                debug!(%session_id, %error, "WebTransport connection closed");
                break;
            }
            received = media.recv() => {
                let bytes = match received {
                    Ok(bytes) => bytes,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(%session_id, skipped, "WebTransport relay lagged; requesting recovery");
                        if last_recovery_request.elapsed() >= Duration::from_millis(250) {
                            last_recovery_request = std::time::Instant::now();
                            let _ = channel.to_agent.send(AgentCommand::Input(InputEvent::RequestKeyframe {
                                generation,
                                reason: "webtransport_server_lag".to_string(),
                            }));
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Ok(frame) = VideoFrameHeader::deserialize(&bytes) else { continue; };
                if frame.is_audio() { continue; }
                sequence = sequence.wrapping_add(1);
                let dependency = if frame.is_keyframe() {
                    DependencyClass::Key
                } else {
                    DependencyClass::Reference
                };
                let header = DatagramHeader {
                    connection_id,
                    stream_generation: generation,
                    frame_sequence: sequence,
                    fragment_index: 0,
                    fragment_count: 1,
                    total_frame_length: bytes.len() as u32,
                    dependency,
                    temporal_id: None,
                    keyframe: frame.is_keyframe(),
                    fec_group: 0,
                    fec_index: 0,
                    fec_data_count: 0,
                    parity: false,
                    payload_length: 0,
                };
                let packets = match fragment_frame(header, &bytes, max_datagram) {
                    Ok(packets) => packets,
                    Err(error) => {
                        warn!(%session_id, %error, "WebTransport frame packetization failed");
                        continue;
                    }
                };
                let mut failed = false;
                for packet in packets {
                    if let Err(error) = connection.send_datagram(packet) {
                        debug!(%session_id, %error, "WebTransport datagram send pressure");
                        failed = true;
                        break;
                    }
                }
                if failed
                    && dependency != DependencyClass::Disposable
                    && last_recovery_request.elapsed() >= Duration::from_millis(250)
                {
                    last_recovery_request = std::time::Instant::now();
                    let _ = channel.to_agent.send(AgentCommand::Input(InputEvent::RequestKeyframe {
                        generation,
                        reason: "webtransport_send_drop".to_string(),
                    }));
                }
            }
        }
    }
    Ok(())
}

fn parse_session_path(path: &str) -> Option<(Uuid, String)> {
    let (path, query) = path.split_once('?')?;
    let id = path.strip_prefix(WEBTRANSPORT_PATH_PREFIX)?.parse().ok()?;
    let token = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| urlencoding::decode(value).ok().map(|v| v.into_owned()))?
    })?;
    Some((id, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_and_encoded_token() {
        let id = Uuid::nil();
        let parsed = parse_session_path(&format!("/webtransport/{id}?token=a%2Fb%2Bc")).unwrap();
        assert_eq!(parsed, (id, "a/b+c".to_string()));
    }

    #[test]
    fn rejects_wrong_path_or_missing_token() {
        assert!(
            parse_session_path("/other/00000000-0000-0000-0000-000000000000?token=x").is_none()
        );
        assert!(parse_session_path("/webtransport/00000000-0000-0000-0000-000000000000").is_none());
    }
}
