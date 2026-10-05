//! Just Inspector —— Bevy 服务端。
//!
//! 把每一个注册过的 [`inspector_protocol::Inspector`] resource 通过 JSON
//! WebSocket 协议暴露给基于 HTML 的 Just Inspector 前端（浏览器版本或
//! VS Code 扩展）（见 just_inspector 仓库里的 `docs/protocol.md`）。
//!
//! ```
//! use bevy_app::App;
//! use bevy_ecs::prelude::Resource;
//! use inspector_protocol_server::{InspectorRegistryExt, JustInspectorPlugin};
//!
//! #[derive(Resource, inspector_protocol::Inspector)]
//! struct MyConfig {
//!     #[inspector(slider(min = 0.0, max = 10.0))]
//!     speed: f32,
//! }
//!
//! App::new()
//!     .add_plugins(JustInspectorPlugin)
//!     .register_inspector::<MyConfig>()
//!     .run();
//! ```
//!
//! WebSocket 服务端跑在专用的 tokio 线程上（`ws://127.0.0.1:8765`，
//! 可用 `JI_PORT` 环境变量覆盖）；Bevy 主线程通过一个 exclusive system
//! 每帧应答命令。游戏自己改动的字段值通过大约每 250 ms 一次的 schema diff
//! 检测出来，并以 `Runtime.nodeChanged` 事件推送给已连接的客户端。

mod protocol;
mod server;

use std::any::type_name;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::component::Mutable;
use bevy_ecs::message::MessageReader;
use bevy_ecs::prelude::{Entity, Message, Res, ResMut, Resource, World};
use bevy_ecs::schedule::IntoScheduleConfigs;
use inspector_protocol::{FieldInfo, FieldValue, Inspector};
use serde_json::json;

use protocol::{node_json, tree_json, value_from_json, value_json};

// app 的整个对外面：tree trait、node 形态、共享的 entity-id
// 方案，以及 `Value` —— 全部 re-export，这样 app 就不必自己
// 依赖 `serde_json`。
pub use protocol::{
    bool_value, entity_from_node_id, entity_node_id, enum_value, f32_value, string_value,
    vec2_value, vec3_value, world_node_json, world_property, WorldNode,
};
pub use serde_json::Value;
// 类型上挂按钮要用它（`#[inspector(button(..))]` 生成的代码引用 `::inspector_protocol`，
// app 只要 `use inspector_protocol::Inspector;` 就够；这里 re-export 是给
// `inspector_buttons()` 之类的手写实现用的）。
pub use inspector_protocol::InspectorButton;
use server::{spawn_server_thread, ServerCommand, ServerThread};

/// 游戏侧 schema 多久 diff 一次并推送给客户端。
const DIFF_INTERVAL: Duration = Duration::from_millis(250);

/// 一个已注册、类型已擦除的 inspector resource。
struct Registration {
    node_id: &'static str,
    display_name: &'static str,
    read_fields: fn(&World) -> Vec<FieldInfo>,
    /// 挂在**类型**上的按钮（`#[inspector(button(..))]` 写在 struct 上）。
    read_buttons: fn(&World) -> &'static [InspectorButton],
    apply_field: fn(&mut World, &str, FieldValue) -> bool,
    /// 游戏启动时（也就是从 TOML 载入、任何编辑之前）该 resource 的
    /// 快照。它作为每个 property 的 `defaultValue` 发给客户端，
    /// 好让 UI 能提供逐字段的重置。
    ///
    /// 在 `register_inspector` 时捕获；如果那时 resource 还没被插入，
    /// 就改在第一次读取时填充。
    defaults: Vec<FieldInfo>,
}

fn make_registration<T>(defaults: Vec<FieldInfo>) -> Registration
where
    T: Resource<Mutability = Mutable> + Inspector,
{
    Registration {
        node_id: type_name::<T>(),
        display_name: short_type_name::<T>(),
        read_fields: |world| {
            world
                .get_resource::<T>()
                .map(Inspector::inspector_fields)
                .unwrap_or_default()
        },
        read_buttons: |world| {
            world
                .get_resource::<T>()
                .map(Inspector::inspector_buttons)
                .unwrap_or(&[])
        },
        apply_field: |world, field, value| {
            let Some(mut res) = world.get_resource_mut::<T>() else {
                return false;
            };
            res.apply_field(field, value);
            true
        },
        defaults,
    }
}

