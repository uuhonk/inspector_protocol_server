//! `inspector_protocol` crate 的 schema 与 Just Inspector wire 协议
//!（CDP 风格的 JSON，见 just_inspector 仓库的 `docs/protocol.md`）之间的映射。

use inspector_protocol::{
    ControlSpec, Curve, CurveMode, CurvePoint, FieldInfo, FieldValue, InspectorButton,
};
use serde_json::{json, Value};

/// 接受客户端传来的控制点上限。前端的 curve 编辑器自己限制到
/// 16；这里的限制只是为了让手写的客户端
/// 无法让游戏分配一个无上限的点列表。
const MAX_CURVE_POINTS: usize = 256;

/// 前端展示的“游戏名”。默认 `"app"`，宿主可以用 `JI_GAME_NAME` 环境变量覆盖
/// （和 `JI_PORT` 一样，不需要改代码）。
fn game_name() -> String {
    std::env::var("JI_GAME_NAME").unwrap_or_else(|_| "app".to_string())
}

pub fn game_info() -> Value {
    json!({
        "gameName": game_name(),
        "engine": "bevy",
        "engineVersion": env!("CARGO_PKG_VERSION"),
        "protocolVersion": "1.0",
        "capabilities": [
            "live-values",
            "curves",
            "world-tree",
            "node-selection",
            "type-actions",
        ],
    })
}

pub fn error_json(code: i32, message: String) -> Value {
    json!({ "error": { "code": code, "message": message } })
}

/// 单个 resource 的 `Runtime.getNode` payload。
///
/// `defaults` 是该 resource 注册时（也就是 TOML config 刚载入之后）
/// 拍下的快照；每个名字在那里仍然存在的 property 都带上一个
/// `defaultValue`，前端可以重置到它。
pub fn node_json(
    node_id: &str,
    display_name: &str,
    fields: &[FieldInfo],
    defaults: &[FieldInfo],
    buttons: &[InspectorButton],
) -> Value {
    let mut node = json!({
        "id": node_id,
        "name": display_name,
        "kind": "Resource",
        "path": node_id,
        "properties": fields
            .iter()
            .map(|field| {
                let default = defaults.iter().find(|d| d.name == field.name);
                field_to_property(field, default)
            })
            .collect::<Vec<_>>(),
    });
    // 挂在**类型**上的按钮（前端渲染成面板顶部一行）。字段上挂的那些走各自的
    // property，见 `field_to_property`；都为空时整个键不发。
    if !buttons.is_empty() {
        node["actions"] = buttons.iter().map(button_json).collect();
    }
    node
}

/// 一个按钮 -> 一个协议 action descriptor。
///
/// `id` 在协议里叫 `name`（和 property 一致），`confirm` 走 `danger` —— 前端
/// 只需要知道"这是个要小心的操作"，不必理解它具体是什么。
pub fn button_json(button: &InspectorButton) -> Value {
    let action = button;
    let mut json = json!({ "name": action.id, "label": action.label });
    if let Some(hint) = action.hint {
        json["tooltip"] = hint.into();
    }
    if action.confirm {
        json["danger"] = true.into();
    }
    json
}

/// 一个带 tag 的 `bool` 值。
pub fn bool_value(value: bool) -> Value {
    json!({ "type": "bool", "value": value })
}

/// 一个带 tag 的 `float` 值。
pub fn f32_value(value: f32) -> Value {
    json!({ "type": "float", "value": value })
}

/// 一个带 tag 的 `string` 值。
pub fn string_value(value: &str) -> Value {
    json!({ "type": "string", "value": value })
}

/// 一个带 tag 的 `vec2` 值。
pub fn vec2_value(x: f32, y: f32) -> Value {
    json!({ "type": "vec2", "x": x, "y": y })
}

/// 一个带 tag 的 `vec3` 值。
pub fn vec3_value(x: f32, y: f32, z: f32) -> Value {
    json!({ "type": "vec3", "x": x, "y": y, "z": z })
}

/// 一个带 tag 的 `enum` 值：前端会从 `options` 渲染出一个下拉框。
pub fn enum_value(value: &str, options: &[(&str, &str)]) -> Value {
    json!({
        "type": "enum",
        "value": value,
        "options": options
            .iter()
            .map(|(value, label)| json!({ "value": value, "label": label }))
            .collect::<Vec<_>>(),
    })
}

/// 一个 world-node property，对应协议的 `PropertyDescriptor`。
///
/// 只包含 world node 真正需要的键：一个名字、一个可选的控件提示、
/// 带 tag 的值，以及它是否可写。
pub fn world_property(
    name: &str,
    display_name: Option<&str>,
    control: Option<&str>,
    value: Value,
    read_only: bool,
) -> Value {
    let mut prop = json!({ "name": name, "value": value });
    if let Some(display_name) = display_name {
        prop["displayName"] = Value::from(display_name);
    }
    if let Some(control) = control {
        prop["control"] = Value::from(control);
    }
    if read_only {
        prop["readOnly"] = Value::from(true);
    }
    prop
}

