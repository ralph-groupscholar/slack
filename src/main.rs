use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    fs,
    path::Path,
    process::Command,
    sync::mpsc,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use egui_wgpu::{Renderer, ScreenDescriptor};
use egui_winit::State as EguiWinitState;
use image::{imageops::FilterType, GenericImageView, ImageReader};
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use tungstenite::{connect, Message as WsMessage};
use url::Url;
use wgpu::{CompositeAlphaMode, PresentMode, SurfaceError, TextureUsages};
use winit::{
    dpi::PhysicalSize,
    event::{Event, WindowEvent},
    event_loop::EventLoop,
    window::{Window, WindowBuilder},
};

#[derive(Clone)]
struct Message {
    id: i64,
    author: String,
    body: String,
    sent_at: String,
    channel_id: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelKind {
    Channel,
    DirectMessage,
}

impl ChannelKind {
    fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Channel => "channel",
            ChannelKind::DirectMessage => "dm",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "channel" => ChannelKind::Channel,
            "dm" => ChannelKind::DirectMessage,
            _ => ChannelKind::Channel,
        }
    }
}

#[derive(Clone)]
struct Channel {
    id: i64,
    name: String,
    kind: ChannelKind,
}

struct ComposerMeta {
    placeholder: String,
    typing_stub: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RealtimeStatus {
    Disconnected,
    Connecting,
    Connected,
}

impl RealtimeStatus {
    fn label(self) -> &'static str {
        match self {
            RealtimeStatus::Disconnected => "Disconnected",
            RealtimeStatus::Connecting => "Connecting",
            RealtimeStatus::Connected => "Connected",
        }
    }
}

enum RealtimeCommand {
    Connect,
    Disconnect,
    SendMessage {
        author: String,
        body: String,
        sent_at: String,
        channel_id: i64,
        attachments: Vec<RealtimeAttachment>,
    },
}

struct RealtimeEvent {
    status: RealtimeStatus,
    message: Option<String>,
    error: Option<String>,
    inbound: Option<IncomingMessage>,
    presence: Option<PresenceUpdate>,
}

struct RealtimeClient {
    status: RealtimeStatus,
    last_message: Option<String>,
    last_error: Option<String>,
    target_url: String,
    cmd_tx: Option<mpsc::Sender<RealtimeCommand>>,
    evt_rx: Option<mpsc::Receiver<RealtimeEvent>>,
    incoming: Vec<IncomingMessage>,
    incoming_presence: Vec<PresenceUpdate>,
}

#[derive(Clone)]
struct PresenceUpdate {
    user: String,
    status: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct RealtimeAttachment {
    file_path: String,
    file_name: String,
    file_size: i64,
    kind: String,
}

struct IncomingMessage {
    message: Message,
    attachments: Vec<RealtimeAttachment>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RealtimePayload {
    Message {
        author: String,
        body: String,
        sent_at: String,
        channel_id: i64,
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<RealtimeAttachment>,
    },
    Auth {
        token: String,
        user: String,
    },
    Ack {
        kind: String,
        detail: String,
    },
    Presence {
        user: String,
        status: String,
    },
}

impl RealtimePayload {
    fn from_message(message: &Message, attachments: Vec<RealtimeAttachment>) -> Self {
        Self::Message {
            author: message.author.clone(),
            body: message.body.clone(),
            sent_at: message.sent_at.clone(),
            channel_id: message.channel_id,
            client_id: None,
            attachments,
        }
    }

    fn into_message(self) -> Option<IncomingMessage> {
        match self {
            RealtimePayload::Message {
                author,
                body,
                sent_at,
                channel_id,
                client_id: _,
                attachments,
            } => Some(IncomingMessage {
                message: Message {
                    id: 0,
                    author,
                    body,
                    sent_at,
                    channel_id,
                },
                attachments,
            }),
            _ => None,
        }
    }
}

fn encode_realtime_message(
    message: &Message,
    attachments: Vec<RealtimeAttachment>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&RealtimePayload::from_message(message, attachments))
}

fn parse_legacy_message(text: &str) -> Option<IncomingMessage> {
    let mut parts = text.splitn(4, '\t');
    let author = parts.next()?;
    let channel_id = parts.next()?.parse::<i64>().ok()?;
    let sent_at = parts.next()?;
    let body = parts.next().unwrap_or(text);
    Some(IncomingMessage {
        message: Message {
            id: 0,
            author: author.to_string(),
            body: body.to_string(),
            sent_at: sent_at.to_string(),
            channel_id,
        },
        attachments: Vec::new(),
    })
}

enum RealtimeInbound {
    Message(IncomingMessage),
    Presence { user: String, status: String },
    Signal(String),
}

fn decode_realtime_inbound(text: &str) -> Result<RealtimeInbound, String> {
    match serde_json::from_str::<RealtimePayload>(text) {
        Ok(payload) => match payload {
            RealtimePayload::Message { .. } => Ok(RealtimeInbound::Message(
                payload
                    .into_message()
                    .ok_or_else(|| "unexpected payload".to_string())?,
            )),
            RealtimePayload::Ack { kind, detail } => {
                Ok(RealtimeInbound::Signal(format!("Ack: {kind} ({detail})")))
            }
            RealtimePayload::Presence { user, status } => Ok(RealtimeInbound::Presence {
                user,
                status,
            }),
            RealtimePayload::Auth { user, .. } => Ok(RealtimeInbound::Signal(format!(
                "Auth received for {user}"
            ))),
        },
        Err(err) => parse_legacy_message(text)
            .map(RealtimeInbound::Message)
            .ok_or_else(|| err.to_string()),
    }
}

impl RealtimeClient {
    fn new(target_url: String) -> Self {
        Self {
            status: RealtimeStatus::Disconnected,
            last_message: None,
            last_error: None,
            target_url,
            cmd_tx: None,
            evt_rx: None,
            incoming: Vec::new(),
            incoming_presence: Vec::new(),
        }
    }

    fn ensure_worker(&mut self) {
        if self.cmd_tx.is_some() {
            return;
        }
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        spawn_realtime_worker(cmd_rx, evt_tx, self.target_url.clone());
        self.cmd_tx = Some(cmd_tx);
        self.evt_rx = Some(evt_rx);
    }

    fn connect(&mut self) {
        self.ensure_worker();
        if let Some(cmd_tx) = self.cmd_tx.as_ref() {
            let _ = cmd_tx.send(RealtimeCommand::Connect);
        }
    }

    fn disconnect(&mut self) {
        if let Some(cmd_tx) = self.cmd_tx.as_ref() {
            let _ = cmd_tx.send(RealtimeCommand::Disconnect);
        } else {
            self.status = RealtimeStatus::Disconnected;
            self.last_message = Some("Already disconnected".to_string());
        }
    }

    fn send_message(&self, message: &Message, attachments: Vec<RealtimeAttachment>) {
        if let Some(cmd_tx) = self.cmd_tx.as_ref() {
            let _ = cmd_tx.send(RealtimeCommand::SendMessage {
                author: message.author.clone(),
                body: message.body.clone(),
                sent_at: message.sent_at.clone(),
                channel_id: message.channel_id,
                attachments,
            });
        }
    }

    fn poll(&mut self) {
        if let Some(evt_rx) = self.evt_rx.as_ref() {
            while let Ok(event) = evt_rx.try_recv() {
                self.status = event.status;
                self.last_message = event.message;
                self.last_error = event.error;
                if let Some(message) = event.inbound {
                    self.incoming.push(message);
                }
                if let Some(presence) = event.presence {
                    self.incoming_presence.push(presence);
                }
            }
        }
    }

    fn take_incoming(&mut self) -> Vec<IncomingMessage> {
        self.incoming.drain(..).collect()
    }

