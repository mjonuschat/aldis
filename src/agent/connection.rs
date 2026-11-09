//! The websocket connection to Moonraker, with reconnect.

use std::io;
use std::sync::mpsc;
use std::time::Duration;

use std::net::TcpStream;

use tungstenite::client::IntoClientRequest;
use tungstenite::error::UrlError;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error, Message, WebSocket};

use crate::agent::protocol::{
    IDENTIFY_ID, Incoming, dispatch, error_message, event_message, identify_message,
    parse_incoming, result_message,
};
use crate::agent::service::{AgentBackend, AgentService, EventSink};
use crate::tls::Tls;

const POLL: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub fn websocket_url(moonraker_url: &str) -> String {
    let base = moonraker_url.trim_end_matches('/');
    let base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| base.to_owned());
    format!("{base}/websocket")
}

struct ClearOnDrop<'a>(&'a EventSink);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

/// Opens the websocket to Moonraker, verifying the certificate of a `wss://` endpoint by `tls`.
/// Once connected, reads time out after [`POLL`] so the caller can interleave writes.
fn connect(url: &str, tls: &Tls) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, Error> {
    let request = websocket_url(url).into_client_request()?;
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or(Error::Url(UrlError::NoHostName))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let stream = TcpStream::connect((host.as_str(), port))?;
    let (socket, _) =
        tungstenite::client_tls_with_config(request, stream, None, Some(tls.websocket_connector()))
            .map_err(|error| match error {
                tungstenite::HandshakeError::Failure(error) => error,
                tungstenite::HandshakeError::Interrupted(_) => Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the websocket handshake did not complete",
                )),
            })?;
    let stream = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => &stream.sock,
        _ => {
            return Err(Error::Io(io::Error::other(
                "the websocket uses an unsupported transport",
            )));
        }
    };
    stream.set_read_timeout(Some(POLL))?;
    Ok(socket)
}

/// Runs one connection until Moonraker closes it. A single thread both reads (with a short
/// timeout) and writes, because a tungstenite socket cannot be split across threads.
pub fn serve<B: AgentBackend>(
    url: &str,
    tls: &Tls,
    service: &AgentService<B>,
    events: &EventSink,
) -> Result<(), Error> {
    let mut socket = connect(url, tls)?;
    let (sender, receiver) = mpsc::channel();
    *events.lock().unwrap() = Some(sender);
    let _clear = ClearOnDrop(events);
    socket.send(Message::text(identify_message(IDENTIFY_ID).to_string()))?;
    tracing::info!(url, "connected to Moonraker");
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => match parse_incoming(text.as_str()) {
                Incoming::Request { id, method, params } => {
                    tracing::info!(method, "request");
                    let reply = match dispatch(service, &method, params) {
                        Ok(result) => result_message(&id, result),
                        Err(error) => error_message(&id, &error),
                    };
                    socket.send(Message::text(reply.to_string()))?;
                }
                Incoming::IdentifyError { message } => {
                    tracing::warn!(message, "Moonraker rejected the identify request");
                    return Err(Error::Io(io::Error::other(format!(
                        "Moonraker rejected identify: {message}"
                    ))));
                }
                Incoming::Ignored => {}
            },
            Ok(Message::Close(_)) | Err(Error::ConnectionClosed | Error::AlreadyClosed) => {
                return Ok(());
            }
            Ok(_) => {}
            Err(Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
        while let Ok(event) = receiver.try_recv() {
            socket.send(Message::text(event_message(&event).to_string()))?;
        }
    }
}

/// Connects forever, backing off up to 30 s between attempts. Runs in progress are unaffected.
pub fn run_forever<B: AgentBackend>(
    url: &str,
    tls: &Tls,
    service: AgentService<B>,
    events: EventSink,
) -> ! {
    let mut backoff = Duration::from_secs(1);
    loop {
        match serve(url, tls, &service, &events) {
            Ok(()) => {
                tracing::info!("Moonraker closed the connection");
                backoff = Duration::from_secs(1);
            }
            Err(error) => tracing::warn!(%error, "Moonraker connection failed"),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
