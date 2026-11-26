# Operation Zero Warnings — Progress & Failure Census

## Current Status (Cycle 2 Complete)
- **Analyzer:** `python tools/analyze_logs.py <log> --json-out <out.json> [--compare <prev.json>]`
- **Total WARN:** `895` (`-40` vs `ae08fb7` baseline `935`) | **Total ERROR:** `0`
- **Unique Failed Shaders:** `672` (`-40` fully restored since `ae08fb7`)
- **`0x218` Precompiled Compute Telemetry:** `accepted=2689, parsed=2689, rejected=0, host_created=244, host_failed=676, executed=4, dropped=1`
- **Descriptor Heap Truncations (`title slot past the host heap`):** `0` (fixed in `4727289`, default `VICTORY_DYNAMIC_SLOTS=524288`)

## Resolved Compiler & Runtime Deficiencies
1. **`f99ddfb`:** Implemented Xbox D3D12.X `0x218` (`CreatePreCompiledComputePipelineState`) via `FSPX`/`ACCC` + `RootType::Constants`/`ResourceHeap`/`SamplerHeap` (`2,689` descriptors parsed, `0` rejected).
2. **`4727289`:** Fixed NVIDIA 1M descriptor heap partition (`524,288` dynamic / `475,711` direct → `0` truncations, down from `457`) and `scc_some` lane-mask translation (`f701e815650eaa2e` restored & GPU-verified on RTX 4060 Ti).
3. **`9bb7d5f` & `ae08fb7`:** Supported two-term computed addresses (`Address::dynamic2` → `0` remaining `"two computed terms"` errors) and prefix-`dmask` `image_store` (`+39` host PSOs restored: `165 → 204`).
4. **Cycle 1 (`3cc9155`):** `s_pack_ll_b32_b16` (`20 → 0`), `s_pack_hh_b32_b16` (`15 → 0`), `s_bfm_b64` (`4 → 0`), counter-texture `image_store` guard (`8` shaders restored: `712 → 704`).
5. **Cycle 2 (`20318bb`):** `v_mad_u32_u16`/`v_mad_i32_i16` (`5 → 0`), `s_sext_i32_i16`/`s_sext_i32_i8` (`1 → 0`), `s_bitreplicate_b64_b32`, and 1–3 channel typed `bufferStore.f32` (`35 → 0`). **`32` more shaders fully restored (`704 → 672`), `host_created: 204 → 244` (+40)!**
6. **Cycle 3 (`d41330e`):** Enabled `made_raw` for `tbuffer_*_format` instructions in `resource.rs` — **204 shaders advanced** past `"a descriptor changed after loading"` (`316 → 112`).
7. **Cycle 4 (`98e07d9`):** Implemented `Input::GroupInfo` (`TG_SIZE_EN` SGPR) in `lower.rs` — **`the thread-group info SGPR`: `6 → 0`**, **`4` more shaders fully restored (`672 → 668`), `host_created: 244 → 248`!**
8. **Cycle 5 (`0a25f7b`):** Fixed `wide` dispatch for `s_bitreplicate_b64_b32` in `lower.rs` — **`s_bitreplicate_b64_b32`: `3 → 0`**, **`a select on an untracked mask`: `4 → 0`**, **`2` more shaders fully restored (`668 → 666`), `host_created: 248 → 250`!**
9. **Cycle 6:** Implemented packed literal & inline f16 broadcast constant operands for `v_pk_fmac_f16` in `lower.rs` — **`a constant operand of v_pk_fmac_f16`: `2 → 0`**, **`2` more shaders fully restored (`666 → 664`), `host_created: 250 → 252` (+87 total since `1dfee4a`)!**

## Remaining Top Shader Failure Categories (Baseline `ae08fb7`)
| Unique Shaders | Category |
| ---: | --- |
| `316` | `a descriptor changed after loading is not supported` |
| `184` | `an address not from a root parameter is not supported` |
| `38` | `a descriptor outside the heaps is not supported` |
| `35` | `a typed buffer store of some channels or at an offset is not supported` |
| `24` | `a structured buffer of unknown stride is not supported` |
| `20` | `s_pack_ll_b32_b16 is not supported` *(fixing now)* |
| `15` | `a lane mask whose lanes depend on its own value is not supported` |
| `15` | `a lane mask exchanged where not every lane runs is not supported` |
| `15` | `s_pack_hh_b32_b16 is not supported` *(fixing now)* |
| `6` | `the thread-group info SGPR is not supported` |
| `5` | `a lane mask holding lanes elsewhere read as data is not supported` |
| `5` | `a select on an untracked mask is not supported` |
| `5` | `a whole-wave section step the simulation does not follow is not supported` |
| `5` | `a descriptor's address or format read as data is not supported` |
| `4` | `s_bfm_b64 is not supported` *(fixing now)* |

## Remaining Missing APIs & Non-Shader Warnings
- **`0x1C8` (`CreatePreCompiledGraphicsPipelineState`):** Sub-blobs captured (`gstate`, `pmeta`, VS/PS ISA + `0x41`/`0x42`/`0x43` metadata); full graphics state & signature decoding pending.
- **Alias-aware texture coherence:** `a texture picked by index changed after its upload with nothing to tell of it` (164 warnings across mip levels 0–7).
