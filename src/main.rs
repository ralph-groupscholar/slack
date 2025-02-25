use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use egui_wgpu::{Renderer, ScreenDescriptor};
use egui_winit::State as EguiWinitState;
use rusqlite::{params, Connection};
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
    },
}

struct RealtimeEvent {
    status: RealtimeStatus,
    message: Option<String>,
    error: Option<String>,
    inbound: Option<Message>,
    presence: Option<PresenceUpdate>,
}

struct RealtimeClient {
    status: RealtimeStatus,
    last_message: Option<String>,
    last_error: Option<String>,
    target_url: String,
    cmd_tx: mpsc::Sender<RealtimeCommand>,
    evt_rx: mpsc::Receiver<RealtimeEvent>,
    incoming: Vec<Message>,
    incoming_presence: Vec<PresenceUpdate>,
}

#[derive(Clone)]
struct PresenceUpdate {
    user: String,
    status: String,
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
    fn from_message(message: &Message) -> Self {
        Self::Message {
            author: message.author.clone(),
            body: message.body.clone(),
            sent_at: message.sent_at.clone(),
            channel_id: message.channel_id,
            client_id: None,
        }
    }

    fn into_message(self) -> Option<Message> {
        match self {
            RealtimePayload::Message {
                author,
                body,
                sent_at,
                channel_id,
                client_id: _,
            } => Some(Message {
                author,
                body,
                sent_at,
                channel_id,
            }),
            _ => None,
        }
    }
}

fn encode_realtime_message(message: &Message) -> Result<String, serde_json::Error> {
    serde_json::to_string(&RealtimePayload::from_message(message))
}

fn parse_legacy_message(text: &str) -> Option<Message> {
    let mut parts = text.splitn(4, '\t');
    let author = parts.next()?;
    let channel_id = parts.next()?.parse::<i64>().ok()?;
    let sent_at = parts.next()?;
    let body = parts.next().unwrap_or(text);
    Some(Message {
        author: author.to_string(),
        body: body.to_string(),
        sent_at: sent_at.to_string(),
        channel_id,
    })
}

enum RealtimeInbound {
    Message(Message),
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
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        spawn_realtime_worker(cmd_rx, evt_tx, target_url.clone());
        Self {
            status: RealtimeStatus::Disconnected,
            last_message: None,
            last_error: None,
            target_url,
            cmd_tx,
            evt_rx,
            incoming: Vec::new(),
            incoming_presence: Vec::new(),
        }
    }

    fn connect(&self) {
        let _ = self.cmd_tx.send(RealtimeCommand::Connect);
    }

    fn disconnect(&self) {
        let _ = self.cmd_tx.send(RealtimeCommand::Disconnect);
    }

    fn send_message(&self, message: &Message) {
        let _ = self.cmd_tx.send(RealtimeCommand::SendMessage {
            author: message.author.clone(),
            body: message.body.clone(),
            sent_at: message.sent_at.clone(),
            channel_id: message.channel_id,
        });
    }

    fn poll(&mut self) {
        while let Ok(event) = self.evt_rx.try_recv() {
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

    fn take_incoming(&mut self) -> Vec<Message> {
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
                    } => {
                        if let Some(ws) = socket.as_mut() {
                            let message = Message {
                                author,
                                body,
                                sent_at,
                                channel_id,
                            };
                            match encode_realtime_message(&message) {
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
            author: "mara".to_string(),
            body: "Shipping the new hotkey flow now.".to_string(),
            sent_at: "09:12".to_string(),
            channel_id: 1,
        },
        Message {
            author: "devin".to_string(),
            body: "Latency on local echo is <100ms.".to_string(),
            sent_at: "09:13".to_string(),
            channel_id: 1,
        },
        Message {
            author: "sasha".to_string(),
            body: "Message search index warmed on startup.".to_string(),
            sent_at: "09:15".to_string(),
            channel_id: 1,
        },
        Message {
            author: "you".to_string(),
            body: "Feels fast. Let's keep it lean.".to_string(),
            sent_at: "09:18".to_string(),
            channel_id: 1,
        },
        Message {
            author: "mara".to_string(),
            body: "Next: attachments + previews.".to_string(),
            sent_at: "09:21".to_string(),
            channel_id: 2,
        },
        Message {
            author: "devin".to_string(),
            body: "Profiling idle CPU now.".to_string(),
            sent_at: "09:24".to_string(),
            channel_id: 2,
        },
        Message {
            author: "mara".to_string(),
            body: "Can you sanity-check the build flags?".to_string(),
            sent_at: "09:26".to_string(),
            channel_id: 3,
        },
        Message {
            author: "devin".to_string(),
            body: "Want me to share flamegraph results?".to_string(),
            sent_at: "09:28".to_string(),
            channel_id: 4,
        },
    ]
}

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
        "SELECT author, body, sent_at, channel_id FROM messages WHERE channel_id = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([channel_id], |row| {
        Ok(Message {
            author: row.get(0)?,
            body: row.get(1)?,
            sent_at: row.get(2)?,
            channel_id: row.get(3)?,
        })
    })?;

    let mut messages = Vec::new();
    for message in rows {
        messages.push(message?);
    }
    Ok(messages)
}

