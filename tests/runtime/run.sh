#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)

python3 -E -u - "$repo_root" "$@" <<'PYTHON_RUNTIME'
import os
import sys
import argparse
import tempfile
import re
import json
import hashlib
import subprocess
import time
from pathlib import Path

parser = argparse.ArgumentParser(
    description="Synthetic VIctory runtime conformance under Wine"
)
parser.add_argument("--victory", required=True, type=Path)
parser.add_argument("--wine", required=True, type=Path)
parser.add_argument("--wine-library-dir", action="append", default=[])
parser.add_argument("--wine-framework-dir")
args = parser.parse_args(sys.argv[2:])

repo = Path(sys.argv[1]).resolve()
base = Path(tempfile.gettempdir()).resolve()

if base == repo or repo in base.parents:
    base = Path("/tmp").resolve()

if base == repo or repo in base.parents:
    raise SystemExit("Temporary directory must be outside checkout")

root = Path(tempfile.mkdtemp(
    prefix="victory-runtime-", dir=base
)).resolve()

title = root / "title"
logs = root / "logs"

for directory in (title, logs, root / "cache", root / "data"):
    directory.mkdir()

wine = args.wine.expanduser().absolute()
victory = args.victory.expanduser().resolve()

print("Laboratory:", root, flush=True)

assert wine.is_file()
assert victory.is_file()

results = []

def record(name, status, detail=""):
    results.append({
        "test": name,
        "status": status,
        "detail": detail
    })
    print(f"{status}: {name} {detail}", flush=True)

def build(args):
    subprocess.run(
        list(map(str, args)),
        cwd=title,
        check=True
    )

def import_lib(name, exports):
    definition = title / f"{name}.def"
    definition.write_text(
        f"LIBRARY {name}.dll\nEXPORTS\n" +
        "".join(f"    {x}\n" for x in exports)
    )
    build([
        "llvm-dlltool", "-m", "i386:x86-64",
        "-d", definition.name,
        "-l", f"{name}.lib"
    ])

import_lib("kernel32", [
    "GetCurrentProcessId",
    "GetCurrentThreadId",
    "CreateThread",
    "WaitForSingleObject",
    "GetExitCodeThread",
    "CloseHandle",
    "CreateEventW",
    "SetEvent",
    "VirtualAlloc",
    "VirtualFree",
    "CreateFileW",
    "ReadFile",
    "LoadLibraryW",
    "GetProcAddress"
])

import_lib("xgameruntime", [
    "XGameRuntimeInitialize",
    "XGameRuntimeUninitialize"
])

import_lib("victory-pr9-missing", [
    "ProbeMissing"
])

