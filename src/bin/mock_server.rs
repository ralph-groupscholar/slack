use std::{
    net::TcpListener,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tungstenite::{accept, Message as WsMessage};

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

fn format_timestamp_utc() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let hours = (now / 3600) % 24;
    let minutes = (now / 60) % 60;
    format!("{:02}:{:02}", hours, minutes)
}

fn broadcast_text(
    subscribers: &Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    text: &str,
) {
    if let Ok(mut list) = subscribers.lock() {
        let mut to_remove = Vec::new();
        for (idx, sender) in list.iter().enumerate() {
            if sender.send(text.to_string()).is_err() {
                to_remove.push(idx);
            }
        }
        for idx in to_remove.into_iter().rev() {
            list.remove(idx);
        }
    }
}

fn send_payload(socket: &mut tungstenite::WebSocket<std::net::TcpStream>, payload: &RealtimePayload) {
    if let Ok(text) = serde_json::to_string(payload) {
        let _ = socket.send(WsMessage::Text(text));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:9001")?;
    println!("mock server listening on ws://127.0.0.1:9001");

    let subscribers: Arc<Mutex<Vec<mpsc::Sender<String>>>> = Arc::new(Mutex::new(Vec::new()));

    for stream in listener.incoming() {
        let stream = stream?;
        let mut socket = accept(stream)?;
        let _ = socket.get_mut().set_nonblocking(true);

        let (tx, rx) = mpsc::channel::<String>();
        subscribers.lock().expect("subscribers").push(tx);
        let subscribers = Arc::clone(&subscribers);

        let welcome = RealtimePayload::Message {
            author: "ralph-bot".to_string(),
            body: "Connected to mock server.".to_string(),
            sent_at: format_timestamp_utc(),
            channel_id: 1,
            client_id: None,
        };
        send_payload(&mut socket, &welcome);

        thread::spawn(move || loop {
            match socket.read() {
                Ok(msg) => {
                    if let WsMessage::Text(text) = msg {
                        match serde_json::from_str::<RealtimePayload>(&text) {
                            Ok(RealtimePayload::Message { author, channel_id, .. }) => {
                                let ack = RealtimePayload::Ack {
                                    kind: "message".to_string(),
                                    detail: format!("stored for {author} in channel {channel_id}"),
                                };
                                send_payload(&mut socket, &ack);
                                broadcast_text(&subscribers, &text);
                            }
                            Ok(RealtimePayload::Auth { user, .. }) => {
                                let ack = RealtimePayload::Ack {
                                    kind: "auth".to_string(),
                                    detail: format!("welcome {user}"),
                                };
                                send_payload(&mut socket, &ack);
                                let presence = RealtimePayload::Presence {
                                    user,
                                    status: "online".to_string(),
                                };
                                if let Ok(payload) = serde_json::to_string(&presence) {
                                    broadcast_text(&subscribers, &payload);
                                }
                            }
                            Ok(RealtimePayload::Ack { .. } | RealtimePayload::Presence { .. }) => {}
                            Err(_) => {
                                broadcast_text(&subscribers, &text);
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
                        break;
                    }
                }
            }

            while let Ok(payload) = rx.try_recv() {
                if socket.send(WsMessage::Text(payload)).is_err() {
                    return;
                }
            }

            thread::sleep(Duration::from_millis(8));
        });
    }

    Ok(())
}
