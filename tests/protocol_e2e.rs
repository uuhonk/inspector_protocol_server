//! 端到端协议测试：一个真实的 WebSocket 客户端（tokio-tungstenite）与
//! Just Inspector 服务端对话，同时一个 headless 的 Bevy `App` 跑在测试主
//! 线程上。（`App` 不是 `Send`，所以客户端跑在派生出的线程上，
//! 并把结果报回来。）

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use bevy_app::App;
use bevy_ecs::prelude::{Resource, World};
use futures_util::{SinkExt, StreamExt};
use inspector_protocol_server::{InspectorRegistryExt, JustInspectorPlugin};
use serde_json::{json, Value};

#[derive(Resource, inspector_protocol::Inspector, Debug, PartialEq)]
struct TestConfig {
    /// 不分组：它出现在第一个 `#[inspector_group(..)]` *之前*。
    ///
    /// 这个字段上挂了一个按钮（渲染在这一行**之后**）：它会写穿 resource。
    #[inspector(button(label = "加十", run = count_higher, hint = "把 count 设成 42", confirm))]
    count: u32,
    /// 负的边界值也顺便检验 derive 的符号分离 float 生成
    /// （见 derive 里的 `f32_expr`）；`Motion` 打开一个 section，
    /// 其后每个字段都继承它。
    #[inspector_group("Motion")]
    #[inspector(slider(min = -10.0, max = 10.0))]
    speed: f32,
    enabled: bool,
    /// glam 向量字段 -> `vector` 控件 / `vec2` 值（同样属于该 group）。
    center: bevy_math::Vec2,
    /// Curve 字段 -> `curve` 控件，使用默认的带符号轴范围。
    ///
    /// 这个字段上挂了一个**执行体报错**的按钮：验证它变成 4106 而不是协议错误。
    #[inspector(button(label = "炸一下", run = explode))]
    edge: inspector_protocol::Curve,
}

/// 把 `count` 设成 42（按钮执行体：写穿 resource，下面的断言据此确认它真的跑了）。
fn count_higher(world: &mut World) -> Result<String, String> {
    world.resource_mut::<TestConfig>().count = 42;
    Ok("count = 42".to_string())
}

/// 总是失败：执行体自己的错误 -> 4106。
fn explode(_world: &mut World) -> Result<String, String> {
    Err("炸了".to_string())
}

const PORT: u16 = 18765;

#[test]
fn protocol_roundtrip() {
    std::env::set_var("JI_PORT", PORT.to_string());

    let mut app = App::new();
    // 先 insert，再 register：`register_inspector` 会把该 resource
    // 快照成逐字段的默认值（这对应 main.rs 里的情形：注册时
    // config plugin 已经载入了它们的 TOML）。
    app.add_plugins(JustInspectorPlugin);
    app.insert_resource(TestConfig {
        count: 3,
        speed: 1.5,
        enabled: true,
        center: bevy_math::Vec2::new(4.0, -2.5),
        edge: inspector_protocol::Curve::linear(&[(0.0, 0.0), (0.5, 1.0), (1.0, -0.5)]),
    })
    .register_inspector::<TestConfig>();

    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(run_client())
        }))
        .map_err(|payload| {
            if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "client panicked".to_string()
            }
        });
        let _ = done_tx.send(outcome);
    });

    // 在客户端探测它的同时驱动 app。
    let deadline = Instant::now() + Duration::from_secs(20);
    let outcome = loop {
        match done_rx.try_recv() {
            Ok(outcome) => break outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    panic!("client timed out after 20s");
                }
                app.update();
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("client thread died without reporting");
            }
        }
    };
    outcome.expect("protocol assertions passed");
}

