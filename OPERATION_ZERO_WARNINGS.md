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
19. **Cycle 17 (`724bc53`):** Added `s_add_u32(..., src_scc)` carry recognition to `high_half()` in `resource.rs` — **`an address with an untracked high half`: `148 → 2` (`-146`)**, **`host_created: 375 → 385` (`+220` total since `1dfee4a`), Total `WARN`: `545` (`-61%` from `1,392` baseline)!**
20. **Cycle 18 (`51abe60`):** Added `Source::Built { address, .. } if address.param == CODE` support for `buffer_load_dword*` in `lower.rs` — **`a descriptor built around a table in the code`: `24 → 0` (18th category eliminated!)**, **`35` shaders restored (`521 → 486`), `host_created: 385 → 409` (`+24`, `+244` total since `1dfee4a`), Total `WARN`: `545 → 511` (`-63%` from `1,392` baseline)!**
21. **Cycles 19–20 (`5ce59ff`):** Guarded `Atom::Opaque` dominance in `lower.rs` (`bitcode.rs:851` forward-ref panic fixed → **`Total ERROR: 1 → 0`**) and parsed `FSPX` `0x41` buffer stride (`le32(e, 16)`) in `native.rs` — **`a structured buffer of unknown stride`: `121 → 0` (19th category eliminated!)**, **`49` shaders restored (`486 → 437`), `host_created: 409 → 443` (`+34`, `+278` total since `1dfee4a`), Total `WARN`: `511 → 463` (`-67%` from `1,392` baseline), `0` errors!**
22. **Cycle 21 (`13e5ab3`):** Implemented saturating `clamp` for `v_add_nc_u32`/`v_sub_nc_u32`/`v_subrev_nc_u32` in `lower.rs` — **`integer output modifiers`: `3 → 0` (20th category eliminated!)**, **`host_created: 443 → 445` (`+280` total since `1dfee4a`), `0` errors!**
23. **Cycle 22 (`99637db`):** Seeded `lane_constant_masks()` with `st.masks.values()` in `lower.rs` so compute shaders with opaque/constant masks in selects/carries enable `console_lanes` — **`an opaque lane mask`: `23 → 0` (21st category eliminated!)**, **`11` shaders restored (`466 → 455`), `host_created: 445 → 454` (`+9`, `+289` total since `1dfee4a`), `executed: 4 → 8`, `dropped: 1 → 0`, `0` errors!**
24. **Cycle 23 (`9d98625`):** Extended `sized_view()` in `lower.rs` to recognize 4-dword `s_load_dwordx4` image dimension queries (`WIDTH_LO`/`HEIGHT` BFE) — **`a descriptor read only as data`: `62 → 0` (22nd category eliminated!)**, **`61` shaders restored (`455 → 394`, broke below 400!), `host_created: 454 → 515` (`+61`, crossed 500+!), Total `WARN`: `480 → 419` (`-70%` from `1,392` baseline), `0` errors!**
25. **Cycle 24 (`8344132`):** Raised `MAX_JOINS` from `8` to `12` in `resource.rs` — **`a descriptor joined too often`: `4 → 0` (23rd category eliminated!, `31 → 30` categories), `0` errors!**
26. **Cycle 25 (`446261a`):** Added `s_bfm_b64`/`s_bfm_b32` and `s_bitreplicate_b64_b32` constant `exec` masks to `emit/wave.rs` — **`a whole-wave section step the simulation does not follow`: `9 → 6` (`-3`), `host_created: 515 → 519` (`+4`), Total `WARN`: `419 → 417`, Unique Failed Shaders: `394 → 392`, `0` errors!**
27. **Cycle 26 (`70aa955`):** Implemented `s_flbit_i32_b64` in `lower.rs` — **`s_flbit_i32_b64`: `1 → 0` (24th category eliminated!), `host_created: 519 → 521` (`+2`), Total `WARN`: `417 → 413` (`-70.3%` from `1,392` baseline), Unique Failed Shaders: `392 → 388`, `0` errors!**
28. **Cycle 27 (`2c8c2ee`):** Resolved multi-block descriptor phis by expanding the latest RPO block first in `resource.rs` — **`a descriptor joined in several blocks`: `3 → 0` (25th category eliminated!, `30 → 29` categories), `host_created: 521 → 523` (`+2`), `0` errors!**
29. **Cycle 28 (`2e39772`):** Treated `vcc_lo`/`vcc_hi` (`106 | 107`) on entry as uninitialized zero in `lower.rs` — **`a hardware SGPR input`: `6 → 0` (26th category eliminated!, `29 → 28` categories), `7` shaders restored (`388 → 381`), `host_created: 523 → 530` (`+7`), Total `WARN`: `413 → 406` (`-70.8%` from `1,392` baseline), `0` errors!**
30. **Cycle 29 (`514083b`):** Supported arbitrary `dmask` (`1..=15`) write masks for `image_store` in `lower.rs` — **`an image store of some channels`: `11 → 0` (27th category eliminated!, `28 → 27` categories), `5` shaders restored (`381 → 376`), `host_created: 530 → 533` (`+3`), Total `WARN`: `406 → 401` (`-71.2%` from `1,392` baseline), `0` errors!**
31. **Cycle 30 (`5deac7b`):** Treated `self.dead(v)` stale descriptor registers in `Atom::Opaque(v)` as `false` in `lower.rs` — **`a sampler's descriptor read as data`: `1 → 0` (28th category eliminated!), `a descriptor's address or format read as data`: `27 → 14` (`-13`), `a constant buffer's descriptor read as data`: `13 → 5` (`-8`), `20` shaders restored (`376 → 356`, 50.0% of baseline 712 restored!), `host_created: 533 → 553` (`+20`), Total `WARN`: `401 → 381` (`-1,011` / `-72.6%` from `1,392` baseline!), `0` errors!**
32. **Cycle 31:** Added `TextureCube` (`dim == 3 => 5`) to `descriptor_as_data` in `lower.rs` — **`an image dimension (cube or MSAA)`: `1 → 0` (29th category eliminated!, `27 → 26` categories), `0` errors!**

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