/// 找一个按钮的执行体并跑它。
///
/// 查找顺序：**字段上挂的**（`#[inspector(button(..))]` 在某个字段上，前端把按钮渲染
/// 在那个字段那一行之后）优先，然后是**类型上挂的**（面板顶部那一行）。id 在同一个
/// 类型里唯一，所以顺序其实只影响同一个 id 同时挂在两处这种手滑。
///
/// 单独抽出来是为了能**不经网络**测它（见文件末尾的单测）：它只依赖
/// [`Registration`] 和 world。
fn run_action(
    reg: &Registration,
    world: &mut World,
    action: &str,
) -> Result<String, (i32, String)> {
    let field_button = (reg.read_fields)(world).into_iter().find_map(|field| {
        field
            .buttons
            .iter()
            .find(|b| b.action.id == action)
            .copied()
    });
    let button = field_button.or_else(|| {
        (reg.read_buttons)(world)
            .iter()
            .find(|b| b.action.id == action)
            .copied()
    });
    match button {
        Some(button) => (button.run)(world).map_err(|message| (4106, message)),
        None => Err((4105, format!("Unknown action: {}.{action}", reg.node_id))),
    }
}

fn short_type_name<T>() -> &'static str {
    type_name::<T>()
        .rsplit("::")
        .next()
        .unwrap_or(type_name::<T>())
}

/// app 侧负责 tree 的 world/entity 分支。
///
/// 这个 crate 只懂协议，对相机、mesh 或 UI 一无所知 —— 它只依赖
/// `bevy_ecs` —— 所以遍历 world 是 *app* 的活儿。想让自己的 entity
/// 出现在 tree 里的 app 实现这个 trait，然后调用
/// [`InspectorRegistryExt::register_world_tree`]。
///
/// 每个方法都会拿到 `&mut World`，这里这样做是安全的，因为 registry
/// 在 pump 运行之前就被从 world 里取出来了（见
/// [`inspector_system`]）。
pub trait WorldTree: Send + Sync + 'static {
    /// world 分支下的整棵树，包含根。
    fn tree(&self, world: &mut World) -> WorldNode;

    /// 针对 [`Self::tree`] 里某个 id 的 `Runtime.getNode`。
    ///
    /// 返回协议的 `NodeDetail` JSON。`None` 会让服务端应答
    /// `4100 Unknown node`，对于一个自上次取 tree 之后已被 despawn 的
    /// entity 来说，这也是正确答案。
    fn node(&self, world: &mut World, node_id: &str) -> Option<Value>;

    /// 针对 [`Self::tree`] 里某个 id 的 `Runtime.setValue`。
    ///
    /// 返回用于回显的归一化值，或者一个错误码 + 消息：未知 node 用
    /// `4100`，未知或非法的 property 用 `4102`，
    /// 只读 property 用 `4101`。
    fn set_value(
        &self,
        world: &mut World,
        node_id: &str,
        property: &str,
        value: &Value,
    ) -> Result<Value, (i32, String)>;

    /// `Runtime.select`：记住 `node_id`（或清空它），好让 app
    /// 高亮它。`false` —— 也就是默认值 —— 表示 app 不做
    /// 选择，此时服务端会应答 `-32601`。
    fn select(&self, _world: &mut World, _node_id: Option<&str>) -> bool {
        false
    }
}

/// 客户端最后一次要求 app 高亮的那个 node。
///
/// 服务端从 `Runtime.select` 写入它；app 也*可以*写它
/// （在它自己的 viewport 里点选），两种情况服务端都会推送由此产生的
/// [`InspectorSelectionChanged`] 事件 —— 正是这一点让 tree
/// 跟随 app 自己的 picking。
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct InspectorSelection(pub Option<Entity>);

/// 每当 [`InspectorSelection`] 变化时就触发（作为协议事件）。
#[derive(Message, Default, Debug, Clone, Copy)]
pub struct InspectorSelectionChanged;

/// 触发它来告诉客户端 world tree 的*形状*变了（一个根被 spawn
/// 或 despawn），好让它们重新取一次。
///
/// 服务端自己算不出来 —— 只有 app 知道它的 entity 结构什么时候
/// 动过 —— 所以这由 app 决定，就像 resource diff 由服务端
/// 决定一样。
#[derive(Message, Default, Debug, Clone, Copy)]
pub struct InspectorTreeChanged;

/// 可被检查的 resource 的 registry + 服务端接线（channel、变更 diff）。
#[derive(Resource)]
pub struct InspectorRegistry {
    registrations: Vec<Registration>,
    command_rx: crossbeam_channel::Receiver<ServerCommand>,
    event_tx: tokio::sync::broadcast::Sender<Value>,
    last_snapshot: HashMap<&'static str, Vec<FieldInfo>>,
    last_diff: Instant,
    /// app 的 world/entity tree，前提是它注册过一个。
    world_tree: Option<Box<dyn WorldTree>>,
    /// 让 WebSocket 服务端线程在 app 的整个生命周期内存活。
    _server: ServerThread,
}