async fn run_client() {
    let url = format!("ws://127.0.0.1:{PORT}");

    // 服务端线程需要一点时间来 bind listener。
    let (mut ws, _) = 'connect: {
        for attempt in 0..50 {
            match tokio_tungstenite::connect_async(&url).await {
                Ok(conn) => break 'connect conn,
                Err(e) if attempt < 49 => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) => panic!("connect {url} after retries: {e}"),
            }
        }
        unreachable!()
    };

    // 1. 服务端用 Inspector.ready 打招呼。
    let ready = read_frame(&mut ws).await;
    assert_eq!(ready["method"], "Inspector.ready", "ready: {ready}");
    assert_eq!(ready["params"]["engine"], "bevy");

    // 2. Inspector.hello.
    send(&mut ws, json!({"id": 1, "method": "Inspector.hello"})).await;
    let resp = read_response(&mut ws, 1).await;
    assert_eq!(resp["result"]["protocolVersion"], "1.0");

    // 3. Runtime.getTree -> resources 分支，每个注册项一个 node。
    //
    // 根是 inspector 本身；resources 是它下面的一条分支，
    // 当 app 注册了 world/entity tree 时那是第二条。
    send(&mut ws, json!({"id": 2, "method": "Runtime.getTree"})).await;
    let resp = read_response(&mut ws, 2).await;
    let branches = resp["result"]["root"]["children"]
        .as_array()
        .expect("children");
    assert_eq!(branches.len(), 1, "no app registers a world tree: {resp}");
    assert_eq!(branches[0]["id"], "inspector-resources");
    let children = branches[0]["children"].as_array().expect("children");
    assert_eq!(children.len(), 1, "tree: {resp}");
    let node_id = children[0]["id"].as_str().unwrap().to_string();
    assert!(node_id.contains("TestConfig"), "node id: {node_id}");

    // 4. Runtime.getNode -> 控件 / 类型映射。
    send(
        &mut ws,
        json!({"id": 3, "method": "Runtime.getNode", "params": {"nodeId": node_id}}),
    )
    .await;
    let resp = read_response(&mut ws, 3).await;
    let props = resp["result"]["node"]["properties"]
        .as_array()
        .expect("props");
    let speed = props
        .iter()
        .find(|p| p["name"] == "speed")
        .expect("speed prop");
    assert_eq!(speed["control"], "slider", "{speed}");
    assert_eq!(
        speed["value"],
        json!({"type": "float", "value": 1.5}),
        "{speed}"
    );
    assert_eq!(speed["hint"]["min"], -10.0);
    assert_eq!(speed["hint"]["max"], 10.0);
    assert_eq!(speed["group"], "Motion", "{speed}");
    // 启动快照会作为逐字段的重置目标发出去。
    assert_eq!(
        speed["defaultValue"],
        json!({"type": "float", "value": 1.5}),
        "{speed}"
    );
    let enabled = props
        .iter()
        .find(|p| p["name"] == "enabled")
        .expect("enabled prop");
    assert_eq!(enabled["control"], "checkbox");
    assert_eq!(enabled["value"], json!({"type": "bool", "value": true}));
    assert_eq!(enabled["group"], "Motion", "{enabled}");
    let count = props
        .iter()
        .find(|p| p["name"] == "count")
        .expect("count prop");
    assert!(count.get("group").is_none(), "ungrouped: {count}");
    assert_eq!(
        count["defaultValue"],
        json!({"type": "int", "value": 3}),
        "{count}"
    );
    // Vec2 字段会变成一个逐分量的 `vector` 控件。
    let center = props
        .iter()
        .find(|p| p["name"] == "center")
        .expect("center prop");
    assert_eq!(center["control"], "vector", "{center}");
    // group 继承会越过 `enabled` 一直延续到结构体末尾。
    assert_eq!(center["group"], "Motion", "{center}");
    assert_eq!(
        center["value"],
        json!({"type": "vec2", "x": 4.0, "y": -2.5}),
        "{center}"
    );
    assert_eq!(
        center["defaultValue"],
        json!({"type": "vec2", "x": 4.0, "y": -2.5}),
        "{center}"
    );

    // 4b. Curve 字段会变成一个 `curve` 控件，带上 widget 的轴范围。
    let edge = props
        .iter()
        .find(|p| p["name"] == "edge")
        .expect("edge prop");
    assert_eq!(edge["control"], "curve", "{edge}");
    assert_eq!(edge["hint"]["valueMin"], -1.0, "{edge}");
    assert_eq!(edge["hint"]["valueMax"], 1.0, "{edge}");
    assert_eq!(edge["hint"]["timeMin"], 0.0, "{edge}");
    assert_eq!(edge["hint"]["timeMax"], 1.0, "{edge}");
    assert_eq!(edge["hint"]["curveEditable"], true, "{edge}");
    assert_eq!(edge["value"]["type"], "curve", "{edge}");
    assert_eq!(edge["value"]["mode"], "linear", "{edge}");
    let points = edge["value"]["points"].as_array().expect("points");
    assert_eq!(points.len(), 3, "{edge}");
    assert_eq!(points[1]["x"], 0.5);
    assert_eq!(points[1]["y"], 1.0);
    // 切线随每个点一起传递，这样 bezier 曲线才能往返。
    assert_eq!(points[1]["lx"], 0.0);
    assert_eq!(points[1]["ry"], 0.0);

    // 4c. 按钮挂在**字段**上：`actions` 出现在那个 property 里（前端据此把按钮渲染在
    // 那一行之后），id 走 `name`，`confirm` 走 `danger`。这个类型没有挂在类型上的
    // 按钮，所以 node 级的 `actions` 键根本不发。
    assert!(
        resp["result"]["node"].get("actions").is_none(),
        "没有类型级按钮就不该发这个键：{resp}"
    );
    let count_prop = props.iter().find(|p| p["name"] == "count").unwrap();
    let edge_prop = props.iter().find(|p| p["name"] == "edge").unwrap();
    assert_eq!(
        count_prop["actions"][0]["name"], "count_higher",
        "{count_prop}"
    );
    assert_eq!(count_prop["actions"][0]["label"], "加十", "{count_prop}");
    assert_eq!(
        count_prop["actions"][0]["tooltip"], "把 count 设成 42",
        "{count_prop}"
    );
    assert_eq!(count_prop["actions"][0]["danger"], true, "{count_prop}");
    assert_eq!(edge_prop["actions"][0]["name"], "explode", "{edge_prop}");
    assert!(
        edge_prop["actions"][0].get("danger").is_none(),
        "普通按钮不该标 danger: {edge_prop}"
    );
    assert!(
        edge_prop["actions"][0].get("tooltip").is_none(),
        "{edge_prop}"
    );
    // 没挂按钮的字段连这个键都没有。
    assert!(speed.get("actions").is_none(), "{speed}");

    // 4d. Runtime.invokeAction 跑注册进来的执行体，并把那句话回报给客户端。
    send(
        &mut ws,
        json!({"id": 20, "method": "Runtime.invokeAction", "params": {
            "nodeId": node_id, "action": "count_higher"
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 20).await;
    assert_eq!(resp["result"]["ok"], true, "{resp}");
    assert_eq!(resp["result"]["message"], "count = 42", "{resp}");
    // 执行体真的改到了 ECS（不是只回了一句话）。
    send(
        &mut ws,
        json!({"id": 21, "method": "Runtime.getNode", "params": {"nodeId": node_id}}),
    )
    .await;
    let resp = read_response(&mut ws, 21).await;
    let count = resp["result"]["node"]["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "count")
        .unwrap();
    assert_eq!(count["value"]["value"], 42, "{resp}");

    // 4e. 执行体自己的错误 -> 4106；未知 action -> 4105；未知 node -> 4100。
    send(
        &mut ws,
        json!({"id": 22, "method": "Runtime.invokeAction", "params": {
            "nodeId": node_id, "action": "explode"
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 22).await;
    assert_eq!(resp["error"]["code"], 4106, "{resp}");
    assert_eq!(resp["error"]["message"], "炸了", "{resp}");
    send(
        &mut ws,
        json!({"id": 23, "method": "Runtime.invokeAction", "params": {
            "nodeId": node_id, "action": "nope"
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 23).await;
    assert_eq!(resp["error"]["code"], 4105, "{resp}");
    send(
        &mut ws,
        json!({"id": 24, "method": "Runtime.invokeAction", "params": {
            "nodeId": "missing", "action": "count_higher"
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 24).await;
    assert_eq!(resp["error"]["code"], 4100, "{resp}");

    // 5. Runtime.setValue 通过 apply_field 写入。
    send(
        &mut ws,
        json!({"id": 4, "method": "Runtime.setValue", "params": {
            "nodeId": node_id, "property": "speed", "value": {"type": "float", "value": 2.5}
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 4).await;
    assert_eq!(resp["result"]["value"]["value"], 2.5, "{resp}");

    // 6. 应用之后游戏推送 Runtime.nodeChanged。
    let ev = read_event(&mut ws).await;
    assert_eq!(ev["method"], "Runtime.nodeChanged", "{ev}");
    assert_eq!(ev["params"]["nodeId"], node_id);
    assert_eq!(ev["params"]["property"], "speed");

    // 7. 读回来 -> 值已持久化在 ECS resource 里。
    send(
        &mut ws,
        json!({"id": 5, "method": "Runtime.getNode", "params": {"nodeId": node_id}}),
    )
    .await;
    let resp = read_response(&mut ws, 5).await;
    let speed = resp["result"]["node"]["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "speed")
        .unwrap();
    assert_eq!(speed["value"]["value"], 2.5, "{resp}");

    // 7b. Vec2 写穿：带 tag 的 `vec2` payload 会逐分量应用，
    //     并且重置基线仍然是启动值，而不是被编辑后的值。
    send(
        &mut ws,
        json!({"id": 8, "method": "Runtime.setValue", "params": {
            "nodeId": node_id, "property": "center",
            "value": {"type": "vec2", "x": 1.0, "y": 2.0}
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 8).await;
    assert_eq!(
        resp["result"]["value"],
        json!({"type": "vec2", "x": 1.0, "y": 2.0}),
        "{resp}"
    );
    send(
        &mut ws,
        json!({"id": 9, "method": "Runtime.getNode", "params": {"nodeId": node_id}}),
    )
    .await;
    let resp = read_response(&mut ws, 9).await;
    let center = resp["result"]["node"]["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "center")
        .unwrap();
    assert_eq!(
        center["value"],
        json!({"type": "vec2", "x": 1.0, "y": 2.0}),
        "{resp}"
    );
    assert_eq!(
        center["defaultValue"],
        json!({"type": "vec2", "x": 4.0, "y": -2.5}),
        "{resp}"
    );

    // 7c. Curve 写穿：点和 mode 都会应用，切线能在往返中存活，
    //     并且重置基线仍然是启动时的曲线。
    let edited = json!({
        "type": "curve",
        "mode": "bezier",
        "points": [
            {"x": 0.0, "y": -1.0, "lx": 0.0, "ly": 0.0, "rx": 0.25, "ry": 0.5},
            {"x": 1.0, "y": 1.0, "lx": -0.25, "ly": -0.5, "rx": 0.0, "ry": 0.0},
        ]
    });
    send(
        &mut ws,
        json!({"id": 10, "method": "Runtime.setValue", "params": {
            "nodeId": node_id, "property": "edge", "value": edited
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 10).await;
    assert_eq!(resp["result"]["value"], edited, "{resp}");
    send(
        &mut ws,
        json!({"id": 11, "method": "Runtime.getNode", "params": {"nodeId": node_id}}),
    )
    .await;
    let resp = read_response(&mut ws, 11).await;
    let edge = resp["result"]["node"]["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "edge")
        .unwrap();
    assert_eq!(edge["value"], edited, "{resp}");
    assert_eq!(edge["defaultValue"]["mode"], "linear", "{resp}");
    assert_eq!(edge["defaultValue"]["points"].as_array().unwrap().len(), 3);

    // 7d. 畸形的曲线会被拒绝，且不会碰到 resource。
    send(
        &mut ws,
        json!({"id": 12, "method": "Runtime.setValue", "params": {
            "nodeId": node_id, "property": "edge",
            "value": {"type": "curve", "points": [{"x": 0.0, "y": 0.0}]}
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 12).await;
    assert_eq!(resp["error"]["code"], 4102, "{resp}");

    // 8. 错误路径。
    send(
        &mut ws,
        json!({"id": 6, "method": "Runtime.setValue", "params": {
            "nodeId": node_id, "property": "nope", "value": {"type": "float", "value": 1.0}
        }}),
    )
    .await;
    let resp = read_response(&mut ws, 6).await;
    assert_eq!(resp["error"]["code"], 4102, "{resp}");
    send(
        &mut ws,
        json!({"id": 7, "method": "Runtime.getNode", "params": {"nodeId": "missing"}}),
    )
    .await;
    let resp = read_response(&mut ws, 7).await;
    assert_eq!(resp["error"]["code"], 4100, "{resp}");

    ws.close(None).await.ok();
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        frame.to_string().into(),
    ))
    .await
    .expect("send");
}

async fn read_frame(ws: &mut Ws) -> Value {
    loop {
        match ws.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                return serde_json::from_str(&text).expect("parse frame");
            }
            Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                return serde_json::from_slice(&bytes).expect("parse binary frame");
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("ws error: {e}"),
            None => panic!("ws closed"),
        }
    }
}

/// 跳过事件（没有 `id` 的 frame），直到 `id` 对应的响应到达。
async fn read_response(ws: &mut Ws, id: u64) -> Value {
    loop {
        let frame = read_frame(ws).await;
        if frame.get("id") == Some(&json!(id)) {
            return frame;
        }
    }
}

/// 跳过响应，直到某个事件到达（服务端在每次成功的 setValue 之后都会推送
/// nodeChanged，周期性的 diff 还可能再加一些）。
async fn read_event(ws: &mut Ws) -> Value {
    loop {
        let frame = read_frame(ws).await;
        if frame.get("id").is_none() {
            return frame;
        }
    }
}