/// 一个 world node 的 `Runtime.getNode` payload。
pub fn world_node_json(
    id: &str,
    name: &str,
    kind: Option<&str>,
    path: &str,
    properties: Vec<Value>,
) -> Value {
    let mut node = json!({
        "id": id,
        "name": name,
        "path": path,
        "properties": properties,
    });
    if let Some(kind) = kind {
        node["kind"] = Value::from(kind);
    }
    node
}

/// 一个 world entity 的 tree id，也是这套方案唯一存在的地方。
///
/// 用 `Entity::to_bits`（index *和* generation）而不是只用 index，
/// 这样客户端在某个 entity 被 despawn 之前留存的 id 就不会悄悄指到
/// 复用该 slot 的 entity 上。`entity:` 前缀让 world id 永远不会
/// 与 resource id（一个 Rust 类型路径）撞车。
pub fn entity_node_id(entity: bevy_ecs::entity::Entity) -> String {
    format!("entity:{:x}", entity.to_bits())
}

/// 解析 [`entity_node_id`]；其他任何东西都返回 `None` —— 包括
/// resource id，这正是调用方可以廉价地问一句 "is this one of mine?"
/// 的原因。
pub fn entity_from_node_id(id: &str) -> Option<bevy_ecs::entity::Entity> {
    let bits = u64::from_str_radix(id.strip_prefix("entity:")?, 16).ok()?;
    bevy_ecs::entity::Entity::try_from_bits(bits)
}

/// tree 的 **world** 分支里的一个 node，对应协议的 `TreeNode`。
///
/// 服务端 crate 是引擎无关的 —— 它只懂协议，对相机、mesh 或 UI
/// 一无所知 —— 所以 app 通过
/// [`WorldTree`](crate::WorldTree) 构造这些，而这个类型只负责 wire 形态。
#[derive(Clone, Debug, Default)]
pub struct WorldNode {
    /// 稳定的 id，通过 `Runtime.getNode` / `Runtime.setValue` 往返。
    pub id: String,
    /// 行标签。
    pub name: String,
    /// 显示在行右侧的短 kind（`"Ui"`、`"Mesh3d"`……）。
    pub kind: Option<String>,
    /// 当该 node 有自己的可见性时为 `Some`；此时该行会显示一个
    /// 开关，并以 `property: "visible"` 写回。
    pub visible: Option<bool>,
    /// 选中该 node 对 app 是否有意义。
    pub selectable: bool,
    /// 子 node，按显示顺序排列。
    pub children: Vec<WorldNode>,
}

impl WorldNode {
    /// 一个没有 kind、没有可见性、也没有子 node 的 node。
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            ..Self::default()
        }
    }

    /// 加一个子 node 并返回 `self`，便于用一个表达式搭出一棵树。
    pub fn with_child(mut self, child: WorldNode) -> Self {
        self.children.push(child);
        self
    }

    /// 设置短 kind。
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// 声明该 node 的可见性，这也正是让它那一行显示一个
    /// 开关的原因。
    pub fn with_visibility(mut self, visible: bool) -> Self {
        self.visible = Some(visible);
        self
    }

    /// 声明该 node 可被选中。
    pub fn with_selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    /// 序列化为协议的 `TreeNode`。`visible` / `selectable` 在未设置时会被
    /// **省略**，这样 "没有可见性" 和 "隐藏" 在
    /// wire 上仍然可区分。
    pub fn to_json(&self) -> Value {
        let mut node = json!({ "id": self.id, "name": self.name });
        if let Some(kind) = &self.kind {
            node["kind"] = Value::from(kind.as_str());
        }
        if let Some(visible) = self.visible {
            node["visible"] = Value::from(visible);
        }
        if self.selectable {
            node["selectable"] = Value::from(true);
        }
        if !self.children.is_empty() {
            node["children"] = Value::Array(self.children.iter().map(Self::to_json).collect());
        }
        node
    }
}

/// `Runtime.getTree` payload：一个虚拟根，每个 registry 一条分支。
///
/// `resources` 总是存在；当 app 注册了 [`WorldTree`](crate::WorldTree) 时，
/// `world` 就是它的 entity tree。
pub fn tree_json(resources: &[(String, String)], world: Option<WorldNode>) -> Value {
    let mut children = vec![json!({
        "id": "inspector-resources",
        "name": "Inspector Resources",
        "kind": "Registry",
        "children": resources
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name, "kind": "Resource" }))
            .collect::<Vec<_>>(),
    })];
    if let Some(world) = world {
        children.push(world.to_json());
    }
    json!({
        "id": "inspector",
        "name": "Inspector",
        "kind": "Registry",
        "children": children,
    })
}

