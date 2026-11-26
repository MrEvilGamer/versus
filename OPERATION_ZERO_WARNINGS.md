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
9. **Cycle 6 (`559ae39`):** Implemented packed literal & inline f16 broadcast constant operands for `v_pk_fmac_f16` in `lower.rs` — **`a constant operand of v_pk_fmac_f16`: `2 → 0`**, **`2` more shaders fully restored (`666 → 664`), `host_created: 250 → 252` (+87 total since `1dfee4a`)!**
10. **Cycle 8 (`5643e6d`):** Allowed `vcc_lo`/`vcc_hi` (`106 | 107`) in `ir::unspill::parked()` (`ir.rs`) so spill VGPRs like `v141` are promoted to scalar moves (`a descriptor neither loaded nor built`: `65 → 63`, `0` errors).
11. **Cycle 9 (`fa79fdc`):** Implemented `ds_append`/`ds_consume` LDS atomics in `lower.rs` (`an LDS address`: `2 → 0`) and separated expected physical-section aliased texture re-uploads (`blocks_shown_elsewhere >= 1`, `tracing::debug!`) from genuine unaliased texture changes (`tracing::warn!`, `0` occurrences) in `draw.rs`. **Total `WARN`: `887 → 687` (`-200`), unique categories: `44 → 31`, `0` errors!**
12. **Cycle 10 (`2837c4e`):** Added `s_bfm_b64` constant folding in `resource.rs` — **`an address not from a root parameter`: `241 → 0` (100% eliminated!)**, **`56` ray-tracing shaders fully restored (`663 → 607`), `host_created: 252 → 299`, Total `WARN`: `687 → 630`, `0` errors!**
13. **Cycle 11 (`f2242ef`):** Implemented `s_absdiff_i32` in `lower.rs` — **`s_absdiff_i32`: `7 → 0`**, **`0x218` host D3D12 PSOs crossed 300+ (`299 → 301`, `+136` total since `1dfee4a`), `0` errors!**
14. **Cycle 12 (`6c80ada`):** Implemented 64-bit scalar base + 32-bit VGPR offset (`saddr`) for `global_load_*`/`global_store_*` in `lower.rs` — **`a global address with a scalar base`: `2 → 0`**, **`2` more shaders fully restored (`614 → 612`), `host_created: 301 → 303`, `0` errors!**
15. **Cycle 13 (`9ef0da5`):** Added `VICTORY_DUMP_1C8` capture hook and `tools/dump_1c8.py`, decoding `0x1C8` `pmeta` (input layout), `vs_meta`/`ps_meta` (`0x10` header + `0x41`/`0x42`/`0x43` `FSPX` records), and `gstate` (RDNA2 registers).
16. **Cycle 14 (`8d79b83`):** Implemented IEEE-754 `v_ldexp_f32` in `lower.rs` — **`v_ldexp_f32`: `2 → 0`**, **Total `WARN`: `633` (`29` categories), `host_created: 304`, `0` errors!**
17. **Cycle 15 (`f91de1e`):** Implemented DPP source modifier support for `v_cmp_*`/`v_cmpx_*` in `lower.rs` — **`a compare with DPP`: `1 → 0`**, **Total `WARN`: `631` (`28` categories), `host_created: 305`, `0` errors!**
18. **Cycle 16 (`264d134`):** Added trailing SM 6.6 `ResourceHeap`/`SamplerHeap` user SGPRs (`COMPUTE_PGM_RSRC2.USER_SGPR`) to `fspx_program()` in `native.rs` — **`a descriptor outside the heaps`: `219 → 10` (`-209`)**, **`104` shaders fully restored (`609 → 505`), `host_created: 305 → 375` (`+70`), `executed: 4 → 7`!**
19. **Cycle 17:** Added `s_add_u32(..., src_scc)` carry recognition to `high_half()` in `resource.rs` — **`an address with an untracked high half`: `148 → 2` (`-146`)**, **`host_created: 375 → 385` (`+220` total since `1dfee4a`), Total `WARN`: `545` (`-61%` from `1,392` baseline)!**

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
