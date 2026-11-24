//! `cargo xtask wine-test`: the Windows build's tests on a Linux host,
//! cross-compiled with clang-cl/lld-link against an `xwin` splat and run
//! under Wine in a prefix of our own (`target/wine-prefix`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

const TARGET: &str = "x86_64-pc-windows-msvc";

pub struct Options {
    /// The MSVC CRT and Windows SDK, as `xwin splat` lays them out.
    pub xwin: Option<PathBuf>,
    /// A folder holding vkd3d-proton's x64 `d3d12.dll` and `d3d12core.dll`,
    /// put in the prefix in place of Wine's own D3D12.
    pub vkd3d_proton: Option<PathBuf>,
    /// Arguments for `cargo test`; the whole workspace but xtask without.
    pub args: Vec<OsString>,
}

pub fn run(root: &Path, o: Options) -> Result<()> {
    let xwin = match o.xwin {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".xwin"),
    };
    // The cargo calls below run from the repository root; anchor a relative
    // path to the invocation directory first. `absolute` keeps symlinks.
    let xwin =
        std::path::absolute(&xwin).with_context(|| format!("resolving {}", xwin.display()))?;
    ensure!(
        xwin.join("crt").is_dir() && xwin.join("sdk").is_dir(),
        "no xwin splat at {} (make one with `xwin --accept-license splat --output {0}`, \
         which accepts Microsoft's licence for the CRT and SDK)",
        xwin.display()
    );
    let target_dir = root.join("target");
    let prefix = target_dir.join("wine-prefix");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    // cc splits *FLAGS like a shell when CC_SHELL_ESCAPED_FLAGS is set;
    // quote each include so a path with spaces stays one argument.
    let includes = [
        xwin.join("crt/include"),
        xwin.join("sdk/include/ucrt"),
        xwin.join("sdk/include/um"),
        xwin.join("sdk/include/shared"),
    ]
    .map(|dir| shell_quote(&format!("/imsvc{}", dir.display())))
    .join(" ");
    let cflags = format!("--target={TARGET} -Wno-unused-command-line-argument {includes}");
    // Cargo splits RUSTFLAGS on spaces inside paths, so encode the
    // separators. Every cargo call below passes --target, so these flags
    // never reach host build scripts or proc macros.
    let rustflags = [
        xwin.join("crt/lib/x86_64"),
        xwin.join("sdk/lib/um/x86_64"),
        xwin.join("sdk/lib/ucrt/x86_64"),
    ]
    .map(|dir| format!("-Lnative={}", dir.display()))
    .join("\x1f");
    let env: Vec<(&str, OsString)> = vec![
        ("CC_x86_64_pc_windows_msvc", "clang-cl".into()),
        ("CXX_x86_64_pc_windows_msvc", "clang-cl".into()),
        ("AR_x86_64_pc_windows_msvc", "llvm-lib".into()),
        ("CFLAGS_x86_64_pc_windows_msvc", cflags.clone().into()),
        ("CXXFLAGS_x86_64_pc_windows_msvc", cflags.into()),
        ("CC_SHELL_ESCAPED_FLAGS", "1".into()),
        (
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
            "lld-link".into(),
        ),
        ("CARGO_ENCODED_RUSTFLAGS", rustflags.into()),
        ("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER", "wine".into()),
        ("WINEPREFIX", prefix.clone().into()),
    ];
    let command = |program: &OsString| {
        let mut c = Command::new(program);
        c.current_dir(root).envs(env.iter().map(|(k, v)| (k, v)));
        c
    };

    // Wine makes the prefix on first use; D3D12 goes into it after that.
    let system32 = prefix.join("drive_c/windows/system32");
    if !system32.is_dir() {
        let status = command(&"wineboot".into())
            .arg("-i")
            .status()
            .context("running wineboot (is Wine installed?)")?;
        ensure!(status.success(), "wineboot failed");
    }
    let mut overrides = None;
    if let Some(dir) = o.vkd3d_proton {
        for dll in ["d3d12.dll", "d3d12core.dll"] {
            // A copy keeps the source's permissions (read-only in Proton),
            // so the one from an earlier run is removed, not overwritten.
            let (from, to) = (dir.join(dll), system32.join(dll));
            let _ = std::fs::remove_file(&to);
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} into the prefix", from.display()))?;
        }
        overrides = Some("d3d12,d3d12core=n");
    }

    // The loader's end-to-end test cannot start cargo from inside Wine:
    // its fixture is built here and named to it.
    let probe_dir = target_dir.join("hle-probe");
    let status = command(&cargo)
        .args(["build", "--quiet", "--target", TARGET, "--manifest-path"])
        .arg(root.join("tests/fixtures/hle-probe/Cargo.toml"))
        .arg("--target-dir")
        .arg(&probe_dir)
        .status()
        .context("running cargo")?;
    ensure!(status.success(), "building hle-probe failed");
    let probe = probe_dir.join(TARGET).join("debug").join("hle_probe.dll");

    let mut test = command(&cargo);
    test.args(["test", "--target", TARGET])
        .env("VICTORY_HLE_PROBE", wine_path(&probe))
        .env(
            "WINEDEBUG",
            std::env::var_os("WINEDEBUG").unwrap_or_else(|| "-all".into()),
        );
    if let Some(overrides) = overrides {
        // vkd3d-proton writes its pipeline cache to the working directory
        // (the crate's, in tests), which the asset policy refuses.
        test.env("WINEDLLOVERRIDES", overrides).env(
            "VKD3D_SHADER_CACHE_PATH",
            wine_path(&target_dir.join("vkd3d-proton-cache")),
        );
        std::fs::create_dir_all(target_dir.join("vkd3d-proton-cache"))?;
    }
    if o.args.is_empty() {
        // xtask is a host tool (it runs git); it is not tested under Wine.
        test.args(["--workspace", "--exclude", "xtask", "--no-fail-fast"]);
    } else {
        test.args(&o.args);
    }
    let status = test.status().context("running cargo")?;
    ensure!(status.success(), "tests failed under Wine");
    Ok(())
}

/// `path` as Wine's Z: drive (the host's root) names it.
fn wine_path(path: &Path) -> String {
    format!("Z:{}", path.display()).replace('/', "\\")
}

/// `arg` as one shell word for cc's `CC_SHELL_ESCAPED_FLAGS` parsing.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}