/// 一个字段 -> 一个协议 property descriptor。
pub fn field_to_property(field: &FieldInfo, default: Option<&FieldInfo>) -> Value {
    let (control, hint) = match &field.control {
        ControlSpec::Input => ("number", None),
        ControlSpec::Slider { min, max } => {
            let mut hint = json!({ "min": min, "max": max });
            // 除非属性覆盖它，否则 derive 会用按类型区分的默认值填入
            // `step`（float 0.1，整数 1.0）。
            if let Some(step) = field.step {
                hint["step"] = step.into();
            }
            ("slider", Some(hint))
        }
        ControlSpec::Checkbox => ("checkbox", None),
        ControlSpec::Dropdown => ("dropdown", None),
        ControlSpec::ColorPlane => ("color", None),
        ControlSpec::Vector => ("vector", None),
        // 前端的 curve 编辑器读的正是这几个 hint 键
        // （`CurveControl.vue`）：轴范围，以及控制点是否可增删。
        ControlSpec::Curve {
            value_min,
            value_max,
            time_min,
            time_max,
        } => (
            "curve",
            Some(json!({
                "valueMin": value_min,
                "valueMax": value_max,
                "timeMin": time_min,
                "timeMax": time_max,
                "curveEditable": true,
            })),
        ),
    };
    let mut prop = json!({
        "name": field.name,
        "control": control,
        "value": value_json(field),
    });
    if let Some(hint) = hint {
        prop["hint"] = hint;
    }
    if let Some(group) = field.group {
        prop["group"] = group.into();
    }
    if let Some(default) = default {
        prop["defaultValue"] = value_json(default);
    }
    // 挂在这个字段上的按钮：前端渲染在这一行**之后**，所以"放在两个字段中间"就是
    // 挂在前面那个字段上。
    if !field.buttons.is_empty() {
        prop["actions"] = field.buttons.iter().map(button_json).collect();
    }
    prop
}

/// `FieldValue` -> 带 tag 的协议值。
pub fn value_json(field: &FieldInfo) -> Value {
    match &field.value {
        FieldValue::Int(v) => json!({ "type": "int", "value": v }),
        FieldValue::Float(v) => json!({ "type": "float", "value": v }),
        FieldValue::Bool(v) => json!({ "type": "bool", "value": v }),
        FieldValue::Enum(index) => {
            let options: Vec<Value> = field
                .options
                .iter()
                .map(|o| json!({ "value": o, "label": o }))
                .collect();
            let current = field.options.get(*index).copied().unwrap_or("");
            json!({ "type": "enum", "value": current, "options": options })
        }
        FieldValue::Color([r, g, b, a]) => {
            json!({ "type": "color", "r": r, "g": g, "b": b, "a": a })
        }
        FieldValue::Vec2([x, y]) => json!({ "type": "vec2", "x": x, "y": y }),
        FieldValue::Vec3([x, y, z]) => json!({ "type": "vec3", "x": x, "y": y, "z": z }),
        FieldValue::Vec4([x, y, z, w]) => {
            json!({ "type": "vec4", "x": x, "y": y, "z": z, "w": w })
        }
        FieldValue::Curve(curve) => json!({
            "type": "curve",
            "mode": curve.mode.as_str(),
            // 即使在 linear 模式下也把切线带过去：编辑器会逐点保留
            // 它们，所以一旦丢掉，用户一切换模式就会毁掉
            // 一条 bezier 曲线。
            "points": curve
                .points
                .iter()
                .map(|p| json!({
                    "x": p.x, "y": p.y, "lx": p.lx, "ly": p.ly, "rx": p.rx, "ry": p.ry,
                }))
                .collect::<Vec<_>>(),
        }),
    }
}