    fn take_presence(&mut self) -> Vec<PresenceUpdate> {
        self.incoming_presence.drain(..).collect()
    }
}

fn spawn_realtime_worker(
    cmd_rx: mpsc::Receiver<RealtimeCommand>,
    evt_tx: mpsc::Sender<RealtimeEvent>,
    target_url: String,
) {
    thread::spawn(move || {
        let mut connected = false;
        let mut socket: Option<
            tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
        > = None;
        loop {
            match cmd_rx.recv_timeout(Duration::from_millis(16)) {
                Ok(command) => match command {
                    RealtimeCommand::Connect => {
                        if connected {
                            continue;
                        }
                        let _ = evt_tx.send(RealtimeEvent {
                            status: RealtimeStatus::Connecting,
                            message: Some(format!("Dialing {target_url}")),
                            error: None,
                            inbound: None,
                            presence: None,
                        });
                        match Url::parse(&target_url)
                            .map_err(|err| err.to_string())
                            .and_then(|url| {
                                connect(url)
                                    .map(|(socket, _response)| socket)
                                    .map_err(|err| err.to_string())
                            }) {
                            Ok(mut ws) => {
                                if let tungstenite::stream::MaybeTlsStream::Plain(stream) =
                                    ws.get_mut()
                                {
                                    let _ = stream.set_nonblocking(true);
                                }
                                connected = true;
                                socket = Some(ws);
                                if let Some(ws) = socket.as_mut() {
                                    let auth = RealtimePayload::Auth {
                                        token: "local-dev".to_string(),
                                        user: "you".to_string(),
                                    };
                                    match serde_json::to_string(&auth) {
                                        Ok(payload) => {
                                            if let Err(err) = ws.send(WsMessage::Text(payload)) {
                                                connected = false;
                                                socket = None;
                                                let _ = evt_tx.send(RealtimeEvent {
                                                    status: RealtimeStatus::Disconnected,
                                                    message: None,
                                                    error: Some(err.to_string()),
                                                    inbound: None,
                                                    presence: None,
                                                });
                                                continue;
                                            }
                                        }
                                        Err(err) => {
                                            let _ = evt_tx.send(RealtimeEvent {
                                                status: RealtimeStatus::Connected,
                                                message: None,
                                                error: Some(err.to_string()),
                                                inbound: None,
                                                presence: None,
                                            });
                                        }
                                    }
                                }
                                let _ = evt_tx.send(RealtimeEvent {
                                    status: RealtimeStatus::Connected,
                                    message: Some("Handshake complete".to_string()),
                                    error: None,
                                    inbound: None,
                                    presence: None,
                                });
                            }
                            Err(err) => {
                                connected = false;
                                socket = None;
                                let _ = evt_tx.send(RealtimeEvent {
                                    status: RealtimeStatus::Disconnected,
                                    message: None,
                                    error: Some(err),
                                    inbound: None,
                                    presence: None,
                                });
                            }
                        }
                    }
                    RealtimeCommand::Disconnect => {
                        if let Some(mut ws) = socket.take() {
                            let _ = ws.close(None);
                        }
                        connected = false;
                        let _ = evt_tx.send(RealtimeEvent {
                            status: RealtimeStatus::Disconnected,
                            message: Some("Closed socket".to_string()),
                            error: None,
                            inbound: None,
                            presence: None,
                        });
                    }
                    RealtimeCommand::SendMessage {
                        author,
                        body,
                        sent_at,
                        channel_id,
                        attachments,
                    } => {
                        if let Some(ws) = socket.as_mut() {
                            let message = Message {
                                id: 0,
                                author,
                                body,
                                sent_at,
                                channel_id,
                            };
                            match encode_realtime_message(&message, attachments) {
                                Ok(payload) => {
                                    if let Err(err) = ws.send(WsMessage::Text(payload)) {
                                        connected = false;
                                        socket = None;
                                        let _ = evt_tx.send(RealtimeEvent {
                                            status: RealtimeStatus::Disconnected,
                                            message: None,
                                            error: Some(err.to_string()),
                                            inbound: None,
                                            presence: None,
                                        });
                                    }
                                }
                                Err(err) => {
                                    let _ = evt_tx.send(RealtimeEvent {
                                        status: RealtimeStatus::Connected,
                                        message: None,
                                        error: Some(err.to_string()),
                                        inbound: None,
                                        presence: None,
                                    });
                                }
                            }
                        }
                    }
                },
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            if connected {
                if let Some(ws) = socket.as_mut() {
                    match ws.read() {
                        Ok(msg) => {
                            if let WsMessage::Text(text) = msg {
                                match decode_realtime_inbound(&text) {
                                    Ok(RealtimeInbound::Message(message)) => {
                                        let _ = evt_tx.send(RealtimeEvent {
                                            status: RealtimeStatus::Connected,
                                            message: Some("Message received".to_string()),
                                            error: None,
                                            inbound: Some(message),
                                            presence: None,
                                        });
                                    }
                                    Ok(RealtimeInbound::Presence { user, status }) => {
                                        let _ = evt_tx.send(RealtimeEvent {
                                            status: RealtimeStatus::Connected,
                                            message: Some(format!("Presence: {user} is {status}")),
                                            error: None,
                                            inbound: None,
                                            presence: Some(PresenceUpdate { user, status }),
                                        });
                                    }
                                    Ok(RealtimeInbound::Signal(signal)) => {
                                        let _ = evt_tx.send(RealtimeEvent {
                                            status: RealtimeStatus::Connected,
                                            message: Some(signal),
                                            error: None,
                                            inbound: None,
                                            presence: None,
                                        });
                                    }
                                    Err(err) => {
                                        let _ = evt_tx.send(RealtimeEvent {
                                            status: RealtimeStatus::Connected,
                                            message: None,
                                            error: Some(err),
                                            inbound: None,
                                            presence: None,
                                        });
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            let io_blocked = matches!(
                                err,
                                tungstenite::Error::Io(ref io_err)
                                    if io_err.kind() == std::io::ErrorKind::WouldBlock
                            );
                            if !io_blocked {
                                connected = false;
                                socket = None;
                                let _ = evt_tx.send(RealtimeEvent {
                                    status: RealtimeStatus::Disconnected,
                                    message: None,
                                    error: Some(err.to_string()),
                                    inbound: None,
                                    presence: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    });
}

fn seed_channels() -> Vec<(i64, &'static str, ChannelKind)> {
    vec![
        (1, "general", ChannelKind::Channel),
        (2, "product", ChannelKind::Channel),
        (3, "mara", ChannelKind::DirectMessage),
        (4, "devin", ChannelKind::DirectMessage),
    ]
}

fn seed_messages() -> Vec<Message> {
    vec![
        Message {
            id: 0,
            author: "mara".to_string(),
            body: "Shipping the new hotkey flow now.".to_string(),
            sent_at: "09:12".to_string(),
            channel_id: 1,
        },
        Message {
            id: 0,
            author: "devin".to_string(),
            body: "Latency on local echo is <100ms.".to_string(),
            sent_at: "09:13".to_string(),
            channel_id: 1,
        },
        Message {
            id: 0,
            author: "sasha".to_string(),
            body: "Message search index warmed on startup.".to_string(),
            sent_at: "09:15".to_string(),
            channel_id: 1,
        },
        Message {
            id: 0,
            author: "you".to_string(),
            body: "Feels fast. Let's keep it lean.".to_string(),
            sent_at: "09:18".to_string(),
            channel_id: 1,
        },
        Message {
            id: 0,
            author: "mara".to_string(),
            body: "Next: attachments + previews.".to_string(),
            sent_at: "09:21".to_string(),
            channel_id: 2,
        },
        Message {
            id: 0,
            author: "devin".to_string(),
            body: "Profiling idle CPU now.".to_string(),
            sent_at: "09:24".to_string(),
            channel_id: 2,
        },
        Message {
            id: 0,
            author: "mara".to_string(),
            body: "Can you sanity-check the build flags?".to_string(),
            sent_at: "09:26".to_string(),
            channel_id: 3,
        },
        Message {
            id: 0,
            author: "devin".to_string(),
            body: "Want me to share flamegraph results?".to_string(),
            sent_at: "09:28".to_string(),
            channel_id: 4,
        },
    ]
}

const MESSAGE_FETCH_LIMIT: i64 = 20;
const THUMBNAIL_CACHE_LIMIT: usize = 24;
const THUMBNAIL_ERROR_LIMIT: usize = 24;

fn format_timestamp_utc() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let hours = (now / 3600) % 24;
    let minutes = (now / 60) % 60;
    format!("{:02}:{:02}", hours, minutes)
}

fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS channels (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            kind TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            author TEXT NOT NULL,
            body TEXT NOT NULL,
            sent_at TEXT NOT NULL,
            channel_id INTEGER NOT NULL,
            FOREIGN KEY(channel_id) REFERENCES channels(id)
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS attachments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id INTEGER NOT NULL,
            file_path TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_size INTEGER NOT NULL,
            kind TEXT NOT NULL,
            FOREIGN KEY(message_id) REFERENCES messages(id)
        )",
        [],
    )?;
    let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut has_channel = false;
    for column in columns {
        if column? == "channel_id" {
            has_channel = true;
            break;
        }
    }
    if !has_channel {
        conn.execute(
            "ALTER TABLE messages ADD COLUMN channel_id INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    Ok(())
}

fn seed_channels_if_empty(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM channels", [], |row| row.get(0))?;
    if count == 0 {
        let tx = conn.transaction()?;
        for (id, name, kind) in seed_channels() {
            tx.execute(
                "INSERT INTO channels (id, name, kind) VALUES (?1, ?2, ?3)",
                params![id, name, kind.as_str()],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn seed_messages_if_empty(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    if count == 0 {
        let seed = seed_messages();
        let tx = conn.transaction()?;
        for message in seed {
            tx.execute(
                "INSERT INTO messages (author, body, sent_at, channel_id) VALUES (?1, ?2, ?3, ?4)",
                params![message.author, message.body, message.sent_at, message.channel_id],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn load_channels(conn: &Connection) -> Result<Vec<Channel>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id, name, kind FROM channels ORDER BY id ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok(Channel {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: ChannelKind::from_str(&row.get::<_, String>(2)?),
        })
    })?;

    let mut channels = Vec::new();
    for channel in rows {
        channels.push(channel?);
    }
    Ok(channels)
}

fn load_messages(conn: &Connection, channel_id: i64) -> Result<Vec<Message>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, author, body, sent_at, channel_id
        FROM messages
        WHERE channel_id = ?1
        ORDER BY id DESC
        LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![channel_id, MESSAGE_FETCH_LIMIT], |row| {
        Ok(Message {
            id: row.get(0)?,
            author: row.get(1)?,
            body: row.get(2)?,
            sent_at: row.get(3)?,
            channel_id: row.get(4)?,
        })
    })?;

    let mut messages = Vec::new();
    for message in rows {
        messages.push(message?);
    }
    messages.reverse();
    Ok(messages)
}

fn insert_message(conn: &Connection, message: &Message) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO messages (author, body, sent_at, channel_id) VALUES (?1, ?2, ?3, ?4)",
        params![message.author, message.body, message.sent_at, message.channel_id],
    )?;
    Ok(conn.last_insert_rowid())
}

fn load_channel_members(
    conn: &Connection,
    channels: &[Channel],
) -> Result<HashMap<i64, HashSet<String>>, rusqlite::Error> {
    let mut members: HashMap<i64, HashSet<String>> = HashMap::new();
    let mut stmt = conn.prepare("SELECT channel_id, author FROM messages")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (channel_id, author) = row?;
        members.entry(channel_id).or_default().insert(author);
    }
    for channel in channels {
        if channel.kind == ChannelKind::DirectMessage {
            members
                .entry(channel.id)
                .or_default()
                .insert(channel.name.clone());
        }
    }
    Ok(members)
}

fn build_composer_meta(channels: &[Channel]) -> HashMap<i64, ComposerMeta> {
    let mut meta = HashMap::new();
    for channel in channels {
        let (placeholder, typing_stub) = match channel.kind {
            ChannelKind::Channel => (
                format!("Message #{}", channel.name),
                "No one is typing.".to_string(),
            ),
            ChannelKind::DirectMessage => (
                format!("Message @{}", channel.name),
                format!("{} is typing...", channel.name),
            ),
        };
        meta.insert(
            channel.id,
            ComposerMeta {
                placeholder,
                typing_stub,
            },
        );
    }
    meta
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresenceStatus {
    Online,
    Away,
    Offline,
    Unknown,
}

impl PresenceStatus {
    fn from_str(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "online" => PresenceStatus::Online,
            "away" => PresenceStatus::Away,
            "offline" => PresenceStatus::Offline,
            _ => PresenceStatus::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            PresenceStatus::Online => "online",
            PresenceStatus::Away => "away",
            PresenceStatus::Offline => "offline",
            PresenceStatus::Unknown => "unknown",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            PresenceStatus::Online => egui::Color32::from_rgb(120, 210, 120),
            PresenceStatus::Away => egui::Color32::from_rgb(220, 180, 80),
            PresenceStatus::Offline => egui::Color32::from_rgb(130, 140, 160),
            PresenceStatus::Unknown => egui::Color32::from_rgb(120, 130, 150),
        }
    }
}

struct PresenceState {
    status: PresenceStatus,
    last_seen: Instant,
}

struct SearchRequest {
    query: String,
    channel_only: bool,
}

#[derive(Clone)]
struct Attachment {
    message_id: i64,
    file_path: String,
    file_name: String,
    file_size: i64,
    kind: String,
}

#[derive(Clone)]
struct PendingAttachment {
    file_path: String,
    file_name: String,
    file_size: i64,
    kind: String,
}

struct ThumbnailResult {
    path: String,
    image: Option<egui::ColorImage>,
    error: Option<String>,
}

struct DeferredLoadResult {
    channel_id: i64,
    channels: Vec<Channel>,
    messages: Vec<Message>,
    attachments: HashMap<i64, Vec<Attachment>>,
    channel_members: HashMap<i64, HashSet<String>>,
    db_ready: bool,
}

struct App {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_state: EguiWinitState,
    egui_ctx: egui::Context,
    egui_renderer: Renderer,
    boot_started: Instant,
    first_frame_logged: bool,
    exit_after_first_frame: bool,
    exit_requested: bool,
    started_at: Instant,
    db: Connection,
    channels: Vec<Channel>,
    messages: Vec<Message>,
    selected_channel_id: i64,
    composer_drafts: HashMap<i64, String>,
    composer_focus_requested: bool,
    composer_meta: HashMap<i64, ComposerMeta>,
    typing_state: HashMap<i64, Instant>,
    realtime: RealtimeClient,
    channel_members: HashMap<i64, HashSet<String>>,
    presence_state: HashMap<String, PresenceState>,
    search_query: String,
    search_last_query: String,
    search_results: Vec<Message>,
    search_channel_only: bool,
    search_last_channel_only: bool,
    messages_loaded: bool,
    message_attachments: HashMap<i64, Vec<Attachment>>,
    attachment_path_drafts: HashMap<i64, String>,
    pending_attachments: HashMap<i64, Vec<PendingAttachment>>,
    attachment_error: Option<String>,
    attachment_action_error: Option<String>,
    attachment_thumbnails: HashMap<String, egui::TextureHandle>,
    attachment_thumbnail_errors: HashMap<String, String>,
    thumbnail_cache_order: VecDeque<String>,
    thumbnail_error_order: VecDeque<String>,
    thumbnail_sender: mpsc::Sender<ThumbnailResult>,
    thumbnail_receiver: mpsc::Receiver<ThumbnailResult>,
    thumbnail_in_flight: HashSet<String>,
    deferred_load_receiver: Option<mpsc::Receiver<DeferredLoadResult>>,
}

impl App {
    fn new(event_loop: &EventLoop<()>, boot_started: Instant, exit_after_first_frame: bool) -> Self {
        let window = Arc::new(
            WindowBuilder::new()
                .with_title("Ralph")
                .with_inner_size(PhysicalSize::new(1100, 720))
                .build(event_loop)
                .expect("window"),
        );

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ralph-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
            },
            None,
        ))
        .expect("device");

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|format| format.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: surface_caps
                .alpha_modes
                .iter()
                .copied()
                .find(|mode| *mode == CompositeAlphaMode::Opaque)
                .unwrap_or(surface_caps.alpha_modes[0]),
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let egui_ctx = egui::Context::default();
        let egui_state = EguiWinitState::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            None,
            None,
        );
        let egui_renderer = Renderer::new(&device, surface_format, None, 1);

        let db = match Connection::open("ralph.db") {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("db open error: {err}");
                Connection::open_in_memory().expect("memory db")
            }
        };
        let channels: Vec<Channel> = seed_channels()
            .into_iter()
            .map(|(id, name, kind)| Channel {
                id,
                name: name.to_string(),
                kind,
            })
            .collect();
        let selected_channel_id = channels.first().map(|channel| channel.id).unwrap_or(1);
        let composer_meta = build_composer_meta(&channels);
        let messages = Vec::new();
        let deferred_channel_id = selected_channel_id;
        let channels_for_load = channels.clone();
        let (deferred_load_sender, deferred_load_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut db = match Connection::open("ralph.db") {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("db open error (deferred): {err}");
                    let messages = seed_messages()
                        .into_iter()
                        .filter(|message| message.channel_id == deferred_channel_id)
                        .collect();
                    let _ = deferred_load_sender.send(DeferredLoadResult {
                        channel_id: deferred_channel_id,
                        channels: channels_for_load.clone(),
                        messages,
                        attachments: HashMap::new(),
                        channel_members: HashMap::new(),
                        db_ready: false,
                    });
                    return;
                }
            };
            let mut db_ready = true;
            if let Err(err) = ensure_schema(&db) {
                eprintln!("db schema error (deferred): {err}");
                db_ready = false;
            }
            if let Err(err) = seed_channels_if_empty(&mut db) {
                eprintln!("db seed channels error (deferred): {err}");
            }
            if let Err(err) = seed_messages_if_empty(&mut db) {
                eprintln!("db seed error (deferred): {err}");
            }
            let channels = match load_channels(&db) {
                Ok(channels) if !channels.is_empty() => channels,
                Ok(_) => channels_for_load.clone(),
                Err(err) => {
                    eprintln!("db channels load error (deferred): {err}");
                    channels_for_load.clone()
                }
            };
            let load_channel_id = channels
                .iter()
                .find(|channel| channel.id == deferred_channel_id)
                .map(|channel| channel.id)
                .or_else(|| channels.first().map(|channel| channel.id))
                .unwrap_or(deferred_channel_id);
            let messages = match load_messages(&db, load_channel_id) {
                Ok(messages) => messages,
                Err(err) => {
                    eprintln!("db load error (deferred): {err}");
                    seed_messages()
                        .into_iter()
                        .filter(|message| message.channel_id == load_channel_id)
                        .collect()
                }
            };
            let message_ids: Vec<i64> = messages.iter().map(|message| message.id).collect();
            let attachments = match load_attachments_for_message_ids(&db, &message_ids) {
                Ok(attachments) => attachments,
                Err(err) => {
                    eprintln!("db attachments load error (deferred): {err}");
                    HashMap::new()
                }
            };
            let channel_members = match load_channel_members(&db, &channels) {
                Ok(members) => members,
                Err(err) => {
                    eprintln!("db members load error (deferred): {err}");
                    HashMap::new()
                }
            };
            let _ = deferred_load_sender.send(DeferredLoadResult {
                channel_id: load_channel_id,
                channels,
                messages,
                attachments,
                channel_members,
                db_ready,
            });
        });
        let mut presence_state = HashMap::new();
        presence_state.insert(
            "you".to_string(),
            PresenceState {
                status: PresenceStatus::Online,
                last_seen: Instant::now(),
            },
        );

        let (thumbnail_sender, thumbnail_receiver) = mpsc::channel();

        Self {
            window,
            surface,
            device,
            queue,
            config,
            egui_state,
            egui_ctx,
            egui_renderer,
            boot_started,
            first_frame_logged: false,
            exit_after_first_frame,
            exit_requested: false,
            started_at: Instant::now(),
            db,
            channels,
            messages,
            selected_channel_id,
            composer_drafts: HashMap::new(),
            composer_focus_requested: true,
            composer_meta,
            typing_state: HashMap::new(),
            realtime: RealtimeClient::new("ws://127.0.0.1:9001".to_string()),
            channel_members: HashMap::new(),
            presence_state,
            search_query: String::new(),
            search_last_query: String::new(),
            search_results: Vec::new(),
            search_channel_only: true,
            search_last_channel_only: true,
            messages_loaded: false,
            message_attachments: HashMap::new(),
            attachment_path_drafts: HashMap::new(),
            pending_attachments: HashMap::new(),
            attachment_error: None,
            attachment_action_error: None,
            attachment_thumbnails: HashMap::new(),
            attachment_thumbnail_errors: HashMap::new(),
            thumbnail_cache_order: VecDeque::new(),
            thumbnail_error_order: VecDeque::new(),
            thumbnail_sender,
            thumbnail_receiver,
            thumbnail_in_flight: HashSet::new(),
            deferred_load_receiver: Some(deferred_load_receiver),
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self) {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            let elapsed_ms = self.boot_started.elapsed().as_secs_f64() * 1000.0;
            println!("ralph: first_frame_ms={elapsed_ms:.2}");
            if self.exit_after_first_frame {
                self.exit_requested = true;
            }
        }
        self.realtime.poll();
        let incoming = self.realtime.take_incoming();
        let presence_updates = self.realtime.take_presence();
        if !presence_updates.is_empty() {
            for update in presence_updates {
                self.presence_state.insert(
                    update.user,
                    PresenceState {
                        status: PresenceStatus::from_str(&update.status),
                        last_seen: Instant::now(),
                    },
                );
            }
        }
        self.drain_thumbnail_results();
        self.apply_deferred_loads();
        let raw_input = self.egui_state.take_egui_input(self.window.as_ref());
        let mut pending_send: Option<String> = None;
        let mut pending_attachments_send = Vec::new();
        let mut channel_switch: Option<i64> = None;
        let mut search_request: Option<SearchRequest> = None;
        let mut search_clear = false;
        let mut realtime_connect = false;
        let mut realtime_disconnect = false;
        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| {
            egui::SidePanel::left("channel_list")
                .resizable(false)
                .default_width(220.0)
                .show(ctx, |ui| {
                    ui.heading("Ralph");
                    ui.add_space(10.0);
                    ui.label("Channels");
                    for channel in self
                        .channels
                        .iter()
                        .filter(|channel| channel.kind == ChannelKind::Channel)
                    {
                        ui.horizontal(|row| {
                            let label = format!("# {}", channel.name);
                            if row
                                .selectable_label(self.selected_channel_id == channel.id, label)
                                .clicked()
                            {
                                channel_switch = Some(channel.id);
                            }
                            let (online, total) = self.channel_presence_counts(channel.id);
                            let summary = if total == 0 {
                                "no members".to_string()
                            } else {
                                format!("{online}/{total} online")
                            };
                            row.label(
                                egui::RichText::new(summary)
                                    .small()
                                    .color(egui::Color32::from_rgb(120, 130, 150)),
                            );
                        });
                    }
                    ui.add_space(8.0);
                    ui.label("Direct Messages");
                    for channel in self
                        .channels
                        .iter()
                        .filter(|channel| channel.kind == ChannelKind::DirectMessage)
                    {
                        ui.horizontal(|row| {
                            let label = format!("@{}", channel.name);
                            if row
                                .selectable_label(self.selected_channel_id == channel.id, label)
                                .clicked()
                            {
                                channel_switch = Some(channel.id);
                            }
                            let status = self.presence_for_user(&channel.name);
                            row.label(
                                egui::RichText::new("o")
                                    .color(status.color())
                                    .small(),
                            );
                            row.label(
                                egui::RichText::new(status.label())
                                    .small()
                                    .color(status.color()),
                            );
                        });
                    }
                });
            egui::CentralPanel::default().show(ctx, |ui| {
                let channel_title = self
                    .channels
                    .iter()
                    .find(|channel| channel.id == self.selected_channel_id)
                    .map(|channel| match channel.kind {
                        ChannelKind::Channel => format!("#{}", channel.name),
                        ChannelKind::DirectMessage => format!("DM: {}", channel.name),
                    })
                    .unwrap_or_else(|| "Messages".to_string());
                ui.heading(format!("Ralph — {}", channel_title));
                ui.add_space(4.0);
                ui.label(format!(
                    "Session uptime: {:.1}s",
                    self.started_at.elapsed().as_secs_f32()
                ));
                ui.horizontal(|row| {
                    row.label(format!("Realtime: {}", self.realtime.status.label()));
                    row.label(
                        egui::RichText::new(&self.realtime.target_url)
                            .small()
                            .color(egui::Color32::from_rgb(120, 130, 150)),
                    );
                    match self.realtime.status {
                        RealtimeStatus::Disconnected => {
                            if row.button("Connect").clicked() {
                                realtime_connect = true;
                            }
                        }
                        RealtimeStatus::Connecting => {
                            row.add_enabled(false, egui::Button::new("Connecting..."));
                        }
                        RealtimeStatus::Connected => {
                            if row.button("Disconnect").clicked() {
                                realtime_disconnect = true;
                            }
                        }
                    }
                    if let Some(message) = &self.realtime.last_message {
                        row.label(
                            egui::RichText::new(message)
                                .small()
                                .color(egui::Color32::from_rgb(140, 150, 170)),
                        );
                    }
                    if let Some(error) = &self.realtime.last_error {
                        row.label(
                            egui::RichText::new(error)
                                .small()
                                .color(egui::Color32::from_rgb(220, 120, 120)),
                        );
                    }
                });
                if let Some(details) = self.channel_presence_details() {
                    ui.label(
                        egui::RichText::new(details)
                            .small()
                            .color(egui::Color32::from_rgb(120, 130, 150)),
                    );
                }
                ui.separator();
                ui.add_enabled_ui(self.messages_loaded, |ui| {
                    ui.horizontal(|row| {
                        row.label("Search");
                        let search_box = row.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .hint_text("Search messages")
                                .desired_width(240.0),
                        );
                        let search_enter = search_box.has_focus()
                            && row.input(|input| input.key_pressed(egui::Key::Enter));
                        if row.button("Go").clicked() || search_enter {
                            let trimmed = self.search_query.trim();
                            if !trimmed.is_empty() {
                                search_request = Some(SearchRequest {
                                    query: trimmed.to_string(),
                                    channel_only: self.search_channel_only,
                                });
                            }
                        }
                        row.checkbox(&mut self.search_channel_only, "This channel");
                        if row.button("Clear").clicked() {
                            search_clear = true;
                        }
                    });
                    if self.search_query.trim().is_empty() {
                        ui.label(
                            egui::RichText::new("Search by author or text.")
                                .small()
                                .color(egui::Color32::from_rgb(120, 130, 150)),
                        );
                    } else if self.search_last_query == self.search_query.trim()
                        && self.search_last_channel_only == self.search_channel_only
                    {
                        ui.label(
                            egui::RichText::new(format!("Results: {}", self.search_results.len()))
                                .small()
                                .color(egui::Color32::from_rgb(120, 130, 150)),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("Press Enter to search.")
                                .small()
                                .color(egui::Color32::from_rgb(120, 130, 150)),
                        );
                    }
                });
                if !self.messages_loaded {
                    ui.label(
                        egui::RichText::new("Search available once messages finish loading.")
                            .small()
                            .color(egui::Color32::from_rgb(120, 130, 150)),
                    );
                }
                ui.separator();
                let show_search_results =
                    !self.search_query.trim().is_empty()
                        && self.search_last_query == self.search_query.trim()
                        && self.search_last_channel_only == self.search_channel_only;
                let show_channel =
                    show_search_results && !self.search_channel_only;
        let messages = if show_search_results {
            &self.search_results
        } else {
            &self.messages
        };
        if show_search_results && messages.is_empty() {
            ui.label(
                egui::RichText::new("No matches found.")
                    .small()
                    .color(egui::Color32::from_rgb(160, 170, 190)),
            );
        } else if !show_search_results && !self.messages_loaded && messages.is_empty() {
            ui.label(
                egui::RichText::new("Loading messages...")
                    .small()
                    .color(egui::Color32::from_rgb(160, 170, 190)),
            );
        }
                let mut thumbnail_requests: Vec<String> = Vec::new();
                let mut touched_thumbnails: Vec<String> = Vec::new();
                let mut touched_errors: Vec<String> = Vec::new();
                for message in messages {
                    ui.horizontal(|row| {
                        row.label(
                            egui::RichText::new(&message.author)
                                .strong()
                                .color(egui::Color32::from_rgb(200, 210, 230)),
                        );
                        row.label(
                            egui::RichText::new(&message.sent_at)
                                .color(egui::Color32::from_rgb(140, 150, 170)),
                        );
                        if show_channel {
                            row.label(
                                egui::RichText::new(self.channel_label(message.channel_id))
                                    .small()
                                    .color(egui::Color32::from_rgb(140, 150, 170)),
                            );
                        }
                        row.label(&message.body);
                    });
                    if let Some(attachments) = self.message_attachments.get(&message.id) {
                        for attachment in attachments {
                            if attachment.kind == "image" {
                                let path = attachment.file_path.as_str();
                                let thumbnail = if self.attachment_thumbnails.contains_key(path) {
                                    touched_thumbnails.push(path.to_string());
                                    self.attachment_thumbnails.get(path)
                                } else if self.attachment_thumbnail_errors.contains_key(path) {
                                    touched_errors.push(path.to_string());
                                    None
                                } else if self.thumbnail_in_flight.contains(path) {
                                    None
                                } else {
                                    thumbnail_requests.push(path.to_string());
                                    None
                                };
                                if let Some(texture) = thumbnail {
                                    let sized =
                                        egui::load::SizedTexture::from_handle(texture);
                                    ui.add(
                                        egui::Image::from_texture(sized)
                                            .max_size(egui::Vec2::new(220.0, 160.0)),
                                    );
                                } else if self.thumbnail_in_flight.contains(path)
                                    || thumbnail_requests
                                        .iter()
                                        .any(|queued| queued == path)
                                {
                                    ui.label(
                                        egui::RichText::new("Loading image preview...")
                                            .small()
                                            .color(egui::Color32::from_rgb(130, 140, 160)),
                                    );
                                } else if let Some(err) =
                                    self.attachment_thumbnail_errors.get(path)
                                {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Image preview unavailable: {err}"
                                        ))
                                        .small()
                                        .color(egui::Color32::from_rgb(170, 140, 140)),
                                    );
                                }
                            }
                            ui.horizontal(|row| {
                                row.label(
                                    egui::RichText::new("[attachment]")
                                        .small()
                                        .color(egui::Color32::from_rgb(120, 130, 150)),
                                );
                                row.label(
                                    egui::RichText::new(&attachment.file_name)
                                        .small()
                                        .color(egui::Color32::from_rgb(190, 200, 215)),
                                )
                                .on_hover_text(&attachment.file_path);
                                row.label(
                                    egui::RichText::new(format!(
                                        "{} • {}",
                                        attachment.kind,
                                        format_bytes(attachment.file_size)
                                    ))
                                    .small()
                                    .color(egui::Color32::from_rgb(120, 130, 150)),
                                );
                                if row.button("Open").clicked() {
                                    match open_attachment(&attachment.file_path) {
                                        Ok(()) => self.attachment_action_error = None,
                                        Err(err) => self.attachment_action_error = Some(err),
                                    }
                                }
                                if row.button("Reveal").clicked() {
                                    match reveal_attachment(&attachment.file_path) {
                                        Ok(()) => self.attachment_action_error = None,
                                        Err(err) => self.attachment_action_error = Some(err),
                                    }
                                }
                            });
                        }
                    }
                    ui.add_space(2.0);
                }
                if !thumbnail_requests.is_empty() {
                    for path in thumbnail_requests {
                        self.queue_thumbnail_load(&path);
                    }
                }
                for path in touched_thumbnails {
                    self.touch_thumbnail_cache(&path);
                }
                for path in touched_errors {
                    self.touch_thumbnail_error(&path);
                }
                if let Some(error) = &self.attachment_action_error {
                    ui.label(
                        egui::RichText::new(error)
                            .small()
                            .color(egui::Color32::from_rgb(220, 120, 120)),
                    );
                }
                ui.separator();
                ui.add_enabled_ui(self.messages_loaded, |ui| {
                    let (composer_placeholder, typing_stub) = self
                        .composer_meta
                        .get(&self.selected_channel_id)
                        .map(|meta| (meta.placeholder.as_str(), meta.typing_stub.as_str()))
                        .unwrap_or(("Send a message", "Typing..."));
                    let draft = self
                        .composer_drafts
                        .entry(self.selected_channel_id)
                        .or_default();
                    let typing_active =
                        match self.typing_state.get(&self.selected_channel_id).copied() {
                            Some(last_edit) if last_edit.elapsed() < Duration::from_secs(3) => true,
                            Some(_) => {
                                self.typing_state.remove(&self.selected_channel_id);
                                false
                            }
                            None => false,
                        };
                    let typing_label = if typing_active && !draft.trim().is_empty() {
                        "You are typing..."
                    } else {
                        typing_stub
                    };
                    ui.label(
                        egui::RichText::new(typing_label)
                            .small()
                            .color(egui::Color32::from_rgb(140, 150, 170)),
                    );
                    let attachment_path = self
                        .attachment_path_drafts
                        .entry(self.selected_channel_id)
                        .or_default();
                    let pending_list = self
                        .pending_attachments
                        .entry(self.selected_channel_id)
                        .or_default();
                    ui.horizontal(|row| {
                        row.label("Attach");
                        row.add(
                            egui::TextEdit::singleline(attachment_path)
                                .hint_text("Path to file")
                                .desired_width(320.0),
                        );
                        if row.button("Add").clicked() {
                            let trimmed = attachment_path.trim();
                            if trimmed.is_empty() {
                                self.attachment_error =
                                    Some("Attachment path is empty.".to_string());
                            } else {
                                match ingest_attachment(trimmed) {
                                    Ok(attachment) => {
                                        pending_list.push(attachment);
                                        attachment_path.clear();
                                        self.attachment_error = None;
                                    }
                                    Err(err) => {
                                        self.attachment_error = Some(err);
                                    }
                                }
                            }
                        }
                    });
                    let mut remove_attachment: Option<usize> = None;
                    for (idx, attachment) in pending_list.iter().enumerate() {
                        ui.horizontal(|row| {
                            row.label(
                                egui::RichText::new(format!(
                                    "{} ({}, {})",
                                    attachment.file_name,
                                    format_bytes(attachment.file_size),
                                    attachment.kind
                                ))
                                .small()
                                .color(egui::Color32::from_rgb(160, 170, 190)),
                            );
                            if row.button("Remove").clicked() {
                                remove_attachment = Some(idx);
                            }
                        });
                    }
                    if let Some(idx) = remove_attachment {
                        if idx < pending_list.len() {
                            pending_list.remove(idx);
                        }
                    }
                    if let Some(error) = &self.attachment_error {
                        ui.label(
                            egui::RichText::new(error)
                                .small()
                                .color(egui::Color32::from_rgb(220, 120, 120)),
                        );
                    }
                    ui.horizontal(|row| {
                        let composer = row.add(
                            egui::TextEdit::singleline(draft)
                                .hint_text(composer_placeholder)
                                .desired_width(f32::INFINITY),
                        );
                        if self.composer_focus_requested {
                            composer.request_focus();
                            self.composer_focus_requested = false;
                        }
                        let send_clicked = row.button("Send").clicked();
                        let send_enter = composer.has_focus()
                            && row.input(|input| input.key_pressed(egui::Key::Enter));
                        let send_now = send_clicked || send_enter;
                        if send_clicked {
                            self.composer_focus_requested = true;
                        }
                        if composer.changed() {
                            if draft.trim().is_empty() {
                                self.typing_state.remove(&self.selected_channel_id);
                            } else {
                                self.typing_state
                                    .insert(self.selected_channel_id, Instant::now());
                            }
                        }
                        if send_now {
                            let body = draft.trim().to_string();
                            if !body.is_empty() || !pending_list.is_empty() {
                                pending_send = Some(body);
                                pending_attachments_send = pending_list.clone();
                                pending_list.clear();
                                draft.clear();
                                self.typing_state.remove(&self.selected_channel_id);
                                self.composer_focus_requested = true;
                            }
                        }
                    });
                });
                if !self.messages_loaded {
                    ui.label(
                        egui::RichText::new("Composer available once messages finish loading.")
                            .small()
                            .color(egui::Color32::from_rgb(140, 150, 170)),
                    );
                }
            });
        });
        if realtime_connect {
            self.realtime.connect();
        }
        if realtime_disconnect {
            self.realtime.disconnect();
        }

        self.egui_state
            .handle_platform_output(self.window.as_ref(), full_output.platform_output);

        for (id, delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: full_output.pixels_per_point,
        };

        let clipped_primitives = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let mut encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("ralph-encoder"),
                });
        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &clipped_primitives,
            &screen_descriptor,
        );

        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(SurfaceError::Lost) => {
                self.resize(PhysicalSize::new(self.config.width, self.config.height));
                return;
            }
            Err(SurfaceError::OutOfMemory) => {
                return;
            }
            Err(err) => {
                eprintln!("surface error: {err}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ralph-render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.06,
                            g: 0.07,
                            b: 0.09,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.egui_renderer.render(
                &mut render_pass,
                &clipped_primitives,
                &screen_descriptor,
            );
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();

        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        if let Some(channel_id) = channel_switch {
            if self.messages_loaded && channel_id != self.selected_channel_id {
                self.selected_channel_id = channel_id;
                self.messages = match load_messages(&self.db, channel_id) {
                    Ok(messages) => messages,
                    Err(err) => {
                        eprintln!("db load error: {err}");
                        Vec::new()
                    }
                };
                self.messages_loaded = true;
                self.message_attachments = match load_attachments_for_message_ids(
                    &self.db,
                    &self.messages.iter().map(|message| message.id).collect::<Vec<_>>(),
                ) {
                    Ok(attachments) => attachments,
                    Err(err) => {
                        eprintln!("db attachments load error: {err}");
                        HashMap::new()
                    }
                };
                self.composer_focus_requested = true;
                if self.search_channel_only && !self.search_query.trim().is_empty() {
                    let query = self.search_query.trim().to_string();
                    match search_messages(&self.db, &query, Some(channel_id)) {
                        Ok(results) => {
                            self.search_last_query = query;
                            self.search_last_channel_only = true;
                            self.search_results = results;
                            self.message_attachments = match load_attachments_for_message_ids(
                                &self.db,
                                &self
                                    .search_results
                                    .iter()
                                    .map(|message| message.id)
                                    .collect::<Vec<_>>(),
                            ) {
                                Ok(attachments) => attachments,
                                Err(err) => {
                                    eprintln!("db attachments load error: {err}");
                                    HashMap::new()
                                }
                            };
                        }
                        Err(err) => {
                            eprintln!("db search error: {err}");
                            self.search_last_query.clear();
                            self.search_last_channel_only = self.search_channel_only;
                            self.search_results.clear();
                        }
                    }
                }
            }
        }

        if search_clear {
            self.search_query.clear();
            self.search_last_query.clear();
            self.search_last_channel_only = self.search_channel_only;
            self.search_results.clear();
            if self.messages_loaded {
                self.message_attachments = match load_attachments_for_message_ids(
                    &self.db,
                    &self.messages.iter().map(|message| message.id).collect::<Vec<_>>(),
                ) {
                    Ok(attachments) => attachments,
                    Err(err) => {
                        eprintln!("db attachments load error: {err}");
                        HashMap::new()
                    }
                };
            }
        }

        if let Some(request) = search_request {
            if self.messages_loaded {
                let query = request.query;
                let channel_filter = if request.channel_only {
                    Some(self.selected_channel_id)
                } else {
                    None
                };
                match search_messages(&self.db, &query, channel_filter) {
                    Ok(results) => {
                        self.search_last_query = query;
                        self.search_last_channel_only = request.channel_only;
                        self.search_results = results;
                        self.message_attachments = match load_attachments_for_message_ids(
                            &self.db,
                            &self
                                .search_results
                                .iter()
                                .map(|message| message.id)
                                .collect::<Vec<_>>(),
                        ) {
                            Ok(attachments) => attachments,
                            Err(err) => {
                                eprintln!("db attachments load error: {err}");
                                HashMap::new()
                            }
                        };
                    }
                    Err(err) => {
                        eprintln!("db search error: {err}");
                        self.search_last_query.clear();
                        self.search_last_channel_only = request.channel_only;
                        self.search_results.clear();
                    }
                }
            }
        }

        if let Some(body) = pending_send {
            if self.messages_loaded {
                let content = if body.is_empty() && !pending_attachments_send.is_empty() {
                    "Attachment".to_string()
                } else {
                    body
                };
                let mut message = Message {
                    id: 0,
                    author: "you".to_string(),
                    body: content,
                    sent_at: format_timestamp_utc(),
                    channel_id: self.selected_channel_id,
                };
                match insert_message(&self.db, &message) {
                    Ok(id) => {
                        message.id = id;
                        let outgoing_attachments =
                            pending_to_realtime_attachments(&pending_attachments_send);
                        if !pending_attachments_send.is_empty() {
                            if let Err(err) = insert_attachments(
                                &mut self.db,
                                message.id,
                                &pending_attachments_send,
                            ) {
                                eprintln!("db attachments insert error: {err}");
                            }
                            self.message_attachments
                                .entry(message.id)
                                .or_default()
                                .extend(pending_attachments_send.into_iter().map(|pending| {
                                    Attachment {
                                        message_id: message.id,
                                        file_path: pending.file_path,
                                        file_name: pending.file_name,
                                        file_size: pending.file_size,
                                        kind: pending.kind,
                                    }
                                }));
                        }
                        self.track_member(&message);
                        self.messages.push(message);
                        self.realtime.send_message(
                            self.messages.last().expect("message"),
                            outgoing_attachments,
                        );
                    }
                    Err(err) => {
                        eprintln!("db insert error: {err}");
                    }
                }
            }
        }

        if !incoming.is_empty() {
            for incoming_message in incoming {
                if self.messages_loaded {
                    let mut inbound = incoming_message.message;
                    let inbound_attachments = incoming_message.attachments;
                    match insert_message(&self.db, &inbound) {
                        Ok(id) => {
                            inbound.id = id;
                            if !inbound_attachments.is_empty() {
                                let pending = realtime_to_pending_attachments(&inbound_attachments);
                                if let Err(err) =
                                    insert_attachments(&mut self.db, inbound.id, &pending)
                                {
                                    eprintln!("db attachments insert error: {err}");
                                }
                                self.message_attachments
                                    .entry(inbound.id)
                                    .or_default()
                                    .extend(pending.into_iter().map(|pending| Attachment {
                                        message_id: inbound.id,
                                        file_path: pending.file_path,
                                        file_name: pending.file_name,
                                        file_size: pending.file_size,
                                        kind: pending.kind,
                                    }));
                            }
                        }
                        Err(err) => {
                            eprintln!("db insert error: {err}");
                        }
                    }
                    self.track_member(&inbound);
                    if inbound.channel_id == self.selected_channel_id {
                        self.messages.push(inbound);
                    }
                }
            }
        }
    }
}

