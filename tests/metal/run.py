"""Run synthetic VIctory shaders on Apple Silicon through Metal."""

import argparse
import hashlib
import json
import os
import platform
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--victory", required=True, type=Path)
parser.add_argument("--candidate", type=Path)
parser.add_argument("--expect-candidate-no-int64", action="store_true")
args = parser.parse_args()

if args.expect_candidate_no_int64 and args.candidate is None:
    parser.error("--expect-candidate-no-int64 requires --candidate")

if sys.platform != "darwin" or platform.machine() != "arm64":
    parser.error("A native Apple Silicon macOS environment is required")

source = Path(__file__).resolve().parent
repo = source.parents[1]

victory = args.victory.resolve()
candidate = args.candidate.resolve() if args.candidate else None

for binary in (victory, candidate):
    if binary is not None and not (
        binary.is_file() and os.access(binary, os.X_OK)
    ):
        parser.error(f"Executable not found: {binary}")

converter = shutil.which("metal-shaderconverter")

if not converter:
    parser.error("Install Apple's Metal Shader Converter first")

root = Path(tempfile.mkdtemp(
    prefix="victory-metal-",
    dir="/tmp"
)).resolve()

if repo in root.parents:
    raise RuntimeError("Generated data must be outside the repository")

results = root / "results"
results.mkdir()

env = os.environ.copy()
env.pop("VICTORY_PAIRS", None)
env.pop("VICTORY_EMIT_DUMP", None)
env["MTL_DEBUG_LAYER"] = "1"
env["MTL_SHADER_VALIDATION"] = "1"

summary = {}
metadata = {
    "platform": platform.platform(),
    "victory_sha256": hashlib.sha256(
        victory.read_bytes()
    ).hexdigest(),
    "converter": converter,
}

if candidate:
    metadata["candidate_sha256"] = hashlib.sha256(
        candidate.read_bytes()
    ).hexdigest()

def record(label, status):
    if label in summary:
        raise RuntimeError(f"Duplicate result: {label}")
    summary[label] = status

def run(label, command, extra=None):
    print(f"\n=== {label} ===", flush=True)

    try:
        p = subprocess.run(
            list(map(str, command)),
            cwd=root,
            env={**env, **(extra or {})},
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            errors="replace",
            timeout=120,
        )
        rc, output = p.returncode, p.stdout

    except subprocess.TimeoutExpired as exc:
        rc = 124
        output = (
            exc.stdout.decode(errors="replace")
            if isinstance(exc.stdout, bytes)
            else exc.stdout or ""
        ) + "\nTIMEOUT"

    except OSError as exc:
        rc, output = 1, str(exc)

    (results / f"{label}.log").write_text(output)
    print(output, flush=True)
    return rc, output

def features(path):
    data = path.read_bytes()

    if len(data) < 32 or data[:4] != b"DXBC":
        raise ValueError("Invalid DXBC header")

    total, count = struct.unpack_from("<II", data, 24)

    if total != len(data) or 32 + 4 * count > total:
        raise ValueError("Invalid DXBC size")

    for i in range(count):
        offset = struct.unpack_from("<I", data, 32 + 4*i)[0]

        if offset < 32 + 4*count or offset + 8 > total:
            raise ValueError("Invalid part offset")

        kind, size = struct.unpack_from("<4sI", data, offset)

        if offset + 8 + size > total:
            raise ValueError("Invalid part size")

        if kind == b"SFI0":
            if size < 8:
                raise ValueError("Truncated SFI0")
            return struct.unpack_from("<Q", data, offset+8)[0]

    raise ValueError("Missing SFI0")

def translate(build, binary, name):
    label = f"{build}-{name}"
    folder = results / label
    folder.mkdir()

    rc, output = run(
        label + "-translate",
        [
            binary, "translate",
            root / "input" / f"{name}.dxbc",
            "--jobs", "1", "--list"
        ],
        {"VICTORY_EMIT_DUMP": str(folder)}
    )

    files = list(folder.glob("*.dxil"))

    if rc != 0 or len(files) != 1:
        record(label + "-translation", "FAIL")
        record(label + "-conversion", "BLOCKED")
        return None

    dxil = files[0]

    try:
        flag = features(dxil)
    except (ValueError, struct.error) as exc:
        print(exc)
        record(label + "-translation", "FAIL")
        record(label + "-conversion", "BLOCKED")
        return None

    print(f"SFI0: 0x{flag:x}")
    print("INT64 required:", bool(flag & 0x8000))
    record(label + "-translation", "PASS")

    metal = folder / "shader.metallib"
    reflection = folder / "reflection.json"

    rc, _ = run(
        label + "-convert",
        [
            converter, dxil, "-o", metal,
            "--output-reflection-file", reflection
        ]
    )

    if rc != 0 and name != "compute":
        metal.unlink(missing_ok=True)

        rc, _ = run(
            label + "-convert-basic",
            [converter, dxil, "-o", metal]
        )

    if rc != 0 or not metal.is_file() or not metal.stat().st_size:
        record(label + "-conversion", "FAIL")
        return None

    record(label + "-conversion", "PASS")
    return metal, reflection, flag