/// 注册一个 resource 供检查：`App::register_inspector::<T>()`。
pub trait InspectorRegistryExt {
    /// 注册一个 resource 供检查。类型上挂的按钮（`#[inspector(button(..))]`）由
    /// 那个类型的 `Inspector` 实现自带，所以这里不需要再传任何东西。
    fn register_inspector<T>(&mut self) -> &mut Self
    where
        T: Resource<Mutability = Mutable> + Inspector;

    /// 把 app 的 entity 放进 tree：`App::register_world_tree(tree)`。
    ///
    /// tree 是可选的 —— 没什么可浏览的游戏干脆不注册它，此时协议的
    /// `world-tree` capability 就是一个 tree 没有兑现的
    /// 承诺，客户端看到的是一条空分支，而不是错误。
    fn register_world_tree<T: WorldTree>(&mut self, tree: T) -> &mut Self;
}

impl InspectorRegistryExt for App {
    fn register_inspector<T>(&mut self) -> &mut Self
    where
        T: Resource<Mutability = Mutable> + Inspector,
    {
        // *现在*就快照该 resource：注册发生在 config plugin 载入
        // TOML 之后不久，所以这就是 UI 的逐字段重置按钮要恢复到的
        // 启动基线。
        let defaults = self
            .world()
            .get_resource::<T>()
            .map(Inspector::inspector_fields)
            .unwrap_or_default();
        self.world_mut()
            .resource_mut::<InspectorRegistry>()
            .registrations
            .push(make_registration::<T>(defaults));
        self
    }

    fn register_world_tree<T: WorldTree>(&mut self, tree: T) -> &mut Self {
        self.world_mut()
            .resource_mut::<InspectorRegistry>()
            .world_tree = Some(Box::new(tree));
        self
    }
}

/// 加上 Just Inspector 服务端（默认监听 `ws://127.0.0.1:8765` 的
/// WebSocket），以及每帧的命令 pump 和变更 diff system。
pub struct JustInspectorPlugin;

