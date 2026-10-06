// The values a handler is given for a live connection or an upload:
// `WebSocket`, `Sse`, and `Multipart`, each bound by its type (varyk-http
// spec 2.5).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, close_code};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

/// A WebSocket connection, on a `get` route. Messages are text; ping and
/// pong are answered by the package (spec 2.5).
pub struct WebSocket {
    /// The connection, none once it is closing or gone. One lock for
    /// reading and writing: a handler borrows its parameter, so it cannot
    /// send while `recv` waits; a sink and a stream, each behind its own
    /// lock, are the fix once the language can share a socket.
    socket: tokio::sync::Mutex<Option<axum::extract::ws::WebSocket>>,
    /// Set when `recv` found the client closed or gone, so a later `send`
    /// is the client's doing, not the handler's own `close`.
    client_closed: AtomicBool,
    /// The request's record of an `Err` for a client that has gone.
    gone: crate::message::ClientGone,
}

/// The message of a WebSocket upgrade that did not complete.
const UPGRADE_FAILED: &str = "the WebSocket upgrade did not complete: the client has gone";

/// How long a closing connection reads on for the client's close in
/// reply, so the closing handshake completes.
const CLOSING: Duration = Duration::from_secs(5);

/// A stream of server-sent events, on a `get` route.
pub struct Sse {
    events: tokio::sync::mpsc::Sender<String>,
    /// The request's record of an `Err` for a client that has gone.
    gone: crate::message::ClientGone,
}

/// How many events wait for a slow client before `send` waits too.
const EVENTS_WAITING: usize = 16;

/// A `multipart/form-data` body, on a `post`, `put`, or `patch` route,
/// read one part at a time.
pub struct Multipart {
    form: Arc<tokio::sync::Mutex<Form>>,
}

/// One part of a `Multipart`: its name, the file name the client gave,
/// and a handle to the form, through which its content is read once.
pub struct Part {
    name: String,
    file_name: Option<String>,
    /// Which part of the form this is, counted from 1; the form has moved
    /// on once its own count is past it.
    index: u64,
    form: Arc<tokio::sync::Mutex<Form>>,
}

/// What a `Multipart` and its parts share: multer allows one live field
/// at a time, so the form keeps it, and `next` drops it before asking for
/// the next (spec 2.5).
struct Form {
    multipart: multer::Multipart<'static>,
    /// The live field of the latest part, until its content is read.
    field: Option<multer::Field<'static>>,
    /// How many times `next` was called.
    index: u64,
}

impl crate::message::Bound for WebSocket {
    /// Sends the 101 early, before the handler runs, and waits for the
    /// upgrade (spec 6.3); a request that is not a WebSocket upgrade is a
    /// 400.
    async fn bound(
        request: &crate::message::Request,
    ) -> Result<WebSocket, crate::message::Response> {
        let Some(upgrade) = request.take_upgrade() else {
            return Err(crate::message::error_body(
                400,
                "the request is not a WebSocket upgrade",
            ));
        };
        // A frame or a message over the body limit is refused as it
        // arrives, and `recv` closes the connection with 1009.
        let limit = usize::try_from(upgrade.limit).unwrap_or(usize::MAX);
        let (sender, upgraded) = tokio::sync::oneshot::channel();
        let switching = upgrade
            .socket
            .max_message_size(limit)
            .max_frame_size(limit)
            .on_failed_upgrade(|err| {
                varyk_std::tracing::debug!("a WebSocket upgrade failed: {err}");
            })
            .on_upgrade(move |socket| async move {
                let _ = sender.send(socket);
            });
        let switching = crate::message::Response::read_back(
            switching.status().as_u16(),
            switching.headers(),
            Vec::new(),
        );
        if !request.send_early(switching) {
            return Err(crate::message::internal_because(
                "the WebSocket cannot be opened: this request's response was sent already, so a handler takes one `Sse` or `WebSocket` at most"
                    .to_string(),
            ));
        }
        match upgraded.await {
            Ok(socket) => Ok(WebSocket {
                socket: tokio::sync::Mutex::new(Some(socket)),
                client_closed: AtomicBool::new(false),
                gone: request.client_gone(),
            }),
            Err(_) => {
                // The client went away, or an `after` hook changed the 101:
                // the response is sent, and nothing is left to answer.
                let error = crate::message::client_gone(&request.client_gone(), UPGRADE_FAILED);
                Err(crate::message::internal_because(
                    error.message().to_string(),
                ))
            }
        }
    }
}

