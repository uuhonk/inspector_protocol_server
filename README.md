# inspector_protocol_server

Just Inspector 的 Bevy 服务端，通过 JSON WebSocket 暴露 Inspector schema 与操作。
协议详情见 just_inspector 仓库的 `docs/protocol.md`。

## Bevy 兼容版本

当前使用 Bevy App / ECS 0.20，测试用 Bevy Math 也是 0.20。
`rust-toolchain.toml` 固定 Rust 1.97.1。调用方与 `inspector_protocol` 必须使用
相同 Bevy minor，避免 Resource / World 等 ECS 类型不兼容。

```sh
cargo check --all-targets
cargo fmt --all --check
cargo test
```

本地同时修改协议仓库而尚未推送时，可以传临时 Cargo patch 验证：

```sh
cargo test --config 'patch."https://github.com/uuhonk/inspector_protocol.git".inspector_protocol.path="../inspector_protocol"'
```

协议仓库提交并推送后，运行 `cargo update -p inspector_protocol` 刷新锁文件。
