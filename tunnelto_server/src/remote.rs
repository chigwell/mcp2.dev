use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tracing::debug;
use tracing::{error, Instrument};
use uuid::Uuid;

async fn direct_to_control(mut incoming: TcpStream) {
    let mut control_socket =
        match TcpStream::connect(format!("localhost:{}", CONFIG.control_port)).await {
            Ok(s) => s,
            Err(error) => {
                tracing::warn!(?error, "failed to connect to local control server");
                return;
            }
        };

    let (mut control_r, mut control_w) = control_socket.split();
    let (mut incoming_r, mut incoming_w) = incoming.split();

    let join_1 = tokio::io::copy(&mut control_r, &mut incoming_w);
    let join_2 = tokio::io::copy(&mut incoming_r, &mut control_w);

    match futures::future::join(join_1, join_2).await {
        (Ok(_), Ok(_)) => {}
        (Err(error), _) | (_, Err(error)) => {
            tracing::error!(?error, "directing stream to control failed");
        }
    }
}

#[tracing::instrument(skip(socket))]
pub async fn accept_connection(socket: TcpStream) {
    // peek the host of the http request
    // if health check, then handle it and return
    let StreamWithPeekedHost {
        mut socket,
        host,
        path,
        forwarded_for,
    } = match peek_http_request_host(socket).await {
        Some(s) => s,
        None => return,
    };

    tracing::info!(%host, %path, %forwarded_for, "new remote connection");

    let host = match normalize_host(&host) {
        Some(host) => host,
        None => {
            error!("invalid host specified");
            let _ = socket.write_all(HTTP_INVALID_HOST_RESPONSE).await;
            return;
        }
    };

    if let Some(base_host) = host.strip_prefix("wormhole.") {
        if is_allowed_host(base_host) {
            direct_to_control(socket).await;
            return;
        }
    }

    if !is_allowed_host(&host) {
        error!("invalid host specified");
        let _ = socket.write_all(HTTP_INVALID_HOST_RESPONSE).await;
        return;
    }

    let tunnel_id = match extract_tunnel_id(&path) {
        Some(id) => id,
        None => {
            error!(%path, "missing tunnel id in path");
            let _ = socket.write_all(HTTP_NOT_FOUND_RESPONSE).await;
            return;
        }
    };

    // find the client listening for this host
    let client = match Connections::find_by_host(&tunnel_id) {
        Some(client) => client.clone(),
        None => {
            // check other instances that may be serving this host
            match network::instance_for_host(&tunnel_id).await {
                Ok((instance, _)) => {
                    network::proxy_stream(instance, socket).await;
                    return;
                }
                Err(network::Error::DoesNotServeHost) => {
                    error!(%tunnel_id, "no tunnel found");
                    let _ = socket.write_all(HTTP_NOT_FOUND_RESPONSE).await;
                    return;
                }
                Err(error) => {
                    error!(%tunnel_id, ?error, "failed to find instance");
                    let _ = socket.write_all(HTTP_ERROR_LOCATING_HOST_RESPONSE).await;
                    return;
                }
            }
        }
    };

    // allocate a new stream for this request
    let (active_stream, queue_rx) = ActiveStream::new(client.clone());
    let stream_id = active_stream.id.clone();

    tracing::debug!(
        stream_id = %active_stream.id.to_string(),
        "new stream connected"
    );
    let (stream, sink) = tokio::io::split(socket);

    // add our stream
    ACTIVE_STREAMS.insert(stream_id.clone(), active_stream.clone());

    // read from socket, write to client
    let span = observability::remote_trace("process_tcp_stream");
    let process_tunnel_id = tunnel_id.clone();
    tokio::spawn(
        async move {
            process_tcp_stream(active_stream, stream, process_tunnel_id).await;
        }
        .instrument(span),
    );

    // read from client, write to socket
    let span = observability::remote_trace("tunnel_to_stream");
    tokio::spawn(
        async move {
            tunnel_to_stream(tunnel_id, stream_id, sink, queue_rx).await;
        }
        .instrument(span),
    );
}

fn normalize_host(host: &str) -> Option<String> {
    let url = format!("http://{}", host);

    let host = match url::Url::parse(&url)
        .map(|u| u.host().map(|h| h.to_owned()))
        .unwrap_or(None)
    {
        Some(domain) => domain.to_string(),
        None => {
            error!("invalid host header");
            return None;
        }
    };

    Some(host.to_string())
}

fn is_allowed_host(host: &str) -> bool {
    CONFIG.allowed_hosts.iter().any(|allowed| allowed == host)
}

fn normalize_path(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        if let Ok(url) = url::Url::parse(path) {
            let mut normalized = url.path().to_string();
            if let Some(query) = url.query() {
                normalized.push('?');
                normalized.push_str(query);
            }
            return normalized;
        }
    }
    path.to_string()
}