source = r'''
typedef unsigned int U32;
typedef unsigned long DWORD;
typedef unsigned long long U64;
typedef unsigned short WCHAR;
typedef void *HANDLE;

#define IMPORT __declspec(dllimport)

IMPORT DWORD GetCurrentProcessId(void);
IMPORT DWORD GetCurrentThreadId(void);

IMPORT HANDLE CreateThread(
    void *, U64, DWORD (*)(void *),
    void *, DWORD, DWORD *);

IMPORT DWORD WaitForSingleObject(HANDLE, DWORD);
IMPORT int GetExitCodeThread(HANDLE, DWORD *);
IMPORT int CloseHandle(HANDLE);

IMPORT HANDLE CreateEventW(void *, int, int, const WCHAR *);
IMPORT int SetEvent(HANDLE);

IMPORT void *VirtualAlloc(void *, U64, DWORD, DWORD);
IMPORT int VirtualFree(void *, U64, DWORD);

IMPORT HANDLE CreateFileW(
    const WCHAR *, DWORD, DWORD, void *,
    DWORD, DWORD, HANDLE);

IMPORT int ReadFile(
    HANDLE, void *, DWORD, DWORD *, void *);

IMPORT HANDLE LoadLibraryW(const WCHAR *);
IMPORT void *GetProcAddress(HANDLE, const char *);

IMPORT long XGameRuntimeInitialize(void);
IMPORT void XGameRuntimeUninitialize(void);

IMPORT U64 ProbeMissing(U64);
IMPORT U32 ProbeValue(void);

static volatile U32 seed = 0x13579bdfu;
static volatile U32 *volatile pointer = &seed;

#ifdef CASE_CPU

int _fltused = 0;
extern void cpu_vectors(U32 *, float *);

static U32 test_cpu(void) {
    U32 out[8];
    float fp[8];

    cpu_vectors(out, fp);

    for (U32 i = 0; i < 8; ++i) {
        if (out[i] != (i + 1) * 3 + 10)
            return 111;
        if (fp[i] != (float)((i + 1) * 2 + 1))
            return 112;
    }

    return 42;
}

#endif

#if defined(CASE_THREAD) || defined(CASE_TLS)

static volatile DWORD main_id;
static volatile DWORD worker_id;
static volatile DWORD shared;
static HANDLE event;

#ifdef CASE_TLS

static volatile U32 process_attach;
static volatile U32 thread_attach;
static volatile U32 thread_detach;

#ifdef STATIC_TLS
__declspec(thread) static volatile U32 local_value = 0x12345678u;
#endif

static void tls_callback(void *module, DWORD reason, void *reserved) {
    (void)module;
    (void)reserved;

    if (reason == 1) {
        if (GetCurrentProcessId() != 0)
            __atomic_add_fetch(&process_attach, 1, __ATOMIC_SEQ_CST);
    }

    if (reason == 2)
        __atomic_add_fetch(&thread_attach, 1, __ATOMIC_SEQ_CST);

    if (reason == 3)
        __atomic_add_fetch(&thread_detach, 1, __ATOMIC_SEQ_CST);
}

#pragma section(".tls", read, write)
#pragma section(".tls$ZZZ", read, write)
#pragma section(".rdata$T", read)

__declspec(allocate(".tls"))
char tls_start = 0;

__declspec(allocate(".tls$ZZZ"))
char tls_end = 0;

DWORD _tls_index = 0;

typedef void (*TLS_CALLBACK)(void *, DWORD, void *);

static TLS_CALLBACK callbacks[] = {
    tls_callback,
    0
};

struct tls_directory {
    const void *start;
    const void *end;
    DWORD *index;
    TLS_CALLBACK *callbacks;
    DWORD zero_fill;
    DWORD characteristics;
};

__declspec(allocate(".rdata$T"))
const struct tls_directory _tls_used = {
    &tls_start,
    &tls_end,
    &_tls_index,
    callbacks,
    0,
    0
};

#endif

static DWORD worker(void *arg) {
    worker_id = GetCurrentThreadId();

    if (worker_id == main_id)
        return 121;

    if (*(U32 *)arg != 0x11223344u)
        return 122;

#ifdef CASE_TLS
    if (__atomic_load_n(&thread_attach, __ATOMIC_SEQ_CST) == 0)
        return 123;
#endif

#ifdef STATIC_TLS
    if (local_value != 0x12345678u)
        return 124;

    local_value = 0xabcdef01u;
#endif

    shared = 0xaabbccddu;

    if (!SetEvent(event))
        return 125;

    return 77;
}

static U32 test_thread(void) {
    DWORD code = 0;
    U32 arg = 0x11223344u;

#ifdef CASE_TLS
    if (process_attach != 1)
        return 126;
#endif

#ifdef STATIC_TLS
    if (local_value != 0x12345678u)
        return 127;

    local_value = 0x76543210u;
#endif

    main_id = GetCurrentThreadId();

    event = CreateEventW(0, 0, 0, 0);
    if (!event)
        return 128;

    HANDLE thread = CreateThread(
        0, 0, worker, &arg, 0, 0
    );

    if (!thread)
        return 129;

    DWORD event_wait = WaitForSingleObject(event, 10000);
    DWORD thread_wait = WaitForSingleObject(thread, 10000);
    int got_code = GetExitCodeThread(thread, &code);

    CloseHandle(thread);
    CloseHandle(event);

    if (got_code && code != 77)
        return code;

    if (event_wait != 0 || thread_wait != 0 || !got_code)
        return 130;

    if (worker_id == 0 || shared != 0xaabbccddu)
        return 131;

#ifdef CASE_TLS
    if (__atomic_load_n(&thread_detach, __ATOMIC_SEQ_CST) == 0)
        return 132;
#endif

#ifdef STATIC_TLS
    if (local_value != 0x76543210u)
        return 133;
#endif

    return 42;
}

#endif

#ifdef CASE_MEMORY

static U32 test_memory(void) {
    volatile U32 *p = VirtualAlloc(
        0, 65536, 0x3000, 4
    );

    if (!p)
        return 141;

    for (U32 i = 0; i < 16384; ++i)
        p[i] = i ^ 0x55aa55aau;

    for (U32 i = 0; i < 16384; ++i)
        if (p[i] != (i ^ 0x55aa55aau))
            return 142;

    if (!VirtualFree((void *)p, 0, 0x8000))
        return 143;

    return 42;
}

#endif

#ifdef CASE_FILES

static U32 test_files(void) {
    HANDLE h = CreateFileW(
        L"G:\\pr9-input.txt",
        0x80000000u, 1, 0, 3, 0x80, 0
    );

    if (h == (HANDLE)(U64)-1)
        return 151;

    char out[16];
    DWORD n = 0;

    int ok = ReadFile(h, out, 16, &n, 0);
    CloseHandle(h);

    if (!ok || n != 8)
        return 152;

    const char expected[] = "VICTORY!";

    for (U32 i = 0; i < 8; ++i)
        if (out[i] != expected[i])
            return 153;

    return 42;
}

#endif

U32 probe_entry(void *unused) {
    (void)unused;

    if (!GetCurrentProcessId())
        return 101;

    if (*pointer != 0x13579bdfu)
        return 102;

#ifdef CASE_HLE
    if (XGameRuntimeInitialize() != 0)
        return 103;
    XGameRuntimeUninitialize();
#endif

#ifdef CASE_CPU
    return test_cpu();
#endif

#if defined(CASE_THREAD) || defined(CASE_TLS)
    return test_thread();
#endif

#ifdef CASE_MEMORY
    return test_memory();
#endif

#ifdef CASE_FILES
    return test_files();
#endif

#ifdef CASE_DLL
    if (ProbeValue() != 0x1234)
        return 161;
#endif

#ifdef CASE_LATE
    HANDLE module = LoadLibraryW(L"pr9-title.dll");
    if (!module)
        return 162;

    U32 (*f)(void) =
        (U32 (*)(void))GetProcAddress(module, "ProbeValue");

    if (!f || f() != 0x1234)
        return 163;
#endif

#ifdef CASE_MISSING
    if (ProbeMissing(0x11223344u) != 0)
        return 171;
#endif

#ifdef CASE_NEGATIVE
    return 43;
#endif

    return *pointer ^ 0x13579bf5u;
}
'''