impl WebSocket {
    /// The next text message; `None` once the client has closed or gone
    /// away, and after a binary message (closed with 1003), a malformed
    /// one (1002, or 1007 for text that is not UTF-8), or one over the
    /// body limit (1009).
    pub async fn recv(&self) -> Result<Option<String>, varyk_std::Error> {
        let mut slot = self.socket.lock().await;
        loop {
            let Some(socket) = slot.as_mut() else {
                return Ok(None);
            };
            // A ping read here is answered by the next read or write.
            let received = socket.recv().await;
            match received {
                Some(Ok(Message::Text(text))) => return Ok(Some(text.as_str().to_string())),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Binary(_))) => {
                    varyk_std::tracing::debug!("a WebSocket client sent a binary message");
                    closing(slot.take(), Some(close_code::UNSUPPORTED));
                    self.client_closed.store(true, Ordering::SeqCst);
                    return Ok(None);
                }
                // The read that follows a client's close sends the reply.
                Some(Ok(Message::Close(_))) | None => {
                    closing(slot.take(), None);
                    self.client_closed.store(true, Ordering::SeqCst);
                    return Ok(None);
                }
                Some(Err(err)) => {
                    let socket = slot.take();
                    if let Some(code) = read_failed(err)? {
                        closing(socket, Some(code));
                    }
                    self.client_closed.store(true, Ordering::SeqCst);
                    return Ok(None);
                }
            }
        }
    }

    /// Sends `text` as one message; an `Err` when the connection is closed
    /// or gone.
    pub async fn send(&self, text: &str) -> Result<bool, varyk_std::Error> {
        let mut slot = self.socket.lock().await;
        let Some(socket) = slot.as_mut() else {
            if self.client_closed.load(Ordering::SeqCst) {
                return Err(crate::message::client_gone(
                    &self.gone,
                    "the message cannot be sent: the client has closed the WebSocket connection",
                ));
            }
            return Err(varyk_std::Error::new(
                "the message cannot be sent: the WebSocket connection is closed".to_string(),
            ));
        };
        match socket.send(Message::Text(Utf8Bytes::from(text))).await {
            Ok(()) => Ok(true),
            Err(err) => {
                *slot = None;
                varyk_std::tracing::debug!("a WebSocket message cannot be sent: {err}");
                Err(crate::message::client_gone(
                    &self.gone,
                    "the message cannot be sent: the WebSocket connection is gone",
                ))
            }
        }
    }

    /// Closes the connection normally: `true`, or `false` when it was
    /// closed already; an `Err` when it is gone.
    pub async fn close(&self) -> Result<bool, varyk_std::Error> {
        let Some(mut socket) = self.socket.lock().await.take() else {
            return Ok(false);
        };
        match socket.send(close_message(close_code::NORMAL)).await {
            Ok(()) => {
                closing(Some(socket), None);
                Ok(true)
            }
            Err(err) => {
                varyk_std::tracing::debug!("a WebSocket connection cannot be closed: {err}");
                Err(crate::message::client_gone(
                    &self.gone,
                    "the WebSocket connection cannot be closed: it is gone",
                ))
            }
        }
    }
}

/// When the handler returns, the connection closes normally.
impl Drop for WebSocket {
    fn drop(&mut self) {
        closing(self.socket.get_mut().take(), Some(close_code::NORMAL));
    }
}