fn extract_tunnel_id(path: &str) -> Option<String> {
    let path = normalize_path(path);
    let trimmed = path.strip_prefix('/')?;
    if trimmed.is_empty() {
        return None;
    }

    let mut split_idx = None;
    for (idx, ch) in trimmed.char_indices() {
        if ch == '/' || ch == '?' {
            split_idx = Some(idx);
            break;
        }
    }

    let (tunnel_id, _) = match split_idx {
        Some(idx) => (&trimmed[..idx], &trimmed[idx..]),
        None => (trimmed, ""),
    };

    if tunnel_id.is_empty() {
        return None;
    }

    let parsed = Uuid::parse_str(tunnel_id).ok()?;
    Some(parsed.to_string())
}

/// Response Constants
const HTTP_INVALID_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 400\r\nContent-Length: 23\r\n\r\nError: Invalid Hostname";
const HTTP_NOT_FOUND_RESPONSE: &'static [u8] =
    b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found";
const HTTP_ERROR_LOCATING_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel";
const HTTP_TUNNEL_REFUSED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 32\r\n\r\nTunnel says: connection refused.";
const HTTP_UNAUTHORIZED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 401\r\nContent-Length: 19\r\n\r\nError: Unauthorized";
const HTTP_OK_RESPONSE: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
const HEALTH_CHECK_PATH: &'static [u8] = b"/0xDEADBEEF_HEALTH_CHECK";

struct StreamWithPeekedHost {
    socket: TcpStream,
    host: String,
    path: String,
    forwarded_for: String,
}
/// Filter incoming remote streams
#[tracing::instrument(skip(socket))]
async fn peek_http_request_host(mut socket: TcpStream) -> Option<StreamWithPeekedHost> {
    /// Note we return out if the host header is not found
    /// within the first 4kb of the request.
    const MAX_HEADER_PEAK: usize = 4096;
    let mut buf = vec![0; MAX_HEADER_PEAK]; //1kb

    tracing::debug!("checking stream headers");

    let n = match socket.peek(&mut buf).await {
        Ok(n) => n,
        Err(e) => {
            error!("failed to read from tcp socket to determine host: {:?}", e);
            return None;
        }
    };

    // make sure we're not peeking the same header bytes
    if n == 0 {
        tracing::debug!("unable to peek header bytes");
        return None;
    }

    tracing::debug!("peeked {} stream bytes ", n);

    let mut headers = [httparse::EMPTY_HEADER; 64]; // 30 seems like a generous # of headers
    let mut req = httparse::Request::new(&mut headers);

    if let Err(e) = req.parse(&buf[..n]) {
        error!("failed to parse incoming http bytes: {:?}", e);
        return None;
    }

    let path = normalize_path(req.path.unwrap_or_default());

    // Handle the health check route
    if path.as_bytes() == HEALTH_CHECK_PATH {
        let _ = socket.write_all(HTTP_OK_RESPONSE).await.map_err(|e| {
            error!("failed to write health_check: {:?}", e);
        });

        return None;
    }

    // get the ip addr in the header
    let forwarded_for = if let Some(Ok(forwarded_for)) = req
        .headers
        .iter()
        .filter(|h| h.name.to_lowercase() == "x-forwarded-for".to_string())
        .map(|h| std::str::from_utf8(h.value))
        .next()
    {
        forwarded_for.to_string()
    } else {
        String::default()
    };

    // look for a host header
    if let Some(Ok(host)) = req
        .headers
        .iter()
        .filter(|h| h.name.to_lowercase() == "host".to_string())
        .map(|h| std::str::from_utf8(h.value))
        .next()
    {
        tracing::info!(host=%host, path=%path, "peek request");

        return Some(StreamWithPeekedHost {
            socket,
            host: host.to_string(),
            path,
            forwarded_for,
        });
    }

    tracing::info!("found no host header, dropping connection.");
    None
}

