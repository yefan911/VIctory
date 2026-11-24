## Description
<!-- Provide a clear and concise description of the changes introduced by this pull request. -->

## Motivation & Context
<!-- Why is this change required? What problem does it solve? If applicable, link to relevant issues. -->

## Quality Assurance & Verification
<!-- Describe the testing or validation performed to verify the changes. -->
- [ ] Verified with the workspace Rust toolchain (`cargo check` or `cargo test`)
- [ ] Formatted changed lines with `cargo fmt`

## Asset Policy & Safety Compliance (Mandatory)
<!-- All pull requests must strictly comply with Rule #1 and repository safety rules. -->
- [ ] **Rule #1 (Asset Policy)**: No proprietary game assets, executables, binaries, or long encoded runs are included.
- [ ] Passes `cargo xtask policy`
- [ ] Every `unsafe` block includes an explicit `// SAFETY:` explanation comment
