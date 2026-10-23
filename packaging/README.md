# 打包

所有格式都由 GitHub Actions 在打 tag 时自动构建（`.github/workflows/release.yml`），
产物直接挂到 Release 页面。

## 产物矩阵

| 文件 | 包管理器 | 主要发行版 | 构建环境 |
| --- | --- | --- | --- |
| `xime_<ver>_amd64.deb` | apt / dpkg | Debian、Ubuntu、Mint、Deepin、UOS | ubuntu-latest（`cargo-deb`） |
| `xime-<ver>-1.fc<N>.x86_64.rpm` | dnf / zypper | Fedora、RHEL 系、openSUSE | Fedora 容器（`rpmbuild`） |
| `xime-<ver>-1-x86_64.pkg.tar.zst` | pacman | Arch、Manjaro、EndeavourOS | Arch 容器（`makepkg`） |
| `xime-<ver>-x86_64-linux.tar.gz` | 手动 | 任意发行版 | ubuntu-latest（`stage.sh`） |
| `xime-<ver>.tar.gz` | 源码 | 打包者 | 含 rime-wubi 子模块的完整源码树 |

rpm 和 arch 都在各自的发行版容器里**从源码编译**，不是把 Ubuntu 的二进制换个壳，
所以 glibc / 依赖关系是对的。

## 安装

```bash
# Debian / Ubuntu
sudo apt install ./xime_*_amd64.deb

# Fedora / RHEL / openSUSE
sudo dnf install ./xime-*.rpm          # openSUSE 用 sudo zypper install ./xime-*.rpm

# Arch / Manjaro
sudo pacman -U ./xime-*.pkg.tar.zst

# 通用（解开就是 /usr 下的布局）
sudo tar -C / -xzf xime-*-x86_64-linux.tar.gz
```

装完在 KDE 里启用（KWin 只在启动时装配虚拟键盘，改完需要重新登录）：

```bash
kwriteconfig6 --file kwinrc --group Wayland --key VirtualKeyboard xime-launcher.desktop
qdbus6 org.kde.KWin /KWin reconfigure   # 没生效就注销重登
```

设置界面：`xime-setup`。首次运行时 daemon 会把 `/usr/share/xime/rime-data`
部署到 `~/.config/xime/rime/`。

## 从源码构建

三种格式共用同一个安装布局脚本 `packaging/stage.sh`：

```bash
cargo build --release
./packaging/stage.sh /tmp/xime-stage /usr   # 铺出完整安装树
```

各格式的配方：

```bash
# deb
cargo install cargo-deb && cargo deb -p xime

# rpm（Fedora 上）
dnf install rpm-build cargo rust git gcc gcc-c++ make pkgconf-pkg-config \
            librime-devel libxkbcommon-devel wayland-devel dbus-devel \
            fontconfig-devel freetype-devel clang clang-devel
rpmbuild -bb packaging/rpm/xime.spec --define "_topdir $PWD/rpmbuild"

# Arch
cd packaging/arch && makepkg -si
```

`.deb` 的文件清单在 `crates/xime-pack/Cargo.toml` 的 `[package.metadata.deb]`，
其余格式走 `packaging/stage.sh`。**改安装布局时两处都要改**。

## 版本同步

CI 用 tag 作为版本号，并校验它和 `Cargo.toml` 的 `[workspace.package] version`
一致（不一致直接失败，避免打出版本号错乱的包）。所以发版前需要一起改：

1. `Cargo.toml` 的 `version`
2. `packaging/arch/PKGBUILD` 的 `pkgver`（CI 会覆盖，但仓库里要保持可读可构建）
3. `packaging/rpm/xime.spec` 的 `Version`（同上）

升级 `rime-wubi` 子模块后，还要同步 `PKGBUILD` 里的 `_rime_wubi_commit`。

## 依赖说明

运行期：`librime`、`libxkbcommon`、`libwayland-client`、`hicolor-icon-theme`。
Rime 走 pkg-config（有 `rime.pc` 时）或直接链系统 `librime`；仓库里不带
librime 源码，也不需要 cmake 构建它。