assembly = r'''
.intel_syntax noprefix
.text
.globl cpu_vectors
cpu_vectors:
    vmovdqu ymm0, ymmword ptr [rip + integers]
    vpmulld ymm0, ymm0, ymmword ptr [rip + threes]
    vpaddd ymm0, ymm0, ymmword ptr [rip + tens]
    vmovdqu ymmword ptr [rcx], ymm0

    vmovups ymm0, ymmword ptr [rip + floats]
    vmovups ymm1, ymmword ptr [rip + twos]
    vfmadd213ps ymm0, ymm1, ymmword ptr [rip + ones]
    vmovups ymmword ptr [rdx], ymm0

    vzeroupper
    ret

.section .rdata,"dr"
.p2align 5
integers:
    .long 1,2,3,4,5,6,7,8
threes:
    .long 3,3,3,3,3,3,3,3
tens:
    .long 10,10,10,10,10,10,10,10
floats:
    .float 1,2,3,4,5,6,7,8
twos:
    .float 2,2,2,2,2,2,2,2
ones:
    .float 1,1,1,1,1,1,1,1
'''

dll_source = r'''
static volatile unsigned int attached;

int DllMain(void *module, unsigned long reason, void *reserved) {
    (void)module;
    (void)reserved;

    if (reason == 1)
        attached = 0x1234;

    return 1;
}

__declspec(dllexport)
unsigned int ProbeValue(void) {
    return attached;
}
'''

(title / "probe.c").write_text(source)
(title / "cpu.S").write_text(assembly)
(title / "dll.c").write_text(dll_source)
(title / "pr9-input.txt").write_bytes(b"VICTORY!")

cc = [
    "clang",
    "--target=x86_64-pc-windows-msvc",
    "-O2", "-ffreestanding",
    "-fno-stack-protector"
]

build(cc + ["-c", "cpu.S", "-o", "cpu-vectors.obj"])
build(cc + ["-c", "dll.c", "-o", "dll.obj"])

build([
    "lld-link", "/dll", "/machine:x64",
    "/entry:DllMain", "/nodefaultlib",
    "/out:pr9-title.dll",
    "/implib:pr9-title.lib",
    "dll.obj"
])

