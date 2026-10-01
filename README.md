# dircrypt2

[![Native tests and release candidate](https://github.com/hatsune-miku/dircrypt2/actions/workflows/release.yml/badge.svg)](https://github.com/hatsune-miku/dircrypt2/actions/workflows/release.yml)

面向 Windows、macOS 和 Linux 大目录的可恢复文件名映射工具。Rust 原生核心，SQLite 保存完整恢复计划；默认不读取、不修改文件内容。隐藏目录、`node_modules/.pnpm`、空目录和文件系统链接均纳入处理。

这是独立的 Rust 实现，`main` 仅包含新体系。当前恢复格式为 **2**，不读取旧 Python 格式或早期 Windows 试验版的格式 1。已有旧归档请先使用创建它的旧程序恢复。

## 下载

在 [Releases](https://github.com/hatsune-miku/dircrypt2/releases) 下载对应压缩包和 `SHA256SUMS`：

| 系统 | 产物 | 运行要求 |
|---|---|---|
| Windows AMD64 | `dircrypt2-x86_64-pc-windows-msvc.zip` | Windows 10/11 x64；静态 MSVC 运行库和 SQLite |
| macOS ARM64 | `dircrypt2-aarch64-apple-darwin.tar.gz` | Apple Silicon，macOS 11+；无 Developer ID 签名、未公证 |
| Linux AMD64 | `dircrypt2-x86_64-unknown-linux-musl.tar.gz` | x86-64，静态 musl；需要内核/文件系统支持 `RENAME_NOREPLACE`，以及已挂载的 `/proc` |

解压后直接运行，无需 Python 或外部 SQLite。macOS 下载的未公证程序可能需要在系统“隐私与安全性”中明确允许打开。

## 使用

```powershell
# 没有 DCDATA：只映射文件名和目录名，内容不变
.\dircrypt.exe "G:\example"

# 没有 DCDATA：额外混淆大于 16 字节的普通文件的前 16 字节
.\dircrypt.exe "G:\example" --obfuscate

# 已有 DCDATA：自动恢复，不需要额外参数
.\dircrypt.exe "G:\example"

# 可选：并行处理独立目录、附加扩展名、输出性能报告
.\dircrypt.exe "G:\example" --jobs 2 --suffix .bin --report .\run.json
```

macOS / Linux 使用相同参数：

```sh
./dircrypt ./example
./dircrypt ./example --obfuscate
./dircrypt ./example --report ./run.json
```

每次执行都会根据当前目录状态选择方向；第二次执行同一个已映射目录就是恢复。

方向先由根目录下 `DCDATA` 是否存在决定。恢复只使用数据库中的头部备份，`--obfuscate` 不决定恢复方向，也不会让普通映射的文件内容发生变化。

`--jobs 0` 为默认值：FAT 系列使用 1 个目录工作线程，其他支持的本地文件系统使用 2 个；可指定 1–16。单个目录按顺序处理，多线程只分配独立目录。`--suffix` 允许 ASCII 字母、数字、点、下划线和连字符。`--quiet` 关闭周期进度，保留错误和最终统计。`--report` 只创建目标目录之外的新文件，不覆盖现有报告；操作失败时该报告可能为空。

扫描、记录校验、文件处理、目录处理分别显示进度，每秒刷新一次，并列出近期速度、平均速度、百分比和当前名称。扫描阶段尚不知道总数，显示 `?`。速度单位是当前阶段的条目数，不是磁盘数据吞吐量。

## 存储结构

```text
example/
├── .dircrypt.lock
└── DCDATA/
    ├── state.sqlite3
    └── data/
        ├── ~a1b2c300000001
        └── ~a1b2c300000002/
            └── ~a1b2c300000003
```

SQLite 使用父目录 ID 加名称组件保存路径；Unicode 名称统一存为 UTF-8，无法表示为 Unicode 的名称保留原平台编码，没有盘符、绝对路径或当前工作目录依赖。一般目录保留原层级，在原父目录中改短名称；只有顶层条目移入 `DCDATA/data`，不会把所有文件聚集到一个巨大目录。

根目录达到 4,096 个条目时分桶；FAT 系列的任意目录达到该规模时也分桶，每桶最多分配 1,024 个原始条目。短名称降低目录项占用，分桶限制新增目标目录的宽度。一个原本极宽的 exFAT 源目录仍然可能产生昂贵的名称查找，不能保证所有目录形状下速度完全恒定。

`.dircrypt.lock` 是根目录保留文件，恢复后仍保留，用于避免两个进程同时处理同一目录。不要在程序运行时修改目标树。Unix 文件锁是协作式锁，不能阻止不遵守锁协议的其他程序修改或移动文件。树内的符号链接、junction 和其他 reparse point 只改链接自身名称，不遍历目标；相对链接在映射期间可能不可用，恢复后还原。移动或复制归档不会改写链接原本指向的路径。

## 恢复与错误处理

- 修改任何文件前，先提交包含全部相对名称、文件身份和可选原始头部的 SQLite 事务。使用回滚日志和 `synchronous=EXTRA`。
- 恢复计划一经生成就保持不变，不为每个文件写完成标记。恢复核对原名称和映射名称的实际存在情况，覆盖改名之后进程突然退出的窗口。
- 映射时先改名，再写混淆头；恢复时核对头部符合原始、混淆或部分写入状态，再从备份覆盖原始头、刷新该文件并还原名称。遇到其他头部改动会停止；重复恢复不会再次 XOR 原始数据。
- SQLite 完整性检查、每条记录校验和、全表清单校验用于发现损坏或缺失的恢复记录。它们不是防恶意篡改的身份认证。
- 不覆盖已经存在的目标。遇到缺失文件、文件身份变化、损坏记录或持久占用时停止，保留恢复资料；Windows 短暂共享冲突最多重试三次，额外等待总计 250 ms。
- 清理只删除已经确认为空的目录。遇到额外文件会停止并保留数据，不递归删除用户目录。
- 普通映射保留硬链接关系；头部混淆拒绝共享硬链接，以免修改归档之外的同一文件对象。

按 `Ctrl+C` 会停止安排后续工作并等待正在进行的操作退出。发生中断或错误后，**保留整个目标目录及 `DCDATA`，重新执行同一命令进行恢复**。不要单独删除数据库、重命名映射条目或覆盖冲突文件。数据库无法识别或已损坏时，程序会停止，不猜测原始名称或头部。

同卷移动整个目标目录后可以恢复。完整映射成功后，也可以完整复制到其他位置或计算机；复制时需保留文件修改时间、整个 `DCDATA` 以及目录结构；文件名必须能够由目标系统和文件系统表示。不能跨系统表示的非 Unicode 名称会被拒绝。复制后的文件 ID 不可沿用，改用记录名称、类型、大小、时间戳核对。Windows 在实测 exFAT 上对复制/设置修改时间按 2 秒舍入，因此涉及 FAT 系列时允许这一精度差异；NTFS、APFS 和支持的 Linux 原生文件系统原地恢复核对文件 ID 与时间。Unix 头部操作保留纳秒级修改时间和文件权限。中断中的归档应在原始目录树恢复，不支持通过复制一个不完整快照来接续。

Windows 支持本地 NTFS、exFAT、FAT/FAT32；macOS 支持 APFS、HFS+、exFAT/FAT；Linux 支持 ext、XFS、Btrfs、F2FS、tmpfs 和内核 FAT/exFAT。真实设备性能测量覆盖 Windows NTFS/exFAT，CI 的功能验证覆盖三平台运行器上的本地文件系统，其他列出的文件系统尚无同等设备测试。未知或网络文件系统会拒绝处理；Linux 不支持 FUSE/NTFS-3G。Unix 发现跨设备挂载点、socket、FIFO 或设备文件时会在规划阶段停止；不会尝试打开它们的内容。程序可以处理长路径和深层级，仍受底层文件系统、权限、打开文件和可用空间限制。进程强制退出测试不能代表断电、磁盘损坏或拔出 USB 的所有情况；SQLite 事务与文件系统改名不构成跨系统原子事务。

**这不是密码学加密。** 没有密码或密钥，数据库包含原始名称和原始头部，头部混淆是固定 XOR。它用于可逆映射和格式混淆，不能提供保密性。

## 构建与验证

构建工具链固定为 Rust 1.94.1。Windows 需要 MSVC C++ 构建工具；macOS 需要 Xcode Command Line Tools；Linux 需要 C 编译器，构建发布用的 musl 目标还需要 `musl-tools`。SQLite 静态编译进各平台程序。Windows 符号链接测试需要开发者模式或创建符号链接的权限。

```powershell
cargo build --release --locked
.\target\release\dircrypt.exe --help

cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked -- --test-threads=1

# 可选：在第二种文件系统上检查完整复制归档的恢复
$env:DIRCRYPT_TEST_VOLUME = 'G:\'
cargo test --test engine copy_complete_archive_to_another_filesystem -- --ignored
```

集成测试覆盖真实进程退出、重复恢复、部分头部写入、清理中断、记录损坏/删除、占用和冲突、隐藏目录、Unicode、70 层路径、空文件、16/17 字节边界、符号链接环、硬链接、宽目录、取消及跨根目录复制。故障注入只编译进 debug 构建，release 不响应测试用退出变量。

性能测量见 [BENCHMARKS.md](BENCHMARKS.md)。`tools/benchmark_native.py` 只生成新的测试目录，再调用独立 CLI，分别报告映射和恢复耗时，并在映射后、恢复后检查全部文件内容。Python 仅用于这一开发工具。

```powershell
python tools/benchmark_native.py --parent 'G:\' --files 130000 --output exfat.json
```

基准生成和内容校验不计入映射/恢复耗时，不会修改现有用户目录。测试目录会保留供检查，可在核对报告后删除输出中明确列出的 `.dircrypt-bench-v2-*` 目录。

## 自动发布

每次 push 到 `main` 都运行三平台格式检查、Clippy、原生集成测试、release 构建，以及 18 组跨平台恢复组合（三个来源 × 两种模式 × 三个恢复平台）。PR 运行相同检查但不发布；也可以手动运行工作流。

全部成功后，工作流创建唯一的 `v2.0.0-rc.<运行序号>.<提交短哈希>` 标签，上传三份压缩包和 `SHA256SUMS`，最后发布 RC。任一平台或恢复测试失败均不会发布不完整产物。工作流只在发布 job 授予 `contents: write`，不需要另配个人访问令牌。

RC 使用 GitHub 的 **Pre-release** 标志。GitHub 不允许预发布同时设为 Latest，因此请从 Releases 列表选择最新 RC；不把候选版冒充正式稳定版。重跑已成功发布的同一次工作流不会重写该 Release。