/// Process Messages from the control path in & out of the remote stream
#[tracing::instrument(skip(tunnel_stream, tcp_stream))]
async fn process_tcp_stream(
    mut tunnel_stream: ActiveStream,
    mut tcp_stream: ReadHalf<TcpStream>,
    tunnel_id: String,
) {
    // send initial control stream init to client
    control_server::send_client_stream_init(tunnel_stream.clone()).await;

    // now read from stream and forward to clients
    let mut buf = [0; 1024];
    let mut pending = Vec::new();
    let mut body_state = BodyState::None;

    loop {
        // client is no longer connected
        if Connections::get(&tunnel_stream.client.id).is_none() {
            debug!("client disconnected, closing stream");
            let _ = tunnel_stream.tx.send(StreamMessage::NoClientTunnel).await;
            tunnel_stream.tx.close_channel();
            return;
        }

        // read from stream
        let n = match tcp_stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                error!("failed to read from tcp socket: {:?}", e);
                return;
            }
        };

        if n == 0 {
            debug!("stream ended");
            let _ = tunnel_stream
                .client
                .tx
                .send(ControlPacket::End(tunnel_stream.id.clone()))
                .await
                .map_err(|e| {
                    error!("failed to send end signal: {:?}", e);
                });
            return;
        }

        debug!("read {} bytes", n);
        pending.extend_from_slice(&buf[..n]);

        loop {
            match body_state {
                BodyState::Unknown => {
                    if pending.is_empty() {
                        break;
                    }
                    let packet =
                        ControlPacket::Data(tunnel_stream.id.clone(), pending.split_off(0));
                    match tunnel_stream.client.tx.send(packet).await {
                        Ok(_) => {
                            debug!(client_id = %tunnel_stream.client.id, "sent data packet to client")
                        }
                        Err(_) => {
                            error!(
                                "failed to forward tcp packets to disconnected client. dropping client."
                            );
                            Connections::remove(&tunnel_stream.client);
                            return;
                        }
                    }
                    continue;
                }
                BodyState::Fixed(remaining) => {
                    if pending.is_empty() {
                        break;
                    }
                    let take = std::cmp::min(remaining, pending.len());
                    let chunk = pending.drain(..take).collect::<Vec<u8>>();
                    let packet = ControlPacket::Data(tunnel_stream.id.clone(), chunk);
                    match tunnel_stream.client.tx.send(packet).await {
                        Ok(_) => {
                            debug!(client_id = %tunnel_stream.client.id, "sent data packet to client")
                        }
                        Err(_) => {
                            error!(
                                "failed to forward tcp packets to disconnected client. dropping client."
                            );
                            Connections::remove(&tunnel_stream.client);
                            return;
                        }
                    }

                    let remaining = remaining - take;
                    body_state = if remaining == 0 {
                        BodyState::None
                    } else {
                        BodyState::Fixed(remaining)
                    };
                    if matches!(body_state, BodyState::Fixed(_)) {
                        break;
                    }
                }
                BodyState::None => {
                    let header_end = match find_header_end(&pending) {
                        Some(end) => end,
                        None => break,
                    };

                    let header_bytes = &pending[..header_end];
                    let mut headers = [httparse::EMPTY_HEADER; 64];
                    let mut req = httparse::Request::new(&mut headers);
                    let parsed = match req.parse(header_bytes) {
                        Ok(status) => status,
                        Err(error) => {
                            error!(?error, "failed to parse request headers");
                            let packet =
                                ControlPacket::Data(tunnel_stream.id.clone(), pending.split_off(0));
                            match tunnel_stream.client.tx.send(packet).await {
                                Ok(_) => {
                                    debug!(
                                        client_id = %tunnel_stream.client.id,
                                        "sent data packet to client"
                                    )
                                }
                                Err(_) => {
                                    error!(
                                        "failed to forward tcp packets to disconnected client. dropping client."
                                    );
                                    Connections::remove(&tunnel_stream.client);
                                    return;
                                }
                            }
                            break;
                        }
                    };

                    if !matches!(parsed, httparse::Status::Complete(_)) {
                        break;
                    }

                    let path = req.path.unwrap_or_default();
                    let normalized_path = normalize_path(path);
                    let new_path = match strip_tunnel_prefix(&normalized_path, &tunnel_id) {
                        Some(path) => path,
                        None => {
                            tracing::info!(
                                %tunnel_id,
                                %normalized_path,
                                "missing tunnel prefix"
                            );
                            let _ = tunnel_stream
                                .tx
                                .send(StreamMessage::NoClientTunnel)
                                .await;
                            let _ = tunnel_stream
                                .client
                                .tx
                                .send(ControlPacket::End(tunnel_stream.id.clone()))
                                .await;
                            return;
                        }
                    };

                    if let Some(expected) = tunnel_stream.client.auth_token.as_deref() {
                        if !auth_header_matches(req.headers, expected) {
                            tracing::info!(
                                %tunnel_id,
                                header = TUNNEL_AUTH_HEADER,
                                "missing or invalid auth header"
                            );
                            let _ = tunnel_stream
                                .tx
                                .send(StreamMessage::Unauthorized)
                                .await;
                            let _ = tunnel_stream
                                .client
                                .tx
                                .send(ControlPacket::End(tunnel_stream.id.clone()))
                                .await;
                            return;
                        }
                    }

                    let rewritten = rewrite_request_path(header_bytes, &new_path);
                    let is_chunked = has_chunked_body(req.headers);
                    let content_len = content_length(req.headers).unwrap_or(0);

                    let packet = ControlPacket::Data(tunnel_stream.id.clone(), rewritten);
                    match tunnel_stream.client.tx.send(packet).await {
                        Ok(_) => {
                            debug!(client_id = %tunnel_stream.client.id, "sent data packet to client")
                        }
                        Err(_) => {
                            error!(
                                "failed to forward tcp packets to disconnected client. dropping client."
                            );
                            Connections::remove(&tunnel_stream.client);
                            return;
                        }
                    }

                    pending.drain(..header_end);

                    if is_chunked {
                        body_state = BodyState::Unknown;
                    } else {
                        body_state = if content_len == 0 {
                            BodyState::None
                        } else {
                            BodyState::Fixed(content_len)
                        };
                    }
                }
            }
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
}

