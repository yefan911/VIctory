# Cross-building on Arch Linux and checking rendering under Proton

On CachyOS x86-64, VIctory at `d2a4bae` built for Windows MSVC and rendered
through loading on a GeForce RTX 4080 (NVIDIA 620.70.06). Entering the world
triggered a GPU fault. This is one compatibility report. See the
[world-rendering status and GPU hang warning](../README.md#status-2026-11-23)
before running a title.

## Build the launcher

The host had AVX2, Clang/LLVM/LLD, CMake, Ninja, Wine and `file`. Check
`df -h .` and the filesystem holding your SDK cache. This release build used
365 MiB for `target/` and 642 MiB for the x64 SDK/CRT cache, measured
separately. Allow extra space for Rust, Cargo downloads and temporary files.

Use the compiler and target standard library from the **same Rust distribution**.
Combining Arch's Rust 1.98.1 with the official 1.98.1 Windows standard library
failed with `E0514` on `compiler_builtins`, even with matching version numbers.
The official 1.98.1 compiler, host standard library and Windows standard
library worked together. With rustup, run these commands from the checkout
in Bash. Check the [Microsoft license notice](https://github.com/rust-cross/cargo-xwin#cargo-xwin)
before installing or using cargo-xwin.

```bash
repo="$PWD"
target=x86_64-pc-windows-msvc
rustup toolchain install 1.98.1 --profile minimal --target "$target"
cargo +1.98.1 fetch --locked --target "$target"
cargo +1.98.1 install --locked --version 0.24.1 cargo-xwin
export XWIN_ARCH=x86_64
export XWIN_CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/victory-xwin-x64"
cargo +1.98.1 xwin build --release --locked --target "$target" -p victory
exe="$repo/target/$target/release/victory.exe"
file "$exe"
du -sh "$repo/target" "$XWIN_CACHE_DIR"
```

`file` should report a PE32+ x86-64 console executable. If Cargo fails to
resolve crates.io, run `curl -fsS https://index.crates.io/config.json` in the
build shell and your normal terminal. In this test the terminal had network
access and could fetch the locked dependencies.

## Check the CLI and game path

System Wine 11.21 ran `--help` and `config` successfully in a dedicated
prefix. It later opened the title's archives and displayed a white window.
`D3D12CreateDevice` at feature level 12.0 returned `0x80070057`, and VIctory
logged `drawn=0` as the frame counter advanced.

The working graphics configuration used Steam's Proton Experimental with
vkd3d-proton and DXVK. First check that `vulkaninfo --summary` lists your
GPU. The example continues in the same Bash shell and uses a separate prefix.
Adjust the Steam library path if needed. This Proton build used
`SteamLinuxRuntime_4`; its `toolmanifest.vdf` identifies the runtime app ID.

```bash
game='/absolute/path/to/your/game'
work="${XDG_DATA_HOME:-$HOME/.local/share}/victory-proton-test"
steam="$HOME/.local/share/Steam"
common="$steam/steamapps/common"
runtime="$common/SteamLinuxRuntime_4/_v2-entry-point"
proton="$common/Proton - Experimental/proton"
unset WINEPREFIX VICTORY_GAME_DIR VICTORY_CACHE_DIR VICTORY_DATA_DIR VICTORY_TEST_TITLES
export STEAM_COMPAT_CLIENT_INSTALL_PATH="$steam"
export STEAM_COMPAT_DATA_PATH="$work/compatdata"
export STEAM_COMPAT_INSTALL_PATH="$repo"
export STEAM_COMPAT_MOUNTS="$repo:$game:$work${STEAM_COMPAT_MOUNTS:+:$STEAM_COMPAT_MOUNTS}"
mkdir -p "$STEAM_COMPAT_DATA_PATH"
cd "$work"
slr() { "$runtime" --verb=run -- "$proton" "$@"; }
slr run cmd.exe /d /c ver
export WINEDLLOVERRIDES='d3d12,d3d12core,dxgi=n'
slr runinprefix "$exe" --help
slr getcompatpath "$game"
```

For a fresh prefix, copy `"$repo/victory.example.toml"` to
`"$STEAM_COMPAT_DATA_PATH/pfx/drive_c/victory.toml"`. Set `[paths].game_dir`
to the Windows path printed by `getcompatpath` (Proton's `winepath -w`).
Use a single-quoted TOML string to preserve backslashes. Keep the game files
outside the checkout and run the following check before launching.

```bash
export VICTORY_CONFIG='C:\victory.toml'
slr runinprefix "$exe" config
```

Check the printed config filename and game directory. If Proton returns zero
with no console output, redirect inside the prefix and read the file on the
host. This example only reads configuration.

```bash
exe_win=$(slr getcompatpath "$exe" | tr -d '\r')
printf '@echo off\r\n"%s" config > C:\\victory-config.txt 2>&1\r\n' "$exe_win" \
    > "$STEAM_COMPAT_DATA_PATH/pfx/drive_c/victory-config.cmd"
slr runinprefix cmd.exe /d /c 'C:\victory-config.cmd'
cat "$STEAM_COMPAT_DATA_PATH/pfx/drive_c/victory-config.txt"
```

## Distinguish a white window from a world-loading crash

With `runinprefix`, specifying the native DLL overrides above made the
difference in the device probe. The loader trace showed native `d3d12.dll`,
`d3d12core.dll` and `dxgi.dll`. A standalone probe then created the feature
level 12.0 device, direct queue, fence, event and both descriptor heaps.
`--help` and `config` alone do not exercise these calls.

For an intentional title run, read the executable name from the game's
manifest (such as `MicrosoftGame.config`). Keep the same shell, prefix and
overrides. Substitute the manifest's relative executable path below.

```bash
title_win=$(slr getcompatpath "$game/relative/path/from/manifest.exe" | tr -d '\r')
slr runinprefix "$exe" run "$title_win" --execute
```

In the Proton run, `drawn` and `dispatched` rose with the submitted work.
At frame 4096 all 56,069 draws and 2,884 dispatches in the reporting interval
had executed. During the transition to the world, two compute pipelines
were refused. The Linux kernel then logged NVIDIA Xid 13 (GPU MMU fault) for
the title process, and VIctory recorded a null write (`0xc0000005`) in
`crashes.log`. The logs do not identify the GPU command that faulted.

To compare a run, match its timestamps against
`journalctl -k --utc -g 'NVRM: Xid'` and the title's `crashes.log` under
`%LOCALAPPDATA%\victory\data` in the same prefix. For this build, successful
menu and loading-screen rendering is the last verified milestone.

## Opening cutscene follow-up (2026-11-24)

These runs used a locally modified build based on `d2a4bae`, on the same RTX 4080. The user reached two cutscenes with audio, subtitles and HUD elements. They saw a few isolated frames with sky, silhouettes or street geometry. Most of the 3D image remained black. Continuous world rendering and playable gameplay are still unverified.

| Run or check | Result |
| --- | --- |
| Loading and world probes | Four representative world graphics pairs and four compute pipelines were created on the device. A later check created seven scene graphics pairs, including the three affected by the cube-gradient change. Those checks predate the latest depth/gather changes. |
| Busy-frame capture | Frame 1471 had a black final output. Frame 1480 showed subtitles. Intermediate targets contained geometry and depth. In frame 1471, 784 of 948 geometry-buffer operations were dropped. A submitted draw count alone overstates how much of the scene reached the screen. |
| Later world runs | Two runs reached mission streaming and then hit a title breakpoint at image offset `+0x63966f` after roughly 16 and 13½ minutes. Correcting GPU copy-back handling did not eliminate that failure. |
| Latest software checks | The revised shader code translated six captured compute programs offline for wave32. A CPU texture-sampling model passed 280 synthetic gather cases. Native Rust tests passed (27 GPU-crate tests and 81 shader-crate tests). Windows release and diagnostic executables built. GPU correctness and the effect on the cutscene remain open. |
| Latest startup failure | The standalone scene check printed GLX `BadValue` at `X_GLXCreateContext` and was interrupted before producing results. A separately started title bound its imports, printed the same GLX error, fitted the title window to 1024×576 and took an unhandled `0x80000003` at `+0x19f4f58` during startup. This run never reached a world frame. The logs establish the order of events. The cause of the breakpoint is unresolved. |

### What changed in the experimental build

- **Loading and GPU setup.** Integrated work from upstream PRs #439, #177, #336 and #431, guarded unbound root descriptors and resource retirement, and adapted the indirect argument clamp (#190), packed render-target formats (#89) and bounded shader buffer views (#493).
- **World geometry.** Ported shader and descriptor work from #307, #309 and #180, plus selected changes from #566, #437, #136, #318, #539 and #251 for per-lane buffer selection, sampling, atomics and descriptor allocation. The Proton wrapper selected native D3D12/DXGI and disabled `VK_NV_raw_access_chains` for these experiments.
- **Tracing and capture.** Shared live texture tables (#562), captured final and intermediate targets on busy frames (#638), and traced recorded and dropped operations by attachment (#591). Paired captures made it possible to distinguish geometry reaching an intermediate target from geometry reaching the presented image.
- **Latest local changes.** Adapted copy-back tracking and deduplication (#261 and #440), cube gradients (#563), explicit-mip texture gathers (selected work from #160), and depth/stencil view format and plane selection (#651). The six sampled compute programs accounted for 92 and 58 dropped dispatches in the two captured frames before the gather change. Rectangle draws also appear in the traces. #282 and #346 are candidates for investigation.

### How to compare the next run

1. In the same graphical login used for Proton, check `glxinfo -B`, `vulkaninfo --summary` and the display mode. Record the GLX renderer, Vulkan device and dimensions. An earlier native Vulkan enumeration identified the RTX 4080 even when Wine could not create a D3D12 device.
2. Run the standalone scene probe first. On the latest local build the acceptance criteria are six compute pipelines, seven graphics pairs and a passing depth-view test. An exit code alone is insufficient if the expected output is missing.
3. Once the probe succeeds, compare a normal run at the first cutscene with the same build and prefix. Record consecutive frames with visible world geometry alongside recorded, dropped and executed operations. If the output stays black, trace the first pass where a populated intermediate target stops contributing to the presented image. Compare startup and streaming breakpoints separately.

The compatibility changes and title-derived probe inputs were local experiments. This PR provides the build guide and field results.

Upstream reference: [VIctory pull requests](https://github.com/Marvisak/VIctory/pulls).