/// What a failed read means: `Ok` with the close code to send when the
/// connection can still be closed (1009 for a message over the limit,
/// 1007 for text that is not UTF-8, 1002 for a malformed message, none
/// for a client that went away), an `Err` for any other failure. Each of
/// those is the client's doing, so it is logged at debug level only.
fn read_failed(err: axum::Error) -> Result<Option<u16>, varyk_std::Error> {
    use tokio_tungstenite::tungstenite::Error as Failure;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    let err = match err.into_inner().downcast::<Failure>() {
        Ok(err) => *err,
        Err(other) => {
            return Err(varyk_std::Error::new(format!(
                "the WebSocket connection failed: {other}"
            )));
        }
    };
    match err {
        Failure::Capacity(_) => {
            varyk_std::tracing::debug!("a WebSocket client sent a message over the body limit");
            Ok(Some(close_code::SIZE))
        }
        Failure::Utf8(_) => {
            varyk_std::tracing::debug!("a WebSocket client sent text that is not UTF-8");
            Ok(Some(close_code::INVALID))
        }
        Failure::ConnectionClosed
        | Failure::AlreadyClosed
        | Failure::Io(_)
        | Failure::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            varyk_std::tracing::debug!("a WebSocket client went away: {err}");
            Ok(None)
        }
        Failure::Protocol(protocol) => {
            varyk_std::tracing::debug!("a WebSocket client sent a malformed message: {protocol}");
            Ok(Some(close_code::PROTOCOL))
        }
        other => Err(varyk_std::Error::new(format!(
            "the WebSocket connection failed: {other}"
        ))),
    }
}

fn close_message(code: u16) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: Utf8Bytes::default(),
    }))
}

/// Ends `socket` on a task of its own: sends a close with `code` when
/// given, then reads on until the client's close in reply, for at most
/// `CLOSING`, so the closing handshake completes and the handler need not
/// wait for it.
fn closing(socket: Option<axum::extract::ws::WebSocket>, code: Option<u16>) {
    let (Some(mut socket), Ok(runtime)) = (socket, tokio::runtime::Handle::try_current()) else {
        return;
    };
    runtime.spawn(async move {
        if let Some(code) = code {
            if socket.send(close_message(code)).await.is_err() {
                return;
            }
        }
        let _ = tokio::time::timeout(CLOSING, async {
            while let Some(Ok(_)) = socket.recv().await {}
        })
        .await;
    });
}

impl crate::message::Bound for Sse {
    /// Sends the `text/event-stream` response early, before the handler
    /// runs, so each event goes out as it is sent (spec 6.3).
    async fn bound(request: &crate::message::Request) -> Result<Sse, crate::message::Response> {
        let (events, receiver) = tokio::sync::mpsc::channel(EVENTS_WAITING);
        if !request.send_early(crate::message::Response::events(receiver)) {
            return Err(crate::message::internal_because(
                "the event stream cannot be started: this request's response was sent already, so a handler takes one `Sse` or `WebSocket` at most"
                    .to_string(),
            ));
        }
        Ok(Sse {
            events,
            gone: request.client_gone(),
        })
    }
}

impl Sse {
    /// Sends `text` as one event, a `data:` line for each of its lines;
    /// an `Err` once the client has gone.
    pub async fn send(&self, text: &str) -> Result<bool, varyk_std::Error> {
        self.deliver(event(None, text)).await
    }

    /// Sends `text` as one event named `name`; an `Err` once the client
    /// has gone, and for a name with a line break.
    pub async fn send_event(&self, name: &str, text: &str) -> Result<bool, varyk_std::Error> {
        if name.contains(['\n', '\r']) {
            return Err(varyk_std::Error::new(format!(
                "the event name {name:?} cannot hold a line break"
            )));
        }
        self.deliver(event(Some(name), text)).await
    }

    async fn deliver(&self, event: String) -> Result<bool, varyk_std::Error> {
        match self.events.send(event).await {
            Ok(()) => Ok(true),
            Err(_) => {
                varyk_std::tracing::debug!("an event stream's client has gone");
                Err(crate::message::client_gone(
                    &self.gone,
                    "the event cannot be sent: the client has gone",
                ))
            }
        }
    }
}