cases = {
    "host": [],
    "hle": ["CASE_HLE"],
    "cpu": ["CASE_CPU"],
    "threads": ["CASE_THREAD"],
    "tls": ["CASE_TLS"],
    "static-tls": ["CASE_TLS", "STATIC_TLS"],
    "memory": ["CASE_MEMORY"],
    "files": ["CASE_FILES"],
    "dll": ["CASE_DLL"],
    "late-dll": ["CASE_LATE"],
    "missing": ["CASE_MISSING"],
    "negative": ["CASE_NEGATIVE"]
}

built = set()

for name, defines in cases.items():
    try:
        build(
            cc +
            [f"-D{x}" for x in defines] +
            ["-c", "probe.c", "-o", f"{name}.obj"]
        )

        cmd = [
            "lld-link", "/machine:x64",
            "/entry:probe_entry",
            "/subsystem:console",
            "/nodefaultlib", "/dynamicbase", "/nxcompat",
            f"/out:{name}.exe",
            f"{name}.obj", "kernel32.lib"
        ]

        if name == "cpu":
            cmd += ["cpu-vectors.obj"]

        if name == "hle":
            cmd += ["xgameruntime.lib"]

        if name in ("tls", "static-tls"):
            cmd += ["/include:_tls_used"]

        if name == "dll":
            cmd += ["pr9-title.lib"]

        if name == "missing":
            cmd += ["victory-pr9-missing.lib"]

        build(cmd)

        info = subprocess.check_output([
            "llvm-readobj",
            "--file-headers",
            "--coff-imports",
            "--coff-basereloc",
            "--coff-tls-directory",
            str(title / f"{name}.exe")
        ], text=True)

        (logs / f"build-{name}.log").write_text(info)

        built.add(name)
        record(f"build-{name}", "PASS")

    except subprocess.CalledProcessError as e:
        record(f"build-{name}", "FAIL", str(e))

def winpath(p):
    return "Z:" + str(Path(p).resolve()).replace("/", "\\")

env = os.environ.copy()

if args.wine_library_dir:
    library_dirs = [
        str(Path(p).expanduser().resolve())
        for p in args.wine_library_dir
    ]
    env["DYLD_LIBRARY_PATH"] = ":".join(library_dirs)
    env["DYLD_FALLBACK_LIBRARY_PATH"] = (
        env["DYLD_LIBRARY_PATH"] + ":/usr/lib"
    )

if args.wine_framework_dir:
    env["DYLD_FRAMEWORK_PATH"] = args.wine_framework_dir

env["PATH"] = str(wine.parent) + ":" + env["PATH"]
env["WINEPREFIX"] = str(root / "wine-prefix")
env["WINEDEBUG"] = "-all"
env["MVK_CONFIG_LOG_LEVEL"] = "0"

for key in list(env):
    if key.startswith("VICTORY_"):
        env.pop(key)

env.pop("WINEARCH", None)

env["VICTORY_CACHE_DIR"] = winpath(root / "cache")
env["VICTORY_DATA_DIR"] = winpath(root / "data")
env["VICTORY_LOG"] = "info"

def invoke(name, args, timeout=120):
    print(f"\n=== {name} ===", flush=True)
    start = time.monotonic()

    try:
        p = subprocess.run(
            [str(wine), *map(str, args)],
            cwd=title,
            env=env,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=timeout
        )
    except subprocess.TimeoutExpired:
        record(name, "FAIL", "timeout")
        return None

    output = p.stdout + "\n" + p.stderr

    (logs / f"{name}.log").write_text(output)

    for line in output.splitlines():
        if any(x in line for x in (
            "mapped at",
            "bound ",
            "title exited",
            "XGameRuntime",
            "Error:",
            "panic",
            "unimplemented"
        )):
            print(line)

    elapsed = time.monotonic() - start
    print(f"Process status: {p.returncode}; {elapsed:.2f}s")

    return p, output

def direct(case, expected=42):
    name = f"direct-{case}"

    if case not in built:
        record(name, "BLOCKED", "fixture not built")
        return

    r = invoke(
        name,
        [winpath(title / f"{case}.exe")],
        timeout=180
    )

    if r is None:
        return

    p, output = r

    record(
        name,
        "PASS" if p.returncode == expected else "FAIL",
        f"exit={p.returncode}; expected={expected}"
    )