/// 带 tag 的协议值 -> `FieldValue`，并对照字段 schema 做校验。
pub fn value_from_json(value: &Value, field: &FieldInfo) -> Result<FieldValue, String> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "int" => {
            let v = value
                .get("value")
                .and_then(Value::as_i64)
                .ok_or_else(|| "int value must be an integer".to_string())?;
            if field.unsigned && v < 0 {
                return Err("value must be >= 0 for unsigned field".to_string());
            }
            Ok(FieldValue::Int(
                v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
            ))
        }
        "float" => {
            let v = value
                .get("value")
                .and_then(Value::as_f64)
                .ok_or_else(|| "float value must be a number".to_string())?;
            Ok(FieldValue::Float(v as f32))
        }
        "bool" => {
            let v = value
                .get("value")
                .and_then(Value::as_bool)
                .ok_or_else(|| "bool value must be a boolean".to_string())?;
            Ok(FieldValue::Bool(v))
        }
        "enum" => {
            let name = value
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| "enum value must be a string".to_string())?;
            let index = field
                .options
                .iter()
                .position(|o| *o == name)
                .ok_or_else(|| format!("invalid enum value `{name}`"))?;
            Ok(FieldValue::Enum(index))
        }
        "color" => {
            let get = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0) as f32;
            Ok(FieldValue::Color([
                get("r"),
                get("g"),
                get("b"),
                get("a").max(0.0).min(1.0),
            ]))
        }
        "vec2" | "vec3" | "vec4" => {
            let get = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0) as f32;
            Ok(match kind {
                "vec2" => FieldValue::Vec2([get("x"), get("y")]),
                "vec3" => FieldValue::Vec3([get("x"), get("y"), get("z")]),
                _ => FieldValue::Vec4([get("x"), get("y"), get("z"), get("w")]),
            })
        }
        "curve" => {
            let raw = value
                .get("points")
                .and_then(Value::as_array)
                .ok_or_else(|| "curve value must have a `points` array".to_string())?;
            if raw.len() > MAX_CURVE_POINTS {
                return Err(format!(
                    "curve has {} points (max {MAX_CURVE_POINTS})",
                    raw.len()
                ));
            }
            let mut points = Vec::with_capacity(raw.len());
            for (index, point) in raw.iter().enumerate() {
                let component = |key: &str| -> Result<f32, String> {
                    let value = point
                        .get(key)
                        .and_then(Value::as_f64)
                        .ok_or_else(|| format!("curve point {index}: `{key}` must be a number"))?;
                    if !value.is_finite() {
                        return Err(format!("curve point {index}: `{key}` must be finite"));
                    }
                    Ok(value as f32)
                };
                points.push(CurvePoint {
                    x: component("x")?,
                    y: component("y")?,
                    lx: component("lx")?,
                    ly: component("ly")?,
                    rx: component("rx")?,
                    ry: component("ry")?,
                });
            }
            let mode = value
                .get("mode")
                .and_then(Value::as_str)
                .map(CurveMode::from_wire)
                .unwrap_or_default();
            Ok(FieldValue::Curve(Curve { points, mode }))
        }
        other => Err(format!("unsupported value type `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// id 通过 wire 往返，而任何不属于我们的东西 —— 一个 resource id、
    /// 垃圾数据 —— 都会被拒绝，而不是被错误地解析。
    #[test]
    fn entity_ids_round_trip_and_reject_foreign_ids() {
        let entity = bevy_ecs::entity::Entity::from_bits(0x0000_0007_0000_0003);
        let id = entity_node_id(entity);
        assert_eq!(entity_from_node_id(&id), Some(entity));

        assert_eq!(entity_from_node_id("my::game::Config"), None);
        assert_eq!(entity_from_node_id("entity:not-hex"), None);
        assert_eq!(entity_from_node_id("entity:"), None);
    }

    /// `visible` 和 `selectable` 是被省略而不是发成 false，正是这一点
    /// 让前端能从缺失的键里读出 "它自己没有可见性"，
    /// 从 `false` 里读出 "隐藏"。
    #[test]
    fn a_world_node_omits_the_flags_it_does_not_have() {
        let plain = WorldNode::new("a", "A").to_json();
        assert!(plain.get("visible").is_none(), "{plain}");
        assert!(plain.get("selectable").is_none(), "{plain}");
        assert!(plain.get("children").is_none(), "{plain}");

        let hidden = WorldNode::new("b", "B")
            .with_visibility(false)
            .with_selectable(true)
            .with_kind("Ui")
            .with_child(WorldNode::new("c", "C"))
            .to_json();
        assert_eq!(hidden["visible"], Value::from(false));
        assert_eq!(hidden["selectable"], Value::from(true));
        assert_eq!(hidden["kind"], Value::from("Ui"));
        assert_eq!(hidden["children"][0]["name"], Value::from("C"));
    }

    /// 两个 registry 都存在，而当 app 没有注册 world 分支时它就不存在 ——
    /// 此时前端显示的是一片空 world，
    /// 而不是错误。
    #[test]
    fn the_root_has_resources_and_optionally_a_world() {
        let resources = vec![("my::Config".to_string(), "Config".to_string())];
        let only_resources = tree_json(&resources, None);
        assert_eq!(only_resources["children"].as_array().unwrap().len(), 1);
        assert_eq!(only_resources["children"][0]["id"], "inspector-resources");

        let with_world = tree_json(&resources, Some(WorldNode::new("world", "World")));
        assert_eq!(with_world["children"].as_array().unwrap().len(), 2);
        assert_eq!(with_world["children"][1]["id"], "world");
    }
}
