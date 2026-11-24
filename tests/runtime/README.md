# Synthetic runtime conformance under Wine

This opt-in integration harness builds synthetic x86-64 PE fixtures
from original source and executes them directly under Wine and through
VIctory's Windows loader.

## Requirements

- A Windows x86-64 build of VIctory.
- Wine capable of executing x86-64 Windows programs.
- AVX2 and FMA support in the execution environment.
- Python 3, Bash, clang, lld-link, llvm-dlltool and llvm-readobj.

The compiler and linker tools must be on PATH.

## Running

    bash tests/runtime/run.sh \
      --victory /path/to/victory.exe \
      --wine /path/to/wine

For bundled macOS Wine engines, optional repeated
`--wine-library-dir` arguments and `--wine-framework-dir`
can configure their library search paths.

The harness creates a fresh Wine prefix and temporary laboratory
outside the checkout. Generated executables, DLLs, caches and logs
are never intended for inclusion in the repository.

## Coverage

- PE loading and relocation away from the preferred address.
- Host and Xbox HLE imports.
- Series X and Series S console configurations.
- AVX2 integer arithmetic and FMA floating-point arithmetic.
- Thread creation, events, waits and thread exit codes.
- TLS process/thread callbacks and static thread-local storage.
- Virtual memory allocation and release.
- Reads through the virtual G: title drive.
- Imported and dynamically loaded title DLLs.
- Missing import reporting and zero-return stub behavior.
- Map-only operation and alternate title exit codes.

The runner records individual results and writes a JSON summary.
A failed or blocked case produces a nonzero exit status.

## Initial validation

Apple M5, macOS 27.2.1, Rosetta and Sikarugir Wine 10.0.
Homebrew LLVM/LLD 23.1.6.

The initial local laboratory recorded 37 successful checks:
12 fixture builds, 9 direct execution controls and
16 VIctory loader checks.

No game files or proprietary assets are required.

These results do not establish general title compatibility,
native ARM64 title execution or graphics compatibility.