语音识别用到的 `libsherpa-onnx-c-api.so` / `libonnxruntime.so` 从
sherpa-onnx 的预编译包随包安装到 `/usr/share/xime/lib`（daemon 的 rpath 是
`$ORIGIN/../share/xime/lib`）。许可证：sherpa-onnx 为 Apache-2.0，onnxruntime 为 MIT。
不需要语音的打包可以 `XIME_SKIP_SPEECH=1 ./packaging/stage.sh <destdir>`。

## Arch Linux 专项说明

`packaging/arch/PKGBUILD` 可以直接作为 AUR 包使用（`xime` 这个名字在 AUR 还没被占用）：

```bash
cd packaging/arch
makepkg -si                          # 本地构建安装
makepkg --printsrcinfo > .SRCINFO    # 推 AUR 前生成元数据
```

- 依赖：`librime`、`libxkbcommon`、`wayland`（运行时 dlopen）、`dbus`、`bzip2`、
  `gcc-libs`、`hicolor-icon-theme`、`desktop-file-utils`。
- `rime-wubi` 是 git 子模块，makepkg 不会自动初始化，`prepare()` 会单独拉取并
  填回源码树；升级子模块后记得同步 PKGBUILD 里的 `_rime_wubi_commit`。
- 链接器用 GNU ld/bfd（`build()` 里显式 `-fuse-ld=bfd`）：Arch 工具链默认的 lld
  在链接 `sherpa-onnx-sys` build script（zip 的 zstd + ring 静态库）时会漏符号。
- CI 的 Arch job 不只是打个包：它会把生成的 `.pkg.tar.zst` 装进容器，用 `ldd`
  确认 daemon 能按 rpath 找到 `/usr/share/xime/lib` 下的语音库，并检查方案数据、
  DBus 服务、可执行文件都在位（`namcap` 的输出只作为提示）。
- Release 里另外挂一个 `xime-<ver>-aur.tar.gz`，里面是版本号已经对好的
  `PKGBUILD`、`makepkg --printsrcinfo` 生成的 `.SRCINFO` 和 `xime.install`。

### 第一次发布到 AUR

```bash
# 1. 在 aur.archlinux.org 建账号并上传 SSH 公钥，然后克隆空仓库
git clone ssh://aur@aur.archlinux.org/xime.git && cd xime

# 2. 解开 release 里的 AUR 提交物
tar -xzf ~/Downloads/xime-<ver>-aur.tar.gz

# 3. 提交
git add PKGBUILD .SRCINFO xime.install
git commit -m "Initial commit: xime <ver>"
git push
```

之后每次发版只要用新的 `xime-<ver>-aur.tar.gz` 覆盖这三个文件再 push
（改了 `PKGBUILD` 就必须重新生成 `.SRCINFO`，否则 AUR 拒绝）。

## 没有提供的格式

- **Flatpak / Snap**：输入法守护进程必须作为 KWin 的虚拟键盘客户端工作——
  由 KWin 通过 `WAYLAND_SOCKET` 传 fd 进来，再用 `zwp_input_method_v2` 收发按键，
  并通过 DBus(`org.xime.Xime.Controller`) 在两个进程间传递这个 fd。沙箱化的应用
  既拿不到这个 fd，也无法被 KWin 选为虚拟键盘，这不是权限问题而是架构冲突。
- **Alpine (apk)**：语音运行库 sherpa-onnx / onnxruntime 上游只提供 glibc 预编译包，
  musl 下链不上。等语音做成可选特性后才好支持。
- **Nix**：git 依赖 + 子模块需要在 `fetchgit`/`cargoHash` 里固定 hash，
  当前没有 Nix 环境生成，欢迎 PR。

## 已知限制

`.deb` 在 `ubuntu-latest`（glibc 2.39）上编译，因此要求
Ubuntu 24.04+ / Debian 13+ / 同等 glibc 的发行版。更老的发行版需要自己编译。

构建时 `libximecore` 是按 `main` 分支解析的，不是锁定版本：`Cargo.lock` 里的
libximecore crate 在本地被 `.cargo/config.toml` patch 成了路径依赖（没有 git 条目），
CI 上没有这个 patch，cargo 会重新解析并拉取 `main` 的最新提交。这意味着
同一个 tag 在不同时间构建，依赖代码可能不同。需要严格复现的话，得先把
`Cargo.lock` 换成带 git 条目的版本再构建（或在 PKGBUILD/spec 里 `cargo update -p`
固定 revision），目前还没做。