/// One event as the format writes it: an `event:` line for a name, a
/// `data:` line for each line of `text`, and a blank line.
fn event(name: Option<&str>, text: &str) -> String {
    let mut out = String::new();
    if let Some(name) = name {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    for line in text.replace("\r\n", "\n").split(['\n', '\r']) {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

impl crate::message::Bound for Multipart {
    async fn bound(
        request: &crate::message::Request,
    ) -> Result<Multipart, crate::message::Response> {
        let content_type = request.header("content-type").unwrap_or_default();
        if !crate::message::is_multipart(&content_type) {
            return Err(crate::message::error_body(
                400,
                "the body is not a multipart/form-data form",
            ));
        }
        let boundary = multer::parse_boundary(&content_type).map_err(|_| {
            crate::message::error_body(400, "the multipart/form-data form has no boundary")
        })?;
        // The route wrapper keeps the body only when it read the request
        // as a form; a content type it read otherwise (a second header, a
        // byte that is not ASCII) is not a form here either. The compiler
        // binds one `Multipart`, so the body is never taken twice.
        let Some(upload) = request.take_upload() else {
            return Err(crate::message::error_body(
                400,
                "the body is not a multipart/form-data form",
            ));
        };
        // The whole body counts against the limit, every part included,
        // and the parts a handler skips too.
        let limit = upload.limit;
        let over_limit = upload.over_limit;
        let mut seen: u64 = 0;
        let stream = upload.body.into_data_stream().map(move |chunk| {
            let chunk = chunk.map_err(|err| -> BoxError { Box::new(err) })?;
            seen = seen.saturating_add(chunk.len() as u64);
            if seen > limit {
                over_limit.store(true, Ordering::SeqCst);
                return Err(BoxError::from("the body is larger than the limit"));
            }
            Ok(chunk)
        });
        Ok(Multipart {
            form: Arc::new(tokio::sync::Mutex::new(Form {
                multipart: multer::Multipart::new(stream, boundary),
                field: None,
                index: 0,
            })),
        })
    }
}

impl Multipart {
    /// The next part, `None` after the last. A part given earlier can no
    /// longer be read.
    pub async fn next(&self) -> Result<Option<Part>, varyk_std::Error> {
        let mut form = self.form.lock().await;
        form.index = form.index.saturating_add(1);
        // multer reads past what is left of the live field once it is
        // dropped.
        form.field = None;
        match form.multipart.next_field().await {
            Ok(Some(field)) => {
                let part = Part {
                    name: field.name().unwrap_or_default().to_string(),
                    file_name: field.file_name().map(|name| name.to_string()),
                    index: form.index,
                    form: Arc::clone(&self.form),
                };
                form.field = Some(field);
                Ok(Some(part))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(unreadable(err)),
        }
    }
}

impl Part {
    /// The form field's name.
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// The file name the client gave: the client's word, never a safe
    /// name to save under.
    pub fn file_name(&self) -> Option<String> {
        self.file_name.clone()
    }

    /// The part's content as text; an `Err` when it is not UTF-8, or was
    /// read already.
    pub async fn text(&self) -> Result<String, varyk_std::Error> {
        let mut form = self.form.lock().await;
        let mut field = self.take(&mut form)?;
        let mut read = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(unreadable)? {
            read.extend_from_slice(&chunk);
        }
        String::from_utf8(read).map_err(|_| {
            varyk_std::Error::new(format!("the part {:?} is not UTF-8 text", self.name))
        })
    }

    /// Writes the part's content to the file `name` in the folder `dir`
    /// and gives the bytes written. A name is refused by the rule of
    /// `Response::file`, with nothing written; the content goes to a
    /// temporary file in the folder first, renamed into place when
    /// complete, so a failed upload leaves no partial file.
    pub async fn save_to(&self, dir: &str, name: &str) -> Result<u64, varyk_std::Error> {
        let cannot = |reason: String| {
            varyk_std::Error::new(format!(
                "the part {:?} cannot be saved as {name:?} in {dir:?}: {reason}",
                self.name
            ))
        };
        let target = target_in(dir, name).map_err(cannot)?;
        let mut temporary = Temporary::create(&target).await.map_err(cannot)?;
        let mut form = self.form.lock().await;
        let mut field = self.take(&mut form)?;
        let mut written: u64 = 0;
        while let Some(chunk) = field.chunk().await.map_err(unreadable)? {
            temporary
                .file
                .write_all(&chunk)
                .await
                .map_err(|err| cannot(err.to_string()))?;
            written = written.saturating_add(chunk.len() as u64);
        }
        temporary
            .file
            .flush()
            .await
            .map_err(|err| cannot(err.to_string()))?;
        tokio::fs::rename(&temporary.path, &target)
            .await
            .map_err(|err| cannot(err.to_string()))?;
        temporary.kept();
        Ok(written)
    }

    /// The live field of this part, taken to be read; an `Err` when the
    /// form has moved on or the part was read already.
    fn take(&self, form: &mut Form) -> Result<multer::Field<'static>, varyk_std::Error> {
        if form.index != self.index {
            return Err(varyk_std::Error::new(format!(
                "the part {:?} cannot be read: the form has moved on to a later part",
                self.name
            )));
        }
        form.field.take().ok_or_else(|| {
            varyk_std::Error::new(format!("the part {:?} was read already", self.name))
        })
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A form that cannot be read, with no status: a handler passing it on
/// gives the fixed 500, multer's message logged, never sent.
fn unreadable(err: multer::Error) -> varyk_std::Error {
    match err {
        multer::Error::StreamReadFailed(_) => {
            varyk_std::Error::new("the form cannot be read".to_string())
        }
        other => varyk_std::Error::new(format!("the form is not valid: {other}")),
    }
}

/// Where `save_to` writes the file `name` in the folder `dir`: the reason
/// when the name is not plain, or its folder is not inside `dir` (links
/// followed). The file itself may be a link: the rename replaces it.
fn target_in(dir: &str, name: &str) -> Result<PathBuf, String> {
    crate::message::plain_name(name)?;
    let folder = crate::message::folder(dir)?;
    let joined = folder.join(name);
    let (Some(parent), Some(file_name)) = (joined.parent(), joined.file_name()) else {
        return Err("the name is not a file name".to_string());
    };
    let parent = std::fs::canonicalize(parent)
        .map_err(|err| format!("the folder of the name cannot be found: {err}"))?;
    if !parent.starts_with(&folder) {
        return Err("the name leads out of the folder".to_string());
    }
    Ok(parent.join(file_name))
}

/// A temporary file beside the target, removed when dropped unless kept.
struct Temporary {
    path: PathBuf,
    file: tokio::fs::File,
    remove: bool,
}

/// Tells the temporary files of one process apart.
static TEMPORARY: AtomicU64 = AtomicU64::new(0);

impl Temporary {
    async fn create(target: &Path) -> Result<Temporary, String> {
        let folder = target.parent().unwrap_or(Path::new("."));
        let count = TEMPORARY.fetch_add(1, Ordering::SeqCst);
        let path = folder.join(format!(".varyk-upload-{}-{count}", std::process::id()));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|err| format!("the temporary file cannot be made: {err}"))?;
        Ok(Temporary {
            path,
            file,
            remove: true,
        })
    }

    /// The file was renamed into place: nothing to remove.
    fn kept(&mut self) {
        self.remove = false;
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if self.remove {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// A WebSocket conversation against a real `serve` on a loopback port, a
// fixed port per test, with `tokio-tungstenite` as the client: what a
// Varyk program cannot reach (spec 7). The routes are written as the
// compiler's adapters are.
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    use super::WebSocket;
    use crate::message::{Request, Response, respond_empty, respond_error};
    use crate::server::{App, route};

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Echoes each message; `bye` closes from the server's side.
    async fn echo(ws: WebSocket) -> Result<bool, varyk_std::Error> {
        while let Some(text) = ws.recv().await? {
            if text == "bye" {
                ws.close().await?;
            } else {
                ws.send(&text).await?;
            }
        }
        Ok(true)
    }

    /// Sends one message and returns, which closes the connection.
    async fn hello(ws: WebSocket) -> Result<bool, varyk_std::Error> {
        ws.send("hi").await
    }

    async fn live(req: Request, handler: fn(WebSocket) -> Handled) -> Response {
        let ws = match req.bind::<WebSocket>().await {
            Ok(ws) => ws,
            Err(response) => return response,
        };
        match handler(ws).await {
            Ok(_) => respond_empty(),
            Err(error) => respond_error(error),
        }
    }

    type Handled =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, varyk_std::Error>> + Send>>;

    /// Serves the test routes on `port`, with a body limit of 64 bytes.
    async fn serve(port: u16) {
        let mut app = App::new(Arc::new(()));
        app.set_body_limit(64);
        app.get("/echo", route(|req| live(req, |ws| Box::pin(echo(ws)))));
        app.get("/hello", route(|req| live(req, |ws| Box::pin(hello(ws)))));
        app.get(
            "/rooms/{id}",
            route(|req| async move {
                let _id = match req.param::<u64>("id") {
                    Ok(id) => id,
                    Err(response) => return response,
                };
                live(req, |ws| Box::pin(echo(ws))).await
            }),
        );
        tokio::spawn(async move {
            if let Err(err) = app.serve(port).await {
                panic!("the app cannot serve: {}", err.message());
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn connect(port: u16, path: &str) -> Client {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}{path}")).await {
            Ok((client, response)) => {
                assert_eq!(response.status().as_u16(), 101);
                client
            }
            Err(err) => panic!("cannot connect: {err}"),
        }
    }

    /// The next message, within two seconds.
    async fn next(client: &mut Client) -> Message {
        match tokio::time::timeout(Duration::from_secs(2), client.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(err))) => panic!("cannot read: {err}"),
            Ok(None) => panic!("the connection ended"),
            Err(_) => panic!("no message within two seconds"),
        }
    }

    async fn send(client: &mut Client, message: Message) {
        if let Err(err) = client.send(message).await {
            panic!("cannot send: {err}");
        }
    }

    /// The code of the close the server sent next.
    async fn closed_with(client: &mut Client) -> CloseCode {
        match next(client).await {
            Message::Close(Some(frame)) => frame.code,
            other => panic!("expected a close, got {other:?}"),
        }
    }

    #[test]
    fn text_is_echoed_and_a_ping_answered() {
        varyk_std::run(async {
            serve(41812).await;
            let mut client = connect(41812, "/echo").await;
            send(&mut client, Message::text("hello")).await;
            assert_eq!(next(&mut client).await, Message::text("hello"));
            send(&mut client, Message::Ping("are you there".into())).await;
            assert_eq!(
                next(&mut client).await,
                Message::Pong("are you there".into())
            );
            send(&mut client, Message::text("again")).await;
            assert_eq!(next(&mut client).await, Message::text("again"));
        });
    }

    #[test]
    fn a_binary_message_closes_with_1003() {
        varyk_std::run(async {
            serve(41813).await;
            let mut client = connect(41813, "/echo").await;
            send(&mut client, Message::binary(vec![1, 2, 3])).await;
            assert_eq!(closed_with(&mut client).await, CloseCode::Unsupported);
        });
    }

    #[test]
    fn a_message_over_the_body_limit_closes_with_1009() {
        varyk_std::run(async {
            serve(41814).await;
            let mut client = connect(41814, "/echo").await;
            send(&mut client, Message::text("x".repeat(100))).await;
            assert_eq!(closed_with(&mut client).await, CloseCode::Size);
        });
    }

    #[test]
    fn text_that_is_not_utf8_closes_with_1007() {
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        varyk_std::run(async {
            serve(41817).await;
            let mut client = connect(41817, "/echo").await;
            let frame = Frame::message(vec![0xff, 0xfe], OpCode::Data(Data::Text), true);
            send(&mut client, Message::Frame(frame)).await;
            assert_eq!(closed_with(&mut client).await, CloseCode::Invalid);
        });
    }

    #[test]
    fn either_side_closes_normally() {
        varyk_std::run(async {
            serve(41815).await;
            // The client closes: the handler's `recv` gives `None`, and the
            // server replies.
            let mut client = connect(41815, "/echo").await;
            if let Err(err) = client.close(None).await {
                panic!("cannot close: {err}");
            }
            assert!(matches!(next(&mut client).await, Message::Close(_)));
            // The handler closes.
            let mut client = connect(41815, "/echo").await;
            send(&mut client, Message::text("bye")).await;
            assert_eq!(closed_with(&mut client).await, CloseCode::Normal);
            // The handler returns.
            let mut client = connect(41815, "/hello").await;
            assert_eq!(next(&mut client).await, Message::text("hi"));
            assert_eq!(closed_with(&mut client).await, CloseCode::Normal);
        });
    }

    #[test]
    fn a_path_parameter_that_does_not_parse_is_a_400_not_a_101() {
        varyk_std::run(async {
            serve(41816).await;
            match tokio_tungstenite::connect_async("ws://127.0.0.1:41816/rooms/abc").await {
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), 400);
                }
                Err(err) => panic!("expected a 400, got {err}"),
                Ok(_) => panic!("expected a 400, got a 101"),
            }
            let mut client = connect(41816, "/rooms/7").await;
            send(&mut client, Message::text("in")).await;
            assert_eq!(next(&mut client).await, Message::text("in"));
        });
    }
}
