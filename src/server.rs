//! Bevy inspector 的 WebSocket 服务端线程。
//!
//! 在专用线程上跑一个 tokio runtime，说 Just Inspector 的
//! JSON 协议（CDP 风格的信封）。它从不直接碰 ECS：
//! 命令被转发到 Bevy 主线程（crossbeam channel + oneshot
//! 应答），运行时事件则广播回来（tokio broadcast channel）。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 服务端线程发给 Bevy 主线程的命令。
pub enum ServerCommand {
    Hello {
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    Ping {
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    GetTree {
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    GetNode {
        node_id: String,
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    SetValue {
        node_id: String,
        property: String,
        value: Value,
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    /// `Runtime.invokeAction`：客户端点了某个类型面板上的一个按钮。
    ///
    /// 服务端只把它转发给 app 注册的执行体（见 `InspectorRegistration::action`），
    /// 自己不理解这个 action 是什么意思。
    InvokeAction {
        node_id: String,
        action: String,
        reply: tokio::sync::oneshot::Sender<Value>,
    },
    /// `Runtime.select`：客户端选中了一个 node；app 高亮它。
    /// 选择被清空时 `node_id` 为 `None`。
    Select {
        node_id: Option<String>,
        reply: tokio::sync::oneshot::Sender<Value>,
    },
}

/// 在整个 app 生命周期内保持存活；drop 它就关闭服务端。
pub struct ServerThread {
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl Drop for ServerThread {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.shutdown.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(());
            }
        }
    }
}

pub fn spawn_server_thread(
    port: u16,
    command_tx: Sender<ServerCommand>,
    event_tx: tokio::sync::broadcast::Sender<Value>,
) -> ServerThread {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown = Arc::new(Mutex::new(Some(shutdown_tx)));

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        runtime.block_on(async move {
            let listener = match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => l,
                Err(err) => {
                    eprintln!("[inspector_protocol_server] failed to bind port {port}: {err}");
                    return;
                }
            };
            println!("[inspector_protocol_server] listening on ws://127.0.0.1:{port}");

            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        if let Ok((stream, addr)) = accepted {
                            tokio::spawn(handle_client(stream, addr, command_tx.clone(), event_tx.clone()));
                        }
                    }
                }
            }
        });
    });

    ServerThread { shutdown }
}

async fn handle_client(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    command_tx: Sender<ServerCommand>,
    event_tx: tokio::sync::broadcast::Sender<Value>,
) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    println!("[inspector_protocol_server] client connected from {addr}");

    let (mut sink, mut source) = ws.split();
    let mut events = event_tx.subscribe();

    // 用游戏身份信息向客户端打招呼。
    if sink
        .send(WsMessage::Text(
            json!({
                "method": "Inspector.ready",
                "params": crate::protocol::game_info(),
            })
            .to_string()
            .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = source.next() => {
                match incoming {
                    Some(Ok(WsMessage::Text(text))) => {
                        if !handle_frame(&text, &command_tx, &mut sink).await {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            if !handle_frame(&text, &command_tx, &mut sink).await {
                                break;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                if let Ok(value) = event {
                    if sink.send(WsMessage::Text(value.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    println!("[inspector_protocol_server] client {addr} disconnected");
}

/// 当连接应当被关闭时返回 false。
async fn handle_frame(
    text: &str,
    command_tx: &Sender<ServerCommand>,
    sink: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        WsMessage,
    >,
) -> bool {
    let frame: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            let _ = sink
                .send(WsMessage::Text(
                    json!({"id": -1, "error": {"code": -32700, "message": "Parse error"}})
                        .to_string()
                        .into(),
                ))
                .await;
            return true;
        }
    };
    let Some(id) = frame["id"].as_u64() else {
        return true;
    };
    let method = frame["method"].as_str().unwrap_or("");
    let params = &frame["params"];

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Value>();
    let command = match method {
        "Inspector.hello" => Some(ServerCommand::Hello { reply: reply_tx }),
        "Inspector.ping" => Some(ServerCommand::Ping { reply: reply_tx }),
        "Runtime.getTree" => Some(ServerCommand::GetTree { reply: reply_tx }),
        "Runtime.getNode" => Some(ServerCommand::GetNode {
            node_id: params["nodeId"].as_str().unwrap_or("").to_string(),
            reply: reply_tx,
        }),
        "Runtime.setValue" => Some(ServerCommand::SetValue {
            node_id: params["nodeId"].as_str().unwrap_or("").to_string(),
            property: params["property"].as_str().unwrap_or("").to_string(),
            value: params["value"].clone(),
            reply: reply_tx,
        }),
        // 类型面板上的按钮：前端只回传 (nodeId, action)，具体干什么在 app 侧。
        "Runtime.invokeAction" => Some(ServerCommand::InvokeAction {
            node_id: params["nodeId"].as_str().unwrap_or("").to_string(),
            action: params["action"].as_str().unwrap_or("").to_string(),
            reply: reply_tx,
        }),
        // `nodeId: null` 是一条真实指令（"清空它"），所以这里缺失的键
        // 和显式的 null 必须表示同一件事。
        "Runtime.select" => Some(ServerCommand::Select {
            node_id: params["nodeId"].as_str().map(str::to_string),
            reply: reply_tx,
        }),
        _ => {
            let _ = sink
                .send(WsMessage::Text(
                    json!({"id": id, "error": {"code": -32601, "message": format!("Method not found: {method}")}})
                        .to_string()
                        .into(),
                ))
                .await;
            return true;
        }
    };

    if let Some(command) = command {
        if command_tx.send(command).is_err() {
            return false;
        }
        // 等待 Bevy 主线程应答（它每帧 tick 一次）。
        match tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx).await {
            Ok(Ok(payload)) => {
                let response = if payload.get("error").is_some() {
                    json!({ "id": id, "error": payload["error"] })
                } else {
                    json!({ "id": id, "result": payload })
                };
                if sink
                    .send(WsMessage::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            _ => {
                let _ = sink
                    .send(WsMessage::Text(
                        json!({"id": id, "error": {"code": -32603, "message": "Timeout waiting for game"}})
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    }
    true
}