def loader(case, expected=42, extra=(), execute=True, suffix=""):
    name = f"loader-{case}{suffix}"

    if case not in built:
        record(name, "BLOCKED", "fixture not built")
        return

    args = [
        winpath(victory),
        "run",
        winpath(title / f"{case}.exe")
    ]

    if execute:
        args.append("--execute")

    args.extend(extra)

    r = invoke(name, args)

    if r is None:
        return

    p, output = r

    binding = re.search(
        r"bound\s+" + re.escape(case + ".exe") + r":\s*(\d+) hle,\s*(\d+) host,"
        r"\s*(\d+) title,\s*(\d+) missing",
        output
    )

    errors = []

    if p.returncode != 0:
        errors.append(f"process status {p.returncode}")

    marker = (
        f"title exited with {expected}"
        if execute else "not executing"
    )

    if execute:
        marker_found = marker in [
            line.strip() for line in output.splitlines()
        ]
    else:
        marker_found = marker in output

    if not marker_found:
        errors.append(f"missing {marker!r}")

    if not binding:
        errors.append("missing binding report")
    else:
        hle, host, shipped, missing = map(int, binding.groups())

        if missing != (1 if case == "missing" else 0):
            errors.append(f"unexpected missing imports: {missing}")

        if case == "hle" and (hle, host, shipped) != (2, 1, 0):
            errors.append("unexpected Xbox HLE bindings")

        if case == "dll" and shipped != 1:
            errors.append("title DLL export not bound")

    if case == "hle" and execute:
        for api in (
            "XGameRuntimeInitialize",
            "XGameRuntimeUninitialize"
        ):
            if api not in output:
                errors.append(f"missing runtime log {api}")

    if case == "missing" and execute:
        if "unimplemented import called, returning 0" not in output:
            errors.append("missing stub invocation log")
        if "ProbeMissing" not in output:
            errors.append("missing stub symbol")

    if case == "late-dll" and execute:
        if "bound          pr9-title.dll:" not in output:
            errors.append("missing late DLL binding report")

    if suffix == "-series-s":
        if "Xbox Series S" not in output:
            errors.append("Series S selection not confirmed")

    if not execute and "title exited with" in output:
        errors.append("map-only mode executed the entry point")

    if case == "host" and execute:
        mapped = re.search(
            r"mapped at\s+(0x[0-9a-f]+)\s+\(preferred (0x[0-9a-f]+)\)",
            output
        )
        if not mapped or mapped.group(1) == mapped.group(2):
            errors.append("relocation away from preferred base not verified")

    record(
        name,
        "FAIL" if errors else "PASS",
        "; ".join(errors)
    )

print("\n=== DIRECT WINDOWS CONTROLS ===")

for case in (
    "host", "cpu", "threads",
    "tls", "static-tls",
    "memory", "dll", "late-dll"
):
    direct(case)

print("\n=== VICTORY LOADER TESTS ===")

loader("host", execute=False, suffix="-map")
loader("host")

loader("hle", execute=False, suffix="-map")
loader("hle")

loader(
    "hle",
    extra=("--console", "series-s"),
    suffix="-series-s"
)

for case in (
    "cpu", "threads", "tls",
    "static-tls", "memory", "files",
    "dll", "late-dll"
):
    loader(case)

loader(
    "missing",
    execute=False,
    suffix="-map"
)

loader(
    "missing",
    extra=("--unimplemented", "zero")
)

print("\n=== NEGATIVE CONTROL ===")

direct("negative", expected=43)
loader("negative", expected=43)

print("\n=== FINAL RESULTS ===")

for item in results:
    print(
        f"{item['status']:8} "
        f"{item['test']:32} "
        f"{item['detail']}"
    )

counts = {
    status: sum(x["status"] == status for x in results)
    for status in ("PASS", "FAIL", "BLOCKED")
}

summary = {
    "counts": counts,
    "results": results,
    "victory_sha256": hashlib.sha256(
        victory.read_bytes()
    ).hexdigest(),
    "wine": str(wine)
}

(root / "summary.json").write_text(
    json.dumps(summary, indent=2)
)

print("\n=== SUMMARY ===")

for status, count in counts.items():
    print(f"{status}: {count}")

print("Results:", root / "summary.json")

raise SystemExit(
    1 if counts["FAIL"] or counts["BLOCKED"] else 0
)
PYTHON_RUNTIME