impl App {
    fn drain_thumbnail_results(&mut self) {
        while let Ok(result) = self.thumbnail_receiver.try_recv() {
            self.thumbnail_in_flight.remove(&result.path);
            if let Some(error) = result.error {
                self.attachment_thumbnail_errors
                    .insert(result.path.clone(), error);
                self.touch_thumbnail_error(&result.path);
                self.enforce_thumbnail_cache_limits();
                continue;
            }
            if let Some(image) = result.image {
                let texture = self.egui_ctx.load_texture(
                    format!("attachment:{}", result.path),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.attachment_thumbnails
                    .insert(result.path.clone(), texture);
                self.touch_thumbnail_cache(&result.path);
                self.enforce_thumbnail_cache_limits();
            }
        }
    }

    fn touch_thumbnail_cache(&mut self, path: &str) {
        Self::touch_cache_order(&mut self.thumbnail_cache_order, path);
    }

    fn touch_thumbnail_error(&mut self, path: &str) {
        Self::touch_cache_order(&mut self.thumbnail_error_order, path);
    }

    fn touch_cache_order(order: &mut VecDeque<String>, path: &str) {
        if let Some(pos) = order.iter().position(|entry| entry == path) {
            order.remove(pos);
        }
        order.push_back(path.to_string());
    }

    fn enforce_thumbnail_cache_limits(&mut self) {
        while self.thumbnail_cache_order.len() > THUMBNAIL_CACHE_LIMIT {
            if let Some(evicted) = self.thumbnail_cache_order.pop_front() {
                self.attachment_thumbnails.remove(&evicted);
            }
        }
        while self.thumbnail_error_order.len() > THUMBNAIL_ERROR_LIMIT {
            if let Some(evicted) = self.thumbnail_error_order.pop_front() {
                self.attachment_thumbnail_errors.remove(&evicted);
            }
        }
    }

    fn apply_deferred_loads(&mut self) {
        let result = match self.deferred_load_receiver.as_ref() {
            Some(receiver) => receiver.try_recv().ok(),
            None => None,
        };
        if let Some(result) = result {
            let selected_before = self.selected_channel_id;
            if !result.channels.is_empty() {
                self.channels = result.channels;
                self.composer_meta = build_composer_meta(&self.channels);
                if !self
                    .channels
                    .iter()
                    .any(|channel| channel.id == self.selected_channel_id)
                {
                    if let Some(channel) = self.channels.first() {
                        self.selected_channel_id = channel.id;
                        self.composer_focus_requested = true;
                    }
                }
            }
            if result.channel_id == self.selected_channel_id {
                self.messages = result.messages;
                self.message_attachments = result.attachments;
                self.messages_loaded = true;
            } else if self.selected_channel_id != selected_before {
                self.messages = match load_messages(&self.db, self.selected_channel_id) {
                    Ok(messages) => messages,
                    Err(err) => {
                        eprintln!("db load error: {err}");
                        Vec::new()
                    }
                };
                self.messages_loaded = true;
                self.message_attachments = match load_attachments_for_message_ids(
                    &self.db,
                    &self.messages.iter().map(|message| message.id).collect::<Vec<_>>(),
                ) {
                    Ok(attachments) => attachments,
                    Err(err) => {
                        eprintln!("db attachments load error: {err}");
                        HashMap::new()
                    }
                };
            }
            for (channel_id, members) in result.channel_members {
                self.channel_members
                    .entry(channel_id)
                    .or_default()
                    .extend(members);
            }
            if !result.db_ready {
                if let Err(err) = ensure_schema(&self.db) {
                    eprintln!("db schema error: {err}");
                }
            }
            self.deferred_load_receiver = None;
        }
    }

    fn queue_thumbnail_load(&mut self, path: &str) {
        if !self.thumbnail_in_flight.insert(path.to_string()) {
            return;
        }
        let sender = self.thumbnail_sender.clone();
        let path = path.to_string();
        thread::spawn(move || {
            let result = match load_attachment_thumbnail_image(&path) {
                Ok(image) => ThumbnailResult {
                    path,
                    image: Some(image),
                    error: None,
                },
                Err(error) => ThumbnailResult {
                    path,
                    image: None,
                    error: Some(error),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn channel_label(&self, channel_id: i64) -> String {
        self.channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .map(|channel| match channel.kind {
                ChannelKind::Channel => format!("#{}", channel.name),
                ChannelKind::DirectMessage => format!("@{}", channel.name),
            })
            .unwrap_or_else(|| format!("#{}", channel_id))
    }

    fn track_member(&mut self, message: &Message) {
        self.channel_members
            .entry(message.channel_id)
            .or_default()
            .insert(message.author.clone());
    }

    fn presence_for_user(&self, user: &str) -> PresenceStatus {
        self.presence_state
            .get(user)
            .map(|state| state.status)
            .unwrap_or(PresenceStatus::Unknown)
    }

    fn channel_presence_counts(&self, channel_id: i64) -> (usize, usize) {
        let members = match self.channel_members.get(&channel_id) {
            Some(members) => members,
            None => return (0, 0),
        };
        let total = members.len();
        let online = members
            .iter()
            .filter(|member| self.presence_for_user(member) == PresenceStatus::Online)
            .count();
        (online, total)
    }

    fn channel_presence_details(&self) -> Option<String> {
        let channel = self
            .channels
            .iter()
            .find(|channel| channel.id == self.selected_channel_id)?;
        match channel.kind {
            ChannelKind::DirectMessage => {
                let state = self.presence_state.get(&channel.name);
                let status = state
                    .map(|state| state.status)
                    .unwrap_or(PresenceStatus::Unknown);
                let detail = if let Some(state) = state {
                    format!(
                        "@{} is {} (updated {}s ago)",
                        channel.name,
                        status.label(),
                        state.last_seen.elapsed().as_secs()
                    )
                } else {
                    format!("@{} status: {}", channel.name, status.label())
                };
                Some(detail)
            }
            ChannelKind::Channel => {
                let (online, total) = self.channel_presence_counts(channel.id);
                if total == 0 {
                    Some("No member activity yet.".to_string())
                } else {
                    Some(format!("{online}/{total} members online"))
                }
            }
        }
    }
}

fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn search_messages(
    conn: &Connection,
    query: &str,
    channel_id: Option<i64>,
) -> Result<Vec<Message>, rusqlite::Error> {
    let escaped = escape_like(query);
    let pattern = format!("%{}%", escaped);
    let mut messages = Vec::new();
    if let Some(channel_id) = channel_id {
        let mut stmt = conn.prepare(
            "SELECT id, author, body, sent_at, channel_id
            FROM messages
            WHERE channel_id = ?1
              AND (author LIKE ?2 ESCAPE '\\' OR body LIKE ?2 ESCAPE '\\')
            ORDER BY id DESC
            LIMIT 200",
        )?;
        let rows = stmt.query_map(params![channel_id, pattern], |row| {
            Ok(Message {
                id: row.get(0)?,
                author: row.get(1)?,
                body: row.get(2)?,
                sent_at: row.get(3)?,
                channel_id: row.get(4)?,
            })
        })?;
        for message in rows {
            messages.push(message?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, author, body, sent_at, channel_id
            FROM messages
            WHERE author LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\'
            ORDER BY id DESC
            LIMIT 200",
        )?;
        let rows = stmt.query_map([pattern], |row| {
            Ok(Message {
                id: row.get(0)?,
                author: row.get(1)?,
                body: row.get(2)?,
                sent_at: row.get(3)?,
                channel_id: row.get(4)?,
            })
        })?;
        for message in rows {
            messages.push(message?);
        }
    }
    Ok(messages)
}

fn insert_attachments(
    conn: &mut Connection,
    message_id: i64,
    attachments: &[PendingAttachment],
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    for attachment in attachments {
        tx.execute(
            "INSERT INTO attachments (message_id, file_path, file_name, file_size, kind)
            VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                message_id,
                attachment.file_path,
                attachment.file_name,
                attachment.file_size,
                attachment.kind
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn pending_to_realtime_attachments(
    attachments: &[PendingAttachment],
) -> Vec<RealtimeAttachment> {
    attachments
        .iter()
        .map(|attachment| RealtimeAttachment {
            file_path: attachment.file_path.clone(),
            file_name: attachment.file_name.clone(),
            file_size: attachment.file_size,
            kind: attachment.kind.clone(),
        })
        .collect()
}

fn realtime_to_pending_attachments(
    attachments: &[RealtimeAttachment],
) -> Vec<PendingAttachment> {
    attachments
        .iter()
        .map(|attachment| {
            let file_name = if attachment.file_name.is_empty() {
                file_name_from_path(&attachment.file_path)
            } else {
                attachment.file_name.clone()
            };
            PendingAttachment {
                file_path: attachment.file_path.clone(),
                file_name,
                file_size: attachment.file_size,
                kind: attachment.kind.clone(),
            }
        })
        .collect()
}

fn load_attachments_for_message_ids(
    conn: &Connection,
    message_ids: &[i64],
) -> Result<HashMap<i64, Vec<Attachment>>, rusqlite::Error> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = std::iter::repeat("?")
        .take(message_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT message_id, file_path, file_name, file_size, kind
        FROM attachments
        WHERE message_id IN ({placeholders})
        ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map(params_from_iter(message_ids.iter().copied()), |row| {
        Ok(Attachment {
            message_id: row.get(0)?,
            file_path: row.get(1)?,
            file_name: row.get(2)?,
            file_size: row.get(3)?,
            kind: row.get(4)?,
        })
    })?;
    let mut map: HashMap<i64, Vec<Attachment>> = HashMap::new();
    for attachment in rows {
        let attachment = attachment?;
        map.entry(attachment.message_id)
            .or_default()
            .push(attachment);
    }
    Ok(map)
}

fn load_attachment_thumbnail_image(path: &str) -> Result<egui::ColorImage, String> {
    let reader = ImageReader::open(path)
        .map_err(|err| format!("file open: {err}"))?
        .with_guessed_format()
        .map_err(|err| format!("format error: {err}"))?;
    let mut image = reader.decode().map_err(|err| format!("decode error: {err}"))?;
    let max_dimension = 240u32;
    let (width, height) = image.dimensions();
    let max_axis = width.max(height);
    if max_axis > max_dimension {
        let scale = max_dimension as f32 / max_axis as f32;
        let new_width = (width as f32 * scale).round().max(1.0) as u32;
        let new_height = (height as f32 * scale).round().max(1.0) as u32;
        image = image.resize(new_width, new_height, FilterType::Triangle);
    }
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.into_raw();
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, &pixels))
}

fn open_attachment(path: &str) -> Result<(), String> {
    open_attachment_with_args(path, &[])
}

fn reveal_attachment(path: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        open_attachment_with_args(path, &["-R"])
    }
    #[cfg(not(target_os = "macos"))]
    {
        let parent = Path::new(path)
            .parent()
            .ok_or_else(|| "Attachment path has no parent directory.".to_string())?;
        open_attachment_with_args(parent.to_str().unwrap_or_default(), &[])
    }
}

fn open_attachment_with_args(path: &str, extra_args: &[&str]) -> Result<(), String> {
    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err("Attachment path does not exist.".to_string());
    }
    let mut command = if cfg!(target_os = "macos") {
        let mut cmd = Command::new("open");
        cmd.args(extra_args);
        cmd
    } else if cfg!(target_os = "windows") {
        Command::new("explorer")
    } else {
        Command::new("xdg-open")
    };
    if cfg!(target_os = "windows") && extra_args.iter().any(|arg| *arg == "-R") {
        command.arg(format!("/select,{}", path_ref.display()));
    } else {
        command.arg(path_ref);
    }
    command
        .status()
        .map_err(|err| format!("Failed to launch attachment: {err}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err("Attachment launcher exited with failure.".to_string())
            }
        })
}

fn ingest_attachment(path: &str) -> Result<PendingAttachment, String> {
    let metadata = fs::metadata(path).map_err(|err| format!("File error: {err}"))?;
    let file_name = file_name_from_path(path);
    let file_size = metadata.len() as i64;
    let kind = detect_attachment_kind(path).to_string();
    Ok(PendingAttachment {
        file_path: path.to_string(),
        file_name,
        file_size,
        kind,
    })
}

fn file_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn detect_attachment_kind(path: &str) -> &'static str {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "image",
        "pdf" | "txt" | "md" | "doc" | "docx" | "rtf" => "document",
        _ => "file",
    }
}

fn format_bytes(size: i64) -> String {
    let size = size as f64;
    let units = ["B", "KB", "MB", "GB"];
    let mut value = size;
    let mut idx = 0;
    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    format!("{value:.1}{}", units[idx])
}

fn main() {
    let boot_started = Instant::now();
    println!("ralph: booting");
    let exit_after_first_frame = env::var("RALPH_STARTUP_BENCH").is_ok();

    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::new(&event_loop, boot_started, exit_after_first_frame);

    let _ = event_loop.run(move |event, elwt| match event {
        Event::WindowEvent { event, window_id } if window_id == app.window.id() => {
            match event {
                WindowEvent::RedrawRequested => app.render(),
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::Resized(size) => app.resize(size),
                WindowEvent::ScaleFactorChanged {
                    mut inner_size_writer,
                    ..
                } => {
                    let size = app.window.inner_size();
                    let _ = inner_size_writer.request_inner_size(size);
                    app.resize(size);
                }
                _ => {
                    let _ = app
                        .egui_state
                        .on_window_event(app.window.as_ref(), &event)
                        .consumed;
                }
            }
        }
        Event::AboutToWait => {
            if app.exit_requested {
                elwt.exit();
            } else {
                app.window.request_redraw();
            }
        }
        _ => {}
    });
}
