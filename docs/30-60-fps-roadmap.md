# Roadmap to 30–60 FPS in GTA VI (Victory)

Target frame budgets:
- **30 FPS:** `33.3 ms/frame` (Xbox Series X target)
- **60 FPS:** `16.7 ms/frame`

Current measured baselines (2026-11-26):
- **12 GB RTX 4070 (`32b31f7`):** `90–110 ms` (9–12 FPS) indoors (~4,700 draws), `160–208 ms` (5–6 FPS) outdoors (~7,200–9,700 draws).
  - GPU `title lists`: ~`50 ms`
  - Layer GPU housekeeping (hashing/comparing/clamps): ~`15–23 ms`
  - CPU submission/prep thread: ~`70 ms + 13 ms / 1,000 draws`
- **8 GB NVIDIA GPU (`32b31f7`):** `220–540 ms` (2–4.6 FPS), 97% GPU busy because **~828 MB of hot shader buffers spill to PC RAM** and are read over PCIe (~20× slower than GDDR6).

---

## Pillar 1: Kill Eager GPU->CPU Copy-Back (Saves ~60–100 ms CPU + ~20 ms GPU + ~1–2 GB VRAM)
- **Measurement:** Upstream run 277 copied back `497 GB` over 49 minutes; **99.81%** of GPU shader writes were never read by the title's CPU code (only **0.19%** actually read).
- **Solution — Lazy Demand-Paged Readback (VEH):**
  - Keep GPU-written buffers permanently in VRAM (`DEFAULT` heap).
  - Eliminate per-frame GPU hashing (`~288 hash passes`), compare passes (`~230 compare passes`), and eager copy-backs.
  - Protect mirrored guest CPU pages with `PAGE_NOACCESS`; trap the rare 0.19% CPU reads in a Vectored Exception Handler (VEH) and fetch only the touched 64 KiB page via the copy queue.
  - Reference: `fxceless/victory-fork` PR `#1178`.

## Pillar 2: Solve the 8 GB VRAM Wall (Saves ~150–300 ms GPU on 8 GB cards)
- **Measurement:** Xbox Series X has `13.5 GB` unified GDDR6 (`560 GB/s`); an 8 GB PC GPU has `~6.0 GB` usable D3D12 VRAM.
- **Solution — Pin Hot Buffers in VRAM + Top-Mip Texture Clamp (`VICTORY_MAX_TEXTURE_DIM`):**
  - Pin 100% of hot shader buffers inside VRAM (`VICTORY_CARD_BUFFERS_MB=1024+`), never letting shaders read hot buffers over PCIe.
  - Add `VICTORY_MAX_TEXTURE_DIM=1024` (using tiled resources + `ResourceMinLODClamp`): skip allocating/uploading mip levels finer than `1024px`, keeping full texture dimensions intact while cutting texture VRAM by **~75%** (freeing **2–3 GB VRAM**).
  - Eliminate duplicate shadow/twin copies via lazy VEH write-back.

## Pillar 3: Kill the CPU Submission Tax (`70+ ms` -> `< 8 ms`)
- **Measurement:** Native D3D12 submits 4,700 draws in `< 2 ms`; Victory currently takes `~70–130 ms` (~50× slower/draw).
- **Solution:**
  - Dirty-tracked / GPU-side descriptor patching instead of CPU scanning 100k–380k descriptor slots/frame.
  - Truly lock-free parallel command-list recording across the game's render threads (PR `#1146`).
  - Hoist indirect draw validation once per command list (PR `#1184` / `VICTORY_INDIRECT_GATHER=1`).

## Pillar 4: Near-Native Shader GPU Speed (`50 ms` -> `12–18 ms`)
- **Measurement:** Even with 0 PCIe spills on a 12 GB RTX 4070 (~2× Xbox Series X GPU), translated `title lists` take `~50 ms` instead of `~15 ms`.
- **Solution:**
  - Compiler optimization pass before DXIL emission (DCE, scalarization, vectorized 4-dword loads `VICTORY_SHADER_OPT=127`, folding address math) to cut VGPR pressure and double occupancy.
  - Static Wave32 specialization: prove when Wave64 shaders have no cross-half lane sync and emit pure native Wave32 without LDS/barrier emulation.
  - Long-term: Vulkan backend (`VK_KHR_buffer_device_address` + `VK_EXT_descriptor_buffer`) to eliminate D3D12's 1M NVIDIA descriptor heap cap and software page-table indirections completely.