impl Plugin for JustInspectorPlugin {
    fn build(&self, app: &mut App) {
        let (command_tx, command_rx) = crossbeam_channel::unbounded::<ServerCommand>();
        let (event_tx, _) = tokio::sync::broadcast::channel::<Value>(128);
        let port = std::env::var("JI_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8765);
        let server = spawn_server_thread(port, command_tx.clone(), event_tx.clone());

        app.insert_resource(InspectorRegistry {
            registrations: Vec::new(),
            command_rx,
            event_tx,
            last_snapshot: HashMap::new(),
            last_diff: Instant::now(),
            world_tree: None,
            _server: server,
        })
        .init_resource::<InspectorSelection>()
        .add_message::<InspectorSelectionChanged>()
        .add_message::<InspectorTreeChanged>();
        app.add_systems(Update, (inspector_system, push_app_events).chain());
    }
}

/// 推送由 *app* 决定的两个事件：selection 变了，以及 tree 的形状
/// 变了。
///
/// 两者读的都是 app 主动 raise 的 message，而不是去观察状态，所以两个都
/// 不 raise 的 app 没有任何开销 —— 服务端也永远不必去猜一个它看不见的
/// entity tree 的 "changed" 是什么意思。
fn push_app_events(
    mut selection_changes: MessageReader<InspectorSelectionChanged>,
    mut tree_changes: MessageReader<InspectorTreeChanged>,
    selection: Res<InspectorSelection>,
    registry: ResMut<InspectorRegistry>,
) {
    if selection_changes.read().count() > 0 {
        let node_id = selection.0.map(protocol::entity_node_id);
        registry.broadcast(json!({
            "method": "Runtime.selectionChanged",
            "params": { "nodeId": node_id },
        }));
    }
    if tree_changes.read().count() > 0 {
        registry.broadcast(json!({
            "method": "Runtime.treeChanged",
            "params": { "reason": "world" },
        }));
    }
}

/// Exclusive system：应答协议命令并推送值变更。
/// registry 被从 World 里取出来，好让它的 vtable 能借用它。
fn inspector_system(world: &mut World) {
    let mut registry = world.remove_resource::<InspectorRegistry>().expect(
        "InspectorRegistry is missing — add JustInspectorPlugin before registering resources",
    );
    registry.pump(world);
    registry.diff_and_push(world);
    world.insert_resource(registry);
}

impl InspectorRegistry {
    fn pump(&mut self, world: &mut World) {
        while let Ok(command) = self.command_rx.try_recv() {
            match command {
                ServerCommand::Hello { reply } => {
                    let _ = reply.send(protocol::game_info());
                }
                ServerCommand::Ping { reply } => {
                    let _ = reply.send(json!({ "pong": "pong" }));
                }
                ServerCommand::GetTree { reply } => {
                    let children: Vec<(String, String)> = self
                        .registrations
                        .iter()
                        .map(|r| (r.node_id.to_string(), r.display_name.to_string()))
                        .collect();
                    let world = self.world_tree.as_ref().map(|tree| tree.tree(world));
                    let _ = reply.send(json!({ "root": tree_json(&children, world) }));
                }
                ServerCommand::GetNode { node_id, reply } => match self.node(world, &node_id) {
                    Some(node) => {
                        let _ = reply.send(json!({ "node": node }));
                    }
                    None => {
                        let _ = reply.send(protocol::error_json(
                            4100,
                            format!("Unknown node: {node_id}"),
                        ));
                    }
                },
                ServerCommand::SetValue {
                    node_id,
                    property,
                    value,
                    reply,
                } => match self.apply(world, &node_id, &property, &value) {
                    Ok(echo) => {
                        let _ = reply.send(json!({ "ok": true, "value": echo }));
                        self.broadcast(json!({
                            "method": "Runtime.nodeChanged",
                            "params": { "nodeId": node_id, "property": property },
                        }));
                    }
                    Err((code, message)) => {
                        let _ = reply.send(protocol::error_json(code, message));
                    }
                },
                ServerCommand::InvokeAction {
                    node_id,
                    action,
                    reply,
                } => {
                    let reg = self.registrations.iter().find(|r| r.node_id == node_id);
                    match reg {
                        Some(reg) => match run_action(reg, world, &action) {
                            // 按钮通常改了 resource（甚至 spawn 了实体）；字段值的
                            // 变化由 250 ms 的 diff 自己推给客户端，这里只回报一句话。
                            Ok(message) => {
                                let _ = reply.send(json!({ "ok": true, "message": message }));
                            }
                            Err((code, message)) => {
                                let _ = reply.send(protocol::error_json(code, message));
                            }
                        },
                        None => {
                            let _ = reply.send(protocol::error_json(
                                4100,
                                format!("Unknown node: {node_id}"),
                            ));
                        }
                    }
                }
                ServerCommand::Select { node_id, reply } => {
                    // 清空永远是可接受的 —— 一个自己没有 selection
                    // 的 app 本来就没什么可清的。
                    let handled = match &self.world_tree {
                        Some(tree) => tree.select(world, node_id.as_deref()),
                        None => false,
                    };
                    if handled || node_id.is_none() {
                        let _ = reply.send(json!({ "ok": true }));
                    } else {
                        let _ = reply.send(protocol::error_json(
                            -32601,
                            "Runtime.select is not implemented by this game".to_string(),
                        ));
                    }
                }
            }
        }
    }

    fn node(&mut self, world: &mut World, node_id: &str) -> Option<Value> {
        if let Some(tree) = &self.world_tree {
            if let Some(node) = tree.node(world, node_id) {
                return Some(node);
            }
        }
        let reg = self
            .registrations
            .iter_mut()
            .find(|r| r.node_id == node_id)?;
        let fields = (reg.read_fields)(world);
        // 针对在插入之前就被注册的 resource 的回退：把第一次观察到的
        // 状态当作默认基线。
        if reg.defaults.is_empty() && !fields.is_empty() {
            reg.defaults = fields.clone();
        }
        let buttons = (reg.read_buttons)(world);
        Some(node_json(
            reg.node_id,
            reg.display_name,
            &fields,
            &reg.defaults,
            buttons,
        ))
    }

    fn apply(
        &mut self,
        world: &mut World,
        node_id: &str,
        property: &str,
        value: &Value,
    ) -> Result<Value, (i32, String)> {
        if let Some(tree) = &self.world_tree {
            // world tree 认识它自己的 id；resource 的 id 会落到
            // 下面的 registry 上。
            match tree.set_value(world, node_id, property, value) {
                Ok(echo) => return Ok(echo),
                Err((4100, _)) => {}
                Err(other) => return Err(other),
            }
        }
        let Some(reg) = self.registrations.iter().find(|r| r.node_id == node_id) else {
            return Err((4100, format!("Unknown node: {node_id}")));
        };
        let fields = (reg.read_fields)(world);
        let Some(field) = fields.iter().find(|f| f.name == property) else {
            return Err((4102, format!("Unknown property: {node_id}.{property}")));
        };
        let field_value = value_from_json(value, field).map_err(|msg| (4102, msg))?;
        let applied = (reg.apply_field)(world, property, field_value);
        if !applied {
            return Err((
                4101,
                format!("Failed to apply '{property}' (resource not present or read-only)"),
            ));
        }
        // 把归一化后的值回显回去。
        let fresh = (reg.read_fields)(world);
        let echo = fresh
            .iter()
            .find(|f| f.name == property)
            .map(value_json)
            .ok_or_else(|| (4102, format!("Property disappeared: {property}")))?;
        Ok(echo)
    }