def render(label, name, width, height, vertex, fragment):
    if vertex is None or fragment is None:
        record(label, "BLOCKED")
        return None

    output = results / f"{label}.rgba"

    rc, log = run(
        label,
        [
            root / "gpu-probe", "render",
            name, width, height, vertex, fragment, output
        ]
    )

    passed = (
        rc == 0
        and f"PASS: {name}" in log
        and output.is_file()
        and output.stat().st_size == width * height * 4
    )

    record(label, "PASS" if passed else "FAIL")

    if passed:
        digest = hashlib.sha256(output.read_bytes()).hexdigest()
        print("RGBA SHA256:", digest)
        return output

    return None

def compare(label, a, b):
    if a is None or b is None:
        record(label, "BLOCKED")
        return

    record(
        label,
        "PASS" if a.read_bytes() == b.read_bytes() else "FAIL"
    )

def find_thread_size(obj):
    if isinstance(obj, dict):
        if "tg_size" in obj:
            return obj["tg_size"]

        for val in obj.values():
            found = find_thread_size(val)
            if found is not None:
                return found

    elif isinstance(obj, list):
        for val in obj:
            found = find_thread_size(val)
            if found is not None:
                return found

    return None

def finish():
    print("\n========== FINAL RESULTS ==========")

    for label, status in summary.items():
        print(f"{status:8} {label}")

    counts = {
        status: list(summary.values()).count(status)
        for status in ("PASS", "FAIL", "BLOCKED")
    }

    for status, count in counts.items():
        print(f"{status}: {count}")

    report = {
        "environment": metadata,
        "counts": counts,
        "checks": summary,
    }

    (results / "summary.json").write_text(
        json.dumps(report, indent=2) + "\n"
    )

    print("LAB:", root)
    print("REPORT:", results / "summary.json")

    return int(counts["FAIL"] > 0 or counts["BLOCKED"] > 0)

print("LAB:", root)

rc, version = run("converter-version", [converter, "--version"])
metadata["converter_version"] = version.strip()

rc, version = run("xcode-version", ["xcodebuild", "-version"])
metadata["xcode_version"] = version.strip()

rc, _ = run(
    "generate-fixtures",
    [sys.executable, source / "generate.py", root]
)

record("generate-fixtures", "PASS" if rc == 0 else "FAIL")

if rc != 0:
    sys.exit(finish())

rc, _ = run(
    "build-probe",
    [
        "xcrun", "swiftc", "-O",
        "-framework", "Metal",
        "-framework", "Foundation",
        source / "probe.swift",
        "-o", root / "gpu-probe"
    ]
)

record("build-probe", "PASS" if rc == 0 else "FAIL")

if rc != 0:
    sys.exit(finish())

render(
    "native-control", "control",
    16, 16, "native", "none"
)

shaders = {}

for name in (
    "red", "gradient", "checker", "select",
    "vertex", "perm_a", "perm_b", "compute"
):
    shaders[name] = translate("baseline", victory, name)

def library(name):
    item = shaders[name]
    return item[0] if item is not None else None

vertex = library("vertex")

for name in ("red", "gradient", "checker", "select"):
    sizes = ((64, 32),) if name == "select" else (
        (16, 16), (32, 24)
    )

    for w, h in sizes:
        a = render(
            f"{name}-native-vs-{w}x{h}",
            name, w, h, "native", library(name)
        )

        b = render(
            f"{name}-victory-vs-{w}x{h}",
            name, w, h, vertex, library(name)
        )

        compare(f"{name}-vertex-equivalence-{w}x{h}", a, b)

for name in ("perm_a", "perm_b"):
    a = render(
        f"baseline-{name}-gpu",
        name, 256, 4, "native", library(name)
    )

    b = render(
        f"baseline-{name}-victory-vs",
        name, 256, 4, vertex, library(name)
    )

    compare(f"{name}-vertex-equivalence", a, b)

    if candidate is not None:
        result = translate("candidate", candidate, name)

        if args.expect_candidate_no_int64:
            record(
                f"{name}-candidate-no-int64",
                "BLOCKED" if result is None else (
                    "PASS" if not result[2] & 0x8000 else "FAIL"
                )
            )

        c = render(
            f"candidate-{name}-gpu",
            name, 256, 4, "native",
            result[0] if result is not None else None
        )

        compare(f"{name}-bit-exact-ab", a, c)

cs = shaders["compute"]

if cs is None:
    record("compute-gpu", "BLOCKED")
else:
    metal, reflection, _ = cs

    try:
        size = find_thread_size(
            json.loads(reflection.read_text())
        )

        print("Compute reflection:", size)

        if size != [64, 1, 1]:
            raise ValueError(f"Unexpected threadgroup size: {size}")

    except (ValueError, OSError) as exc:
        print("Compute reflection unavailable:", exc)
        record("compute-gpu", "BLOCKED")

    else:
        rc, output = run(
            "compute-gpu",
            [root / "gpu-probe", "compute", metal, size[0]]
        )

        record(
            "compute-gpu",
            "PASS" if rc == 0
            and "PASS: compute GPU readback" in output
            else "FAIL"
        )

# Negative control: a red shader must fail the checkerboard oracle.
# This exercises the actual mismatch path rather than just exit status.
red = library("red")

if red is None:
    record("negative-control", "BLOCKED")
else:
    rc, output = run(
        "negative-control",
        [
            root / "gpu-probe", "render", "checker",
            16, 16, "native", red,
            results / "negative-control.rgba"
        ]
    )

    record(
        "negative-control",
        "PASS" if rc == 1
        and "FAIL: Pixel readback mismatch" in output
        else "FAIL"
    )

sys.exit(finish())
