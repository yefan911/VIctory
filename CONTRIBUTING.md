# Contributing to VIctory

You can help with source code, synthetic tests, documentation, or reproducible
compatibility reports. You do not need game files for the source and test
workflow below.

## Pick a task

Check the open [issues](https://github.com/Marvisak/VIctory/issues) and
[pull requests](https://github.com/Marvisak/VIctory/pulls) for existing work.
For a concrete open issue, comment with the part you plan to tackle before
starting. Keep each PR focused and link the relevant issue.

## Run a small check

Follow the build prerequisites in the [README](README.md). From a clone of
your fork, run the checks that apply to your change:

```powershell
cargo xtask install-hooks
cargo test --locked -p victory-formats
cargo xtask policy
```

The formats suite uses synthetic fixtures and needs no Direct3D device. For
changes in other crates, run their relevant tests and state which checks you
ran in the PR. Run the policy check with the files staged if you have not
committed them yet: `cargo xtask policy --staged`.

On the current main branch, `cargo fmt --all --check` also reports existing
formatting differences. Format only the Rust files you touch so unrelated
changes stay out of the PR.

## Write a useful report

Include the VIctory commit, build profile, Windows version, CPU, GPU and driver.
Give the command or settings needed to reproduce the issue, the last stage
that worked, and the exact error code if available. Replace personal paths
with placeholders. When comparing two builds, use the same settings and give
both commit IDs.

Read the [asset policy](docs/asset-policy.md) before sharing anything. Use
source or synthetic fixtures. Summarize runtime observations in text and
keep game files, captures, dumps, and logs that contain proprietary material
out of issues and PRs.