    /// 把每个已注册的 resource 与上次快照做 diff，并对游戏自己改动过的
    /// 字段推送 `Runtime.nodeChanged`。
    fn diff_and_push(&mut self, world: &mut World) {
        if self.last_diff.elapsed() < DIFF_INTERVAL {
            return;
        }
        self.last_diff = Instant::now();
        let mut changes: Vec<(String, String)> = Vec::new();
        for reg in &self.registrations {
            let fields = (reg.read_fields)(world);
            let prev = self
                .last_snapshot
                .entry(reg.node_id)
                .or_insert_with(|| fields.clone());
            for (before, after) in prev.iter().zip(fields.iter()) {
                if before.name == after.name
                    && (before.value != after.value || before.control != after.control)
                {
                    changes.push((reg.node_id.to_string(), after.name.to_string()));
                }
            }
            *prev = fields;
        }
        for (node_id, property) in changes {
            self.broadcast(json!({
                "method": "Runtime.nodeChanged",
                "params": { "nodeId": node_id, "property": property },
            }));
        }
    }

    fn broadcast(&self, frame: Value) {
        let _ = self.event_tx.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inspector_protocol::{InspectorAction, InspectorButton};

    #[derive(Resource, Default)]
    struct Counter(u32);

    fn bump(world: &mut World) -> Result<String, String> {
        let mut counter = world.resource_mut::<Counter>();
        counter.0 += 1;
        Ok(format!("counter = {}", counter.0))
    }

    fn boom(_world: &mut World) -> Result<String, String> {
        Err("炸了".to_string())
    }

    /// 字段上挂的按钮（`&[..]` 在函数体里不是 `'static`，所以要做成 const 项）。
    const FIELD_BUTTONS: &[InspectorButton] = &[InspectorButton::new(
        InspectorAction::new("bump", "加一"),
        bump,
    )];

    /// 类型上挂的按钮。
    const TYPE_BUTTONS: &[InspectorButton] = &[InspectorButton::new(
        InspectorAction::new("boom", "炸").confirm(),
        boom,
    )];

    /// 一个手搭的 registration：`bump` 挂在**字段**上（模拟 `#[inspector(button(..))]`
    /// 写在一个字段上），`boom` 挂在**类型**上（面板顶部那一行）。
    fn registration() -> Registration {
        Registration {
            node_id: "test::Counter",
            display_name: "Counter",
            read_fields: |_| {
                vec![FieldInfo {
                    name: "count",
                    control: inspector_protocol::ControlSpec::Input,
                    value: FieldValue::Int(0),
                    options: &[],
                    unsigned: true,
                    step: None,
                    group: None,
                    buttons: FIELD_BUTTONS,
                }]
            },
            read_buttons: |_| TYPE_BUTTONS,
            apply_field: |_, _, _| false,
            defaults: Vec::new(),
        }
    }

    /// 按钮跑的是挂在它上面的那个函数（它真的动了 world）；未知 action 报 4105，
    /// 执行体自己返回的错误变成 4106（"按钮执行失败"），不会被当成协议错误。
    ///
    /// 两处挂法（字段 / 类型）都能找到。
    #[test]
    fn actions_run_their_registered_function() {
        let mut world = World::new();
        world.insert_resource(Counter::default());
        let reg = registration();

        // 字段级。
        assert_eq!(run_action(&reg, &mut world, "bump").unwrap(), "counter = 1");
        assert_eq!(run_action(&reg, &mut world, "bump").unwrap(), "counter = 2");
        assert_eq!(world.resource::<Counter>().0, 2);

        // 类型级（执行体报错 -> 4106）。
        assert_eq!(
            run_action(&reg, &mut world, "boom"),
            Err((4106, "炸了".to_string()))
        );
        match run_action(&reg, &mut world, "nope") {
            Err((4105, message)) => assert!(message.contains("nope"), "{message}"),
            other => panic!("{other:?}"),
        }
    }
}