fn rewrite_request_path(header_bytes: &[u8], new_path: &str) -> Vec<u8> {
    let line_end = match header_bytes
        .windows(2)
        .position(|window| window == b"\r\n")
    {
        Some(end) => end,
        None => return header_bytes.to_vec(),
    };

    let line = &header_bytes[..line_end];
    let first_space = match line.iter().position(|&b| b == b' ') {
        Some(idx) => idx,
        None => return header_bytes.to_vec(),
    };
    let second_space = match line[first_space + 1..].iter().position(|&b| b == b' ') {
        Some(idx) => idx + first_space + 1,
        None => return header_bytes.to_vec(),
    };

    let mut rewritten = Vec::with_capacity(header_bytes.len() + new_path.len());
    rewritten.extend_from_slice(&line[..first_space + 1]);
    rewritten.extend_from_slice(new_path.as_bytes());
    rewritten.extend_from_slice(&line[second_space..]);
    rewritten.extend_from_slice(b"\r\n");
    rewritten.extend_from_slice(&header_bytes[line_end + 2..]);
    rewritten
}

fn strip_tunnel_prefix(path: &str, tunnel_id: &str) -> Option<String> {
    let path = normalize_path(path);
    let prefix = format!("/{}", tunnel_id);

    if path == prefix {
        return Some("/".to_string());
    }

    if path.starts_with(&(prefix.clone() + "/")) {
        return Some(path[prefix.len()..].to_string());
    }

    if path.starts_with(&(prefix.clone() + "?")) {
        return Some(format!("/{}", &path[prefix.len()..]));
    }

    None
}

fn auth_header_matches(headers: &[httparse::Header], expected: &str) -> bool {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(TUNNEL_AUTH_HEADER))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .map(|value| value.trim() == expected)
        .unwrap_or(false)
}

fn content_length(headers: &[httparse::Header]) -> Option<usize> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
}

fn has_chunked_body(headers: &[httparse::Header]) -> bool {
    headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("transfer-encoding")
            && std::str::from_utf8(h.value)
                .map(|v| v.to_ascii_lowercase().contains("chunked"))
                .unwrap_or(false)
    })
}

#[derive(Debug, Clone, Copy)]
enum BodyState {
    None,
    Fixed(usize),
    Unknown,
}

#[tracing::instrument(skip(sink, stream_id, queue))]
async fn tunnel_to_stream(
    tunnel_id: String,
    stream_id: StreamId,
    mut sink: WriteHalf<TcpStream>,
    mut queue: UnboundedReceiver<StreamMessage>,
) {
    loop {
        let result = queue.next().await;

        let result = if let Some(message) = result {
            match message {
                StreamMessage::Data(data) => Some(data),
                StreamMessage::TunnelRefused => {
                    tracing::debug!(?stream_id, "tunnel refused");
                    let _ = sink.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
                    None
                }
                StreamMessage::NoClientTunnel => {
                    tracing::info!(%tunnel_id, ?stream_id, "client tunnel not found");
                    let _ = sink.write_all(HTTP_NOT_FOUND_RESPONSE).await;
                    None
                }
                StreamMessage::Unauthorized => {
                    tracing::info!(%tunnel_id, ?stream_id, "missing auth header");
                    let _ = sink.write_all(HTTP_UNAUTHORIZED_RESPONSE).await;
                    None
                }
            }
        } else {
            None
        };

        let data = match result {
            Some(data) => data,
            None => {
                tracing::debug!("done tunneling to sink");
                let _ = sink.shutdown().await.map_err(|_e| {
                    error!("error shutting down tcp stream");
                });

                ACTIVE_STREAMS.remove(&stream_id);
                return;
            }
        };

        let result = sink.write_all(&data).await;

        if let Some(error) = result.err() {
            tracing::warn!(?error, "stream closed, disconnecting");
            return;
        }
    }
}