fn insert_message(conn: &Connection, message: &Message) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO messages (author, body, sent_at, channel_id) VALUES (?1, ?2, ?3, ?4)",
        params![message.author, message.body, message.sent_at, message.channel_id],
    )?;
    Ok(())
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

struct App {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_state: EguiWinitState,
    egui_ctx: egui::Context,
    egui_renderer: Renderer,
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
}

impl App {
    fn new(event_loop: &EventLoop<()>) -> Self {
        let window = Arc::new(
            WindowBuilder::new()
                .with_title("Ralph")
                .with_inner_size(PhysicalSize::new(1100, 720))
                .build(event_loop)
                .expect("window"),
        );

        let instance = wgpu::Instance::default();
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

        let mut db = match Connection::open("ralph.db") {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("db open error: {err}");
                Connection::open_in_memory().expect("memory db")
            }
        };
        if let Err(err) = ensure_schema(&db) {
            eprintln!("db schema error: {err}");
        }
        if let Err(err) = seed_channels_if_empty(&mut db) {
            eprintln!("db seed channels error: {err}");
        }
        if let Err(err) = seed_messages_if_empty(&mut db) {
            eprintln!("db seed error: {err}");
        }
        let channels = match load_channels(&db) {
            Ok(channels) => channels,
            Err(err) => {
                eprintln!("db channels load error: {err}");
                seed_channels()
                    .into_iter()
                    .map(|(id, name, kind)| Channel {
                        id,
                        name: name.to_string(),
                        kind,
                    })
                    .collect()
            }
        };
        let selected_channel_id = channels.first().map(|channel| channel.id).unwrap_or(1);
        let composer_meta = build_composer_meta(&channels);
        let messages = match load_messages(&db, selected_channel_id) {
            Ok(messages) => messages,
            Err(err) => {
                eprintln!("db load error: {err}");
                seed_messages()
                    .into_iter()
                    .filter(|message| message.channel_id == selected_channel_id)
                    .collect()
            }
        };
        let channel_members = match load_channel_members(&db, &channels) {
            Ok(members) => members,
            Err(err) => {
                eprintln!("db members load error: {err}");
                HashMap::new()
            }
        };
        let mut presence_state = HashMap::new();
        presence_state.insert(
            "you".to_string(),
            PresenceState {
                status: PresenceStatus::Online,
                last_seen: Instant::now(),
            },
        );

        Self {
            window,
            surface,
            device,
            queue,
            config,
            egui_state,
            egui_ctx,
            egui_renderer,
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
            channel_members,
            presence_state,
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
        let raw_input = self.egui_state.take_egui_input(self.window.as_ref());
        let mut pending_send: Option<String> = None;
        let mut channel_switch: Option<i64> = None;
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
                                self.realtime.connect();
                            }
                        }
                        RealtimeStatus::Connecting => {
                            row.add_enabled(false, egui::Button::new("Connecting..."));
                        }
                        RealtimeStatus::Connected => {
                            if row.button("Disconnect").clicked() {
                                self.realtime.disconnect();
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
                for message in &self.messages {
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
                        row.label(&message.body);
                    });
                    ui.add_space(2.0);
                }
                ui.separator();
                let (composer_placeholder, typing_stub) = self
                    .composer_meta
                    .get(&self.selected_channel_id)
                    .map(|meta| (meta.placeholder.as_str(), meta.typing_stub.as_str()))
                    .unwrap_or(("Send a message", "Typing..."));
                let draft = self
                    .composer_drafts
                    .entry(self.selected_channel_id)
                    .or_default();
                let typing_active = match self.typing_state.get(&self.selected_channel_id).copied() {
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
                        if !body.is_empty() {
                            pending_send = Some(body);
                            draft.clear();
                            self.typing_state.remove(&self.selected_channel_id);
                            self.composer_focus_requested = true;
                        }
                    }
                });
            });
        });

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
            if channel_id != self.selected_channel_id {
                self.selected_channel_id = channel_id;
                self.messages = match load_messages(&self.db, channel_id) {
                    Ok(messages) => messages,
                    Err(err) => {
                        eprintln!("db load error: {err}");
                        Vec::new()
                    }
                };
                self.composer_focus_requested = true;
            }
        }

        if let Some(body) = pending_send {
            let message = Message {
                author: "you".to_string(),
                body,
                sent_at: format_timestamp_utc(),
                channel_id: self.selected_channel_id,
            };
            if let Err(err) = insert_message(&self.db, &message) {
                eprintln!("db insert error: {err}");
            }
            self.track_member(&message);
            self.messages.push(message);
            self.realtime.send_message(self.messages.last().expect("message"));
        }

        if !incoming.is_empty() {
            for message in incoming {
                if let Err(err) = insert_message(&self.db, &message) {
                    eprintln!("db insert error: {err}");
                }
                self.track_member(&message);
                if message.channel_id == self.selected_channel_id {
                    self.messages.push(message);
                }
            }
        }
    }
}

impl App {
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

fn main() {
    println!("ralph: booting");

    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::new(&event_loop);

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
            app.window.request_redraw();
        }
        _ => {}
    });
}
