你是一只苦哈哈的牛马，你从公司巨大项目的 monorepo 中抽取了一个 crate 到独立仓库。但是你们使用的编译器版本极其古典，导致独立仓库根本编译不了。你需要对照原来 workspace 的 Cargo.lock，把独立仓库的依赖版本对齐到旧版本。这个过程漫长又痛苦，你红温了，于是你写了一个小工具，叫做 `cargo-lock-align`。

除了上面这句话，整个仓库都是 AI 写的。

# cargo-lock-align

`cargo-lock-align` 是一个用于对齐 Rust 项目 `Cargo.lock` 依赖版本的命令行工具。

它适合在拆分仓库、迁移 crate、抽取独立 workspace 或搭建最小复现工程时使用：新项目的直接依赖可能已经和旧项目一致，但传递依赖会因为重新解析而升级到更新版本。这个工具会读取旧项目的 `Cargo.lock`，再逐步把新项目的依赖锁定到旧项目中兼容的版本。

## 安装

发布到 crates.io 后可以通过 Cargo 安装：

```bash
cargo install cargo-lock-align
```

安装后推荐作为 Cargo 子命令运行：

```bash
cargo lock-align --old-lock /path/to/old/Cargo.lock --manifest-path /path/to/new/Cargo.toml
```

本地开发时也可以直接通过 `cargo run` 运行：

```bash
cargo run -- --old-lock /path/to/old/Cargo.lock --manifest-path /path/to/new/Cargo.toml
```

## 快速开始

先使用 `--dry-run` 查看将要调整的依赖：

```bash
cargo lock-align \
  --old-lock /path/to/old/Cargo.lock \
  --manifest-path /path/to/new/Cargo.toml \
  --dry-run
```

确认输出符合预期后，去掉 `--dry-run` 让工具修改新项目的 `Cargo.lock`：

```bash
cargo lock-align \
  --old-lock /path/to/old/Cargo.lock \
  --manifest-path /path/to/new/Cargo.toml
```

工具不会直接手写 `Cargo.lock`。它会调用 Cargo 自己的命令：

```bash
cargo update -p <当前包版本> --precise <目标版本或 git rev>
```

每次只对齐一个包，然后重新读取 `cargo metadata` 和当前 `Cargo.lock`，避免一次性计算出的依赖计划在中途变成过期状态。

## 使用场景

常见场景包括：

- 从大仓库中抽取一个 crate 到独立仓库
- 新项目需要尽量复用旧项目的依赖版本
- 迁移过程中需要减少 lockfile 抖动
- 排查依赖升级导致的构建或运行时差异
- 对齐 git dependency 的旧提交版本

## 参数

### `--old-lock <PATH>`

旧项目的 `Cargo.lock` 路径。这个 lockfile 会作为版本基准。

```bash
cargo lock-align --old-lock ../old-project/Cargo.lock
```

### `--manifest-path <PATH>`

新项目的 `Cargo.toml` 路径。默认是当前目录下的 `Cargo.toml`。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --manifest-path ./Cargo.toml
```

### `--dry-run`

只打印将要对齐的依赖，不修改 `Cargo.lock`。

```bash
cargo lock-align --old-lock ../old-project/Cargo.lock --dry-run
```

### `--no-default-features`

解析新项目依赖图时不启用默认 features。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --no-default-features
```

### `--all-features`

解析新项目依赖图时启用所有 features。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --all-features
```

### `--features <FEATURES>`

解析新项目依赖图时启用指定 features。可以用逗号分隔，也可以传多次。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --features foo,bar
```

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --features foo \
  --features bar
```

### `--max-depth <N>`

限制最多对齐到第几层依赖。默认不限制。

`1` 表示只看直接依赖，`2` 表示继续看直接依赖的子依赖，以此类推。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --max-depth 2
```

### `--no-dev`

