# VIctory on RTX 4060 — Story Mode validation log

[Versão em português](rtx4060-story-validation.pt-BR.md)

**Status: partial compatibility progress, not playable in Story Mode.** This is a hardware validation report, not a new upstream fix or a binary release. Two end-to-end runs and a later synthetic shader experiment are reported separately.

## Test setup

- Windows 11, NVIDIA GeForce RTX 4060, D3D12 hardware rendering, WARP disabled for title runs. The Windows GPU performance counter showed 3D-engine activity for the baseline title process.
- Xbox Series S profile, 1920×1080 output, custom host-owned presentation window, mesh Wave64 skip enabled, and DRED requested. The report branch is based on upstream `ffe310e`; the local integration used for validation was based on `8f5e08a` and already contained local presentation and Wave64 changes. The report branch does not ship those changes.
- The texture build ported the relevant GPU-format changes from upstream PR [#1076](https://github.com/Marvisak/VIctory/pull/1076), specifically `e426feb` and `44cbed0`, onto that integration. It did not replace the integration with the full fork.

## Run log — 2026-11-24 (UTC)

| Event | Baseline GPU run | Texture compatibility run |
|---|---|---|
| First `Present` returning `S_OK` | 16:38:32, 1920×1080 | 16:47:43, 1920×1080 |
| Host texture creation failures | 4, all `0x80070057` | 0 |
| Last periodic `frame presented` marker | 1024 | 2048 |
| Transitions to black reported by the probe | Frames 1, 917, 1638, 1640 | Frames 1, 3542, 3852, 3854 |
| `shader does not translate` warnings | 292 | 249 |
| `a descriptor read only as data` warnings | 166 | 129 |
| Wave64-in-groups-over-64 refusal warnings | 26 | 26 |
| Terminal GPU failure | 16:41:54, `DXGI_ERROR_DEVICE_HUNG (0x887A0006)` | 16:55:47, `DXGI_ERROR_DEVICE_HUNG (0x887A0006)` |

- Baseline D3D12 texture refusals were `(DXGI format, flags)`: `(67, 5)`, `(67, 4)`, `(86, 5)`, `(115, 1)`. They were absent in the texture compatibility run. This does not imply every format/usage pair is supported or that texture contents are correct.
- The corrected runtime displayed menu prompts and opening artwork; it advanced to subtitles during the opening story sequence. The scene underneath was still black during the observed transition. Neither a correctly rendered 3D world nor an on-screen controllable character was verified.
- The last regular presentation marker is not the final frame: the logger emits milestones, and the black-frame warnings show later processing. The warning counts are counts of log lines, not an offline shader census. These are single runs, with different build profiles and frame trajectories; differences in warning totals or frame numbers are not proof of a performance improvement or of a change in translator coverage.
- Both logs end with a device-removal diagnostic after a submission reported as containing one command list. This shows the failure was still present; it does not identify a root-cause shader, and splitting a multi-list batch alone would not explain this observation. The baseline initially appeared healthy in an early log sample; the complete log shows the later failure.
- This is a sanitized event log and test record. No raw runtime logs, memory addresses, shader dumps, game screenshots, game files, local configuration, or credentials are included. See [asset policy](asset-policy.md).

## Reproduction and test results

1. In a Windows source checkout, leave `VICTORY_D3D12_WARP` unset and use the hardware adapter. Build with `cargo build --locked --profile diag -p victory --features diagnostics`. Launch with a locally configured, user-supplied title executable, `--execute --console series-s`, and `VICTORY_D3D12_DRED=1`. Use the local integration's 1080p/window settings when matching these runs. Log the startup, story transition and device-removal outcome separately.
2. In the local texture integration, run `cargo test --locked --profile diag -p victory-gpu --features diagnostics -- --test-threads=1`. The new allocation regression failed before the port at format 67/flags 5. It passed after the port; GPU suite: **75 passed, 0 failed, 1 ignored**. On the local integration, attempting the GPU test build without `--features diagnostics` hit `E0560` for a `Shader.wave64` field; that is a separate test-build configuration issue, not a GPU runtime result.
3. The local integration already passed the named-descriptor test from [#1067](https://github.com/Marvisak/VIctory/pull/1067). A new synthetic variant that removes the view's named range failed with `a descriptor read only as data`. An experimental fallback permitted only full eight-dword unnamed view descriptors, treated as 2D SRVs; the synthetic test then passed, the shader suite reported **193 passed, 0 failed, 1 ignored**, and the diagnostic executable built. This experiment has **not** been validated against the title. It does not establish correctness for cube, 3D, array or unordered-access views, or a fix for the World/Story Mode hang.

## Avoiding duplicate work

| Existing work | What this report contributes |
|---|---|
| [#1076](https://github.com/Marvisak/VIctory/pull/1076), [#1092](https://github.com/Marvisak/VIctory/pull/1092), [#735](https://github.com/Marvisak/VIctory/issues/735): unsupported texture formats and NVIDIA compatibility | Independent hardware regression and a Series S run showing four allocation refusals eliminated, but the device hang remains. No duplicate texture patch is published here. |
| [#1067](https://github.com/Marvisak/VIctory/pull/1067), [#1130](https://github.com/Marvisak/VIctory/pull/1130): descriptor-only shader reads and descriptor-kind recovery | Synthetic validation of one unnamed 2D case. Compare the existing approaches before proposing code; the overlapping local experiment is intentionally not published here. |
| [#439](https://github.com/Marvisak/VIctory/pull/439), [#1132](https://github.com/Marvisak/VIctory/issues/1132): Wave64 translation and comparisons | The over-64-thread refusal remains visible; do not treat the local texture result as a Wave64 fix. |
| [#1133](https://github.com/Marvisak/VIctory/pull/1133), [#1081](https://github.com/Marvisak/VIctory/pull/1081): capped heaps, narrow-wave barriers and shader breadcrumbs | Next comparison: repeat on GPU with the same settings and use DRED/breadcrumbs to isolate the failing pass. A reported improvement is not evidence that these changes remove this hang. |

**Acceptance still open:** on the hardware GPU, render the 3D scene and character, verify a gameplay input moves the character, and continue beyond the opening transition without device removal. No tested build has met these criteria.
