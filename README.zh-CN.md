# Agent Task Scheduler (ats)

[English](README.md) | 简体中文

一台电脑上的多个 AI agent 会话可以同时编译、测试、跑模型。每个会话都当整台机器归自己用。结果内存耗尽，系统开始换页，所有命令都变慢，直到超时。

`ats` 是一个本地的重型 shell 命令队列。agent 把重命令交给 `ats run`，守护进程在机器有足够内存时启动它，并告诉它用多少个并行任务。命令在 agent 的环境里运行，输出和退出码原样返回。

- **按内存放行。** 命令的预测内存峰值放得进空闲内存时才启动。CPU 交给操作系统调度，`ats` 只设置每个任务的并行度。
- **按实测预测。** 每次运行都记录实测的内存峰值和耗时，同一命令下次运行时按这些值预测。
- **压力下减负。** 系统开始换页时，`ats` 不再启动新命令；在 macOS 上还会把正在跑的任务降为后台优先级。它不会暂停或终止任务。
- **由 agent 判断。** 指令告诉模型哪些命令交给 `ats`。轻命令直接运行，没有额外开销。

`ats` 支持 macOS 和 Linux，64 位 Arm 和 x86。任何能运行 shell 命令的 agent 都能用。提醒 hook 适用于 Claude Code 和 Codex。

## 安装

把这句提示词交给你的 AI 编程 agent，例如 Claude Code、Codex、OpenCode 或 Gemini CLI：

```text
帮我安装 Agent Task Scheduler（ats）。阅读 https://raw.githubusercontent.com/Natsuki-Kaede/agent-task-scheduler/main/INSTALL.md 并按步骤操作。修改本项目以外的文件前先问我。
```

agent 会安装二进制文件，并把要做的改动展示给你，你同意后才写入。

手动安装：

```sh
brew install natsuki-kaede/tap/ats
ats install
```

Homebrew 在 macOS 和 Linux 上都能用。没有 Homebrew 时，从 [Releases](https://github.com/Natsuki-Kaede/agent-task-scheduler/releases) 下载对应机器的压缩包；或者装好 Rust 工具链，运行 `cargo install --locked --git https://github.com/Natsuki-Kaede/agent-task-scheduler`。

`ats install` 把每处改动以 diff 展示，你同意前不写入任何文件：

- 给模型的指令：`~/.claude/ats.md`，加上 `~/.claude/CLAUDE.md` 里的一行 `@ats.md`；以及 `~/.codex/AGENTS.md` 里的一节；
- `~/.claude/settings.json` 和 `~/.codex/hooks.json` 里的两个提醒 hook，告诉 agent 哪些命令还在跑、哪些已经结束；
- 启动守护进程的登录服务：macOS 上是 LaunchAgent，Linux 上是 systemd 用户服务。

其他 agent 用 `ats install --service` 只装登录服务；`ats instructions` 打印指令，你把它加进该 agent 的指令文件。

Codex 下次启动时会请你信任改动过的 hook。

在 Linux 上，你关闭最后一个登录会话时 systemd 会停掉守护进程。如果你通过 SSH 工作，运行 `loginctl enable-linger` 让它保持运行。

`ats uninstall` 以同样的 diff 和确认移除这些内容。

## 使用

模型把重命令写成 `ats run -- cargo build` 或 `ats run --sh 'npm run build'`。命令在 90 秒内结束时，输出和平常一样。运行更久时，`ats` 打印一行

```
[ats] submission s_x job j_y is running; log: ...; check with: ats wait s_x
```

并以退出码 75 退出，命令继续在后台运行。`ats` 的其他退出码：76 表示 `--after` 指定的前置任务失败，77 表示 `ats` 没有接受这次提交，78 表示 `ats` 自身出错。

| 命令 | |
|---|---|
| `ats run [选项] -- <程序> [参数...]` | 机器有足够内存时运行程序 |
| `ats run [选项] --sh '<shell 代码>'` | 用你的 shell 运行一段 shell 代码 |
| `ats wait <id>` | 等提交结束，然后返回它的输出和退出码 |
| `ats status [<id>]` | 列出任务，或查看一个提交或任务 |
| `ats logs <id> [-f]` | 查看任务输出，或持续跟随 |
| `ats why <id>` | 查看任务为什么在等 |
| `ats cancel <id>` | 停止一个提交 |
| `ats detach <id>` | agent 可以不等这个提交就结束本轮，提交结束时 agent 会收到通知 |
| `ats unblock <id>` | 运行前置任务失败的任务 |

`ats run` 的选项：`--wait <时长>`（默认 90s）或 `--no-wait`；`--after <id,...>` 在其他任务成功后再运行；`--lock <路径或名称>` 用于同一时间只能给一个任务用的资源；`--hint mem=6G` 用于命令需要的内存比预测多的情况；`--now` 不排队直接启动。

在运行 shell 命令前会询问的 Claude Code 权限模式下，每条 `ats run` 也会询问。不要添加 `Bash(ats run:*)` 这样的允许规则，它等于允许任何命令。

## 配置

文件在 macOS 的 `~/Library/Application Support/ats/`，Linux 的 `~/.local/share/ats/`。

`config.toml` 列出把其余参数当作命令运行的程序，`ats` 据此识别其中的命令：

```toml
[recognize]
wrappers = ["mytool"]
```

工具画像说明如何识别一个工具、它通常需要多少内存、会写哪些路径，以及如何设置它的并行度。`profiles.d/` 里的文件新增画像，或替换同名的内置画像。格式见 `profiles/builtin.toml`。

## 许可证

GPL-3.0-only，见 [LICENSE](LICENSE)。