不对齐 dev-dependencies。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --no-dev
```

### `--no-build`

不对齐 build-dependencies。

```bash
cargo lock-align \
  --old-lock ../old-project/Cargo.lock \
  --no-build
```

## 对齐规则

### 只对齐 semver 兼容版本

工具不会把包强行降到 semver 不兼容的版本。

例如当前项目中有：

```text
proc-macro2 1.0.107
```

旧 lockfile 中同时有：

```text
proc-macro2 0.4.30
proc-macro2 1.0.95
```

工具会选择：

```text
proc-macro2 1.0.107 -> 1.0.95
```

而不会错误地选择 `0.4.30`。

对于 `0.x` 版本，工具按 Cargo 常见的兼容语义处理：`0.y.z` 只会对齐到同一个 `0.y` 版本线。

### 支持 git dependency

对于 git dependency，工具会忽略 source 字符串里的 commit hash，用仓库和分支识别同一个依赖，并把旧 lockfile 中的 commit 作为目标。

例如：

```text
molten-ffi 0.14.2#87226568 -> 0.14.2#090efe06
```

实际执行时会使用旧 lockfile 中的 git rev 作为 `--precise` 目标。

工具也会处理 URL 编码差异。例如下面两个 source 会被识别为同一个分支：

```text
branch=v/0.14.x
branch=v%2F0.14.x
```

### 支持旧 lockfile 中的本地 patch 包

旧项目可能通过 `[patch.crates-io]` 把 crates.io 包替换成本地 path 包。这类包在旧 `Cargo.lock` 中可能没有 `source` 字段。

如果新项目解析到的是同名 registry 包，工具会允许用旧 lockfile 中的版本作为对齐目标。

例如：

```text
backtrace 0.3.76 -> 0.3.73
prost 0.11.9 -> 0.11.2
getrandom 0.2.17 -> 0.2.10
```

这个兜底规则只用于 registry 包，不会把本地 path 包误匹配到 git dependency。

## 输出说明

常见输出包括：

```text
OK   tokio                          1.46.0
SKIP some-crate                     new=1.2.3 (not found in old lock)
SKIP some-crate                     new=1.2.3 (no compatible version in old lock)
```

含义如下：

- `OK`：新项目当前版本已经和旧 lockfile 中的目标版本一致
- `SKIP ... not found in old lock`：旧 lockfile 中没有找到可匹配的包
- `SKIP ... no compatible version in old lock`：旧 lockfile 中有同名或同源包，但没有 semver 兼容版本

待对齐的包会显示为：

```text
Packages to align:
  futures 0.3.34 -> 0.3.31
  tokio-util 0.7.19 -> 0.7.13
  tokio 1.53.1 -> 1.46.0
```

## 工作方式

工具的核心流程是：

1. 读取旧项目的 `Cargo.lock`
2. 使用 `cargo metadata` 解析新项目当前依赖图
3. 读取新项目当前 `Cargo.lock`
4. 找出可以向旧 lockfile 对齐的包
5. 按依赖拓扑排序，优先处理父依赖，再处理子依赖
6. 调用 `cargo update --precise` 对齐一个包
7. 重新解析依赖图，重复以上流程

一次只更新一个包是刻意设计的。因为更新父依赖后，子依赖的解析结果可能会变化；如果一次性按旧依赖图更新所有包，容易在中间状态触发 Cargo 的版本约束错误。

## 注意事项

- 请先使用 `--dry-run` 检查输出
- 建议在干净的 git worktree 中运行，方便审查 lockfile diff
- 工具会修改 `--manifest-path` 所在目录下的 `Cargo.lock`
- 工具依赖 Cargo 的解析结果，如果 `cargo metadata` 本身失败，需要先修复项目依赖配置
- 工具不会绕过 Cargo 的版本约束；如果旧版本不满足当前依赖要求，Cargo 会拒绝更新

## License

本项目使用 `MIT OR Apache-2.0` 双许可证。
