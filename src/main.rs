use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use egui_wgpu::{Renderer, ScreenDescriptor};
use egui_winit::State as EguiWinitState;
use rusqlite::{params, Connection};
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
        let raw_input = self.egui_state.take_egui_input(self.window.as_ref());
        let mut pending_send: Option<String> = None;
        let mut channel_switch: Option<i64> = None;
        let full_output = self.egui_ctx.run(raw_input, |ctx| {
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
                        let label = format!("# {}", channel.name);
                        if ui
                            .selectable_label(self.selected_channel_id == channel.id, label)
                            .clicked()
                        {
                            channel_switch = Some(channel.id);
                        }
                    }
                    ui.add_space(8.0);
                    ui.label("Direct Messages");
                    for channel in self
                        .channels
                        .iter()
                        .filter(|channel| channel.kind == ChannelKind::DirectMessage)
                    {
                        let label = format!("@{}", channel.name);
                        if ui
                            .selectable_label(self.selected_channel_id == channel.id, label)
                            .clicked()
                        {
                            channel_switch = Some(channel.id);
                        }
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
            self.messages.push(message);
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
