use crate::auth::reconnect_token::ReconnectTokenPayload;
use crate::auth::{AuthResult, AuthService};
use crate::{ReconnectToken, CONFIG};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tracing::{error, info};
use tunnelto_lib::{ClientHello, ClientId, ClientType, ServerHello};
use uuid::Uuid;
use warp::filters::ws::{Message, WebSocket};

pub struct ClientHandshake {
    pub id: ClientId,
    pub sub_domain: String,
    pub is_anonymous: bool,
    pub auth_token: Option<String>,
}

#[tracing::instrument(skip(websocket))]
pub async fn auth_client_handshake(
    mut websocket: WebSocket,
) -> Option<(WebSocket, ClientHandshake)> {
    let client_hello_data = match websocket.next().await {
        Some(Ok(msg)) => msg,
        _ => {
            error!("no client init message");
            return None;
        }
    };

    auth_client(client_hello_data.as_bytes(), websocket).await
}

#[tracing::instrument(skip(client_hello_data, websocket))]
async fn auth_client(
    client_hello_data: &[u8],
    mut websocket: WebSocket,
) -> Option<(WebSocket, ClientHandshake)> {
    // parse the client hello
    let client_hello: ClientHello = match serde_json::from_slice(client_hello_data) {
        Ok(ch) => ch,
        Err(error) => {
            error!(?error, "invalid client hello");
            let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
    };

    let auth_token = client_hello.auth_token.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let (auth_key, client_id, is_anonymous) = match client_hello.client_type {
        ClientType::Anonymous => {
            if !CONFIG.allow_anonymous {
                let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
                let _ = websocket.send(Message::binary(data)).await;
                return None;
            }

            if let Some(token) = client_hello.reconnect_token {
                return handle_reconnect_token(token, auth_token, websocket).await;
            }

            (None, ClientId::generate(), true)
        }
        ClientType::Auth { key } => {
            if let Some(token) = client_hello.reconnect_token {
                return handle_reconnect_token(token, auth_token, websocket).await;
            }

            let client_id = key.client_id();
            (Some(key), client_id, false)
        }
    };

    let tunnel_id = match request_tunnel_id().await {
        Ok(id) => id,
        Err(error) => {
            error!(?error, "failed to request tunnel id");
            let data = serde_json::to_vec(&ServerHello::Error(error)).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
    };

    if is_anonymous {
        return Some((
            websocket,
            ClientHandshake {
                id: client_id,
                sub_domain: tunnel_id,
                is_anonymous: true,
                auth_token,
            },
        ));
    }

    let auth_key = match auth_key {
        Some(key) => key,
        None => {
            let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
    };

    info!(tunnel_id=%tunnel_id, "will auth tunnel id");

    // next authenticate the sub-domain
    let sub_domain = match crate::AUTH_DB_SERVICE
        .auth_sub_domain(&auth_key.0, &tunnel_id)
        .await
    {
        Ok(AuthResult::Available) | Ok(AuthResult::ReservedByYou) => tunnel_id,
        Ok(AuthResult::ReservedByYouButDelinquent) | Ok(AuthResult::PaymentRequired) => {
            // note: delinquent payments get a random suffix
            // ServerHello::prefixed_random_domain(&tunnel_id)
            // TODO: create free trial domain
            tracing::info!(tunnel_id=%tunnel_id, "payment required");
            let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
        Ok(AuthResult::ReservedByOther) => {
            let data = serde_json::to_vec(&ServerHello::SubDomainInUse).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
        Err(error) => {
            error!(?error, "error auth-ing user");
            let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
    };

    tracing::info!(subdomain=%sub_domain, "did auth tunnel id");

    Some((
        websocket,
        ClientHandshake {
            id: client_id,
            sub_domain,
            is_anonymous: false,
            auth_token,
        },
    ))
}

#[tracing::instrument(skip(token, websocket))]
async fn handle_reconnect_token(
    token: ReconnectToken,
    auth_token: Option<String>,
    mut websocket: WebSocket,
) -> Option<(WebSocket, ClientHandshake)> {
    let payload = match ReconnectTokenPayload::verify(token, &CONFIG.master_sig_key) {
        Ok(payload) => payload,
        Err(error) => {
            error!(?error, "invalid reconnect token");
            let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None;
        }
    };

    tracing::debug!(
        client_id=%&payload.client_id,
        "accepting reconnect token from client",
    );

    Some((
        websocket,
        ClientHandshake {
            id: payload.client_id,
            sub_domain: payload.sub_domain,
            is_anonymous: true,
            auth_token,
        },
    ))
}

#[derive(Deserialize)]
struct GuidResponse {
    guid: String,
}

async fn request_tunnel_id() -> Result<String, String> {
    let client = reqwest::Client::new();
    let response = client
        .get(&CONFIG.tunnel_id_url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("guid request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "guid request failed: status {}",
            response.status()
        ));
    }

    let guid = response
        .json::<GuidResponse>()
        .await
        .map_err(|e| format!("guid response invalid: {e}"))?
        .guid;

    let guid = guid.trim();
    let parsed = Uuid::parse_str(guid).map_err(|_| "guid response invalid: not a uuid".to_string())?;
    let guid = parsed.to_string();

    if !is_valid_tunnel_id(&guid) {
        return Err("guid response invalid: illegal characters".to_string());
    }

    Ok(guid)
}

fn is_valid_tunnel_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }

    if id.contains('/') || id.contains('?') {
        return false;
    }

    if CONFIG
        .blocked_sub_domains
        .iter()
        .any(|blocked| blocked == id)
    {
        return false;
    }

    if id == "wormhole" {
        return false;
    }

    true
}
