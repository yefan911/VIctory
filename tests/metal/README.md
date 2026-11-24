# Experimental Apple Silicon shader conformance

This is a standalone GPU test harness, not a Metal runtime backend.

It generates small synthetic GFX10 shader containers, translates them
through the VIctory CLI, converts the emitted DXIL with Apple's Metal
Shader Converter, executes the resulting libraries on Metal, and
checks GPU readback against CPU reference values.

## Requirements

- Apple Silicon Mac with Metal support.
- Xcode with the macOS SDK and Swift compiler.
- Python 3.
- Apple's Metal Shader Converter, installed separately:
  https://developer.apple.com/metal/shader-converter/
- A working native ARM64 VIctory executable.

The native CLI build requires the non-Windows workspace portability
changes proposed in PR #235. When those changes are available in your
checkout, build the CLI using:

    cargo build --locked --release -p victory

## Run

    python3 tests/metal/run.py --victory /path/to/victory

Optional A/B comparison with another build:

    python3 tests/metal/run.py \
      --victory /path/to/baseline/victory \
      --candidate /path/to/candidate/victory

For testing the INT64 removal in PR #423, additionally pass:

    --expect-candidate-no-int64

## Coverage

- Native Metal control.
- Constant pixel color.
- Position-dependent floating-point gradients.
- Integer conversions and bitwise checkerboard.
- Conditional selection.
- Dynamic byte permutation with two operand pairs.
- All 256 selector values in each of the four output bytes.
- Vertex ID and translated vertex positions.
- Native versus translated vertex pipeline output comparison.
- Compute dispatch with a writable raw buffer.
- 64 expected compute results and 16 trailing guard values.
- Negative control that intentionally fails a pixel comparison.

The gradient comparison allows one RGBA8 quantization step.
Other RGBA8 comparisons require exact channel values.
A/B image comparisons require byte-for-byte equality.

The compute probe uses the descriptor binding layout observed with
the tested converter and synthetic raw-buffer shader. This is not
a general implementation of the converter's resource binding ABI.

## Results and generated files

The runner uses a fresh temporary directory outside the checkout.
It keeps logs, reflection data, synthetic containers, GPU readbacks
and a JSON summary there. Its location is printed at the end.

Every required check must pass for the runner to exit successfully.
Failed or blocked checks produce a nonzero exit status.

No game files are required. No generated binaries, shader dumps,
images, proprietary assets or converter components belong in Git.

## Initial development environment

Apple M5, macOS 27.2.1, Rust 1.98.1, Xcode 27.0,
macOS SDK 27.0, Metal Shader Converter 4.0.3.

The initial exploratory labs successfully executed translated
vertex, pixel and compute shaders on this hardware.

This harness does not establish compatibility with arbitrary shaders,
Xbox title execution, mesh shaders, textures, samplers or a complete
Metal graphics backend.
