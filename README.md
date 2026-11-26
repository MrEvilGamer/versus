# Versus

**Experimental Xbox Series compatibility layer for Windows — focused on NVIDIA GPU performance, memory efficiency and rendering correctness.**

Versus extends [VIctory](https://github.com/Marvisak/VIctory) and its community forks with a tuned Direct3D 12 backend and an incremental Guest GPU Virtualization Layer (GGVL).

[Architecture](docs/architecture.md) · [Performance roadmap](docs/30-60-fps-roadmap.md) · [Technical findings](docs/findings.md) · [Frame replay](docs/replay.md)

> **Active development:** Stable 30 FPS is the initial engineering target, with 60 FPS a longer-term investigation where hardware permits. Neither is currently demonstrated on the target NVIDIA hardware.

This repository contains no game files, proprietary game code or game assets. Users must provide their own legitimately obtained compatible files. Versus is not affiliated with or endorsed by any game developer, publisher or console manufacturer.

## Development snapshot — November 26, 2026

### NVIDIA performance

Reported local development benchmarks on an RTX 4060 Ti (8 GB), Ryzen 9 7900X, 32 GB RAM and Windows 11.

| Metric | Initial baseline | [M2](https://github.com/MrEvilGamer/versus/commit/93b6e0a) | [M3](https://github.com/MrEvilGamer/versus/commit/75bdd1f) |
|---|---:|---:|---:|
| Reported in-game FPS | 1.6 | 5.3 | 5.7 |
| Frame time | 637.3 ms | 187.1 ms | 176.4 ms |
| CPU preparation | 260.8 ms | 85.9 ms | 70.4 ms |
| GPU execution | 403.4 ms | 94.8 ms | 101.6 ms |
| Copy-back / readback | 424.0 ms | 28.1 ms | 25.1 ms |
| Heap buffer pass | 138.0 ms | 5.9 ms | 2.7 ms |

Milestone 3 reports a reduction from **197–240 to 13–20 hash passes per frame**, with 738 read-only hash skips during the benchmark and 4,286 tracked buffers.

These are preliminary reported local measurements, not independently reproduced results. Timing categories may overlap. The NVIDIA profile defaults to 640×360 and a 256-pixel texture dimension cap. Reduced graphics quality, disabled safeguards, and architectural improvements must be distinguished in comparisons. Sustained FPS, matched-scene repeat runs and correctness validation remain outstanding.

### Historical AMD results

The inherited AMD-focused fork reported 13–20 FPS indoors, 19–22 FPS during the first boat mission, and 6–10 FPS during a busy street chase on a Radeon RX 7900 XTX at 1440p (i9-12900K, 64 GB RAM, November 25). It also reported 40–46 GB of process private memory. These are historical upstream results, not directly comparable NVIDIA benchmarks.

## Guest GPU Virtualization Layer

GGVL aims to replace redundant memory scans and transfers with event-driven resource tracking, demand-driven synchronization and budget-aware memory management. It is not yet the authoritative memory system.

| Milestone | Implementation status |
|---|---|
| [M1 — Foundations](https://github.com/MrEvilGamer/versus/commit/5fc2e25) | Page table, coherency model, residency candidate selection and preliminary wave classifier. |
| [M1.1 — Hardening](https://github.com/MrEvilGamer/versus/commit/c630bb7) | Per-page ownership, transfer tickets, checked errors and queue-aware dependency structures. |
| [M2 — Runtime observation](https://github.com/MrEvilGamer/versus/commit/93b6e0a) | Live buffer registration and DXGI memory budget feedback. |
| [M3 — Write tracking](https://github.com/MrEvilGamer/versus/commit/f0fee0e) | CPU upload and declared UAV write hooks; experimental read-only hash bypass, enabled by default in [75bdd1f](https://github.com/MrEvilGamer/versus/commit/75bdd1f). |
| [M4 — Residency integration](https://github.com/MrEvilGamer/versus/commit/0346343) | DXGI usage/budget hysteresis (>92% / ≤82%), lazy eviction candidate sorting, and live empty-pool VRAM reclamation (`704 MiB` reclaimed under pressure). |
| [M5 — System RAM efficiency](https://github.com/MrEvilGamer/versus/commit/86bb38f) | Adaptive `VICTORY_WINDOW_BUDGET_MB` (512 MB on 16 GB systems, 2048 MB on 32 GB+ systems) and runtime hardware profile detection (`Constrained16GB` vs `HighEnd32GB`). |
| [M6 — Wave32 & IR optimization](https://github.com/MrEvilGamer/versus/commit/1fcdc87) | Mesa NIR-style algebraic peephole, shift chaining, power-of-2 strength reduction (`bitcode/opt.rs`), Class A/B/C Wave32 classification, and unused-LDS `split_lds` barrier elimination. |

Milestone labels indicate implementation progress, not production certification. Precise sub-page ownership, alias synchronization, lossless event tracking and actual queue dependencies require further validation. Legacy synchronization remains necessary for unsupported or unproven access paths.

## Engineering roadmap

- [x] Initial GGVL runtime observation and memory budget telemetry
- [x] Initial CPU upload and declared GPU write tracking
- [x] Experimental read-only hash bypass
- [ ] Validate all write paths, aliases, partial writes and queue dependencies
- [ ] Safely eliminate redundant hashes, comparisons and eager readbacks
- [ ] Implement and validate fence-safe eviction, restoration and adaptive VRAM residency
- [ ] Reduce system RAM pressure and PCIe spilling
- [ ] Optimize existing Wave64-to-Wave32 shader translation
- [ ] Reduce CPU submission and descriptor overhead
- [ ] Demonstrate sustained 30 FPS at documented quality settings
- [ ] Investigate 60 FPS where hardware permits

D3D12 remains the active backend. Vulkan is a potential future investigation, not an implemented alternative.

## Build from source

Requirements: Windows 10/11 x64, AVX2-capable CPU, a Direct3D 12 GPU supporting shader model 6.6 and mesh shaders, Rust (see [rust-toolchain.toml](rust-toolchain.toml)), MSVC build tools and compatible title files. An 8 GB GPU and 32 GB RAM are development targets, not guaranteed minimum requirements.

```powershell
git clone https://github.com/MrEvilGamer/versus.git
cd versus
Copy-Item victory.example.toml victory.toml
# Configure paths in victory.toml before running.
cargo build --locked --profile diag -p victory --features diagnostics
.\target\diag\versus.exe --help
```

The Cargo package is still named `victory`, but the executable is `versus.exe`. Some inherited scripts still reference `victory.exe`; check their executable paths before use.

### Experimental configuration

Defaults are defined in [versus-backend/src/lib.rs](crates/versus-backend/src/lib.rs) and can be overridden before launch.

| Variable | Purpose |
|---|---|
| `VICTORY_GGVL=0` | Disable GGVL observation. |
| `VICTORY_GGVL_SKIP_READONLY_HASH=0` | Disable experimental read-only hash bypass for comparisons and troubleshooting. |
| `VICTORY_DISPLAY_MODE` | Output resolution. |
| `VICTORY_MAX_TEXTURE_DIM` | Texture dimension limit; affects visual quality. |
| `VICTORY_VIDEO_BUDGET_MB` | Configured video-memory budget. |

The NVIDIA profile also modifies watchdog, residency waiting and BVH behavior. Maintain consistent settings during benchmark comparisons.

## Known limitations

Inherited issues include outdoor flicker, dim interiors without ray-traced indirect lighting, delayed texture streaming, pipeline compilation stutter and watchdog interruptions during frame capture. The new GGVL path requires further regression and long-session testing.

## Documentation

- [30–60 FPS roadmap](docs/30-60-fps-roadmap.md)
- [Architecture](docs/architecture.md)
- [Historical findings](docs/findings.md)
- [Frame capture and replay](docs/replay.md)
- [Upstream commands and debugging](docs/README-fxceless.md)
- [Asset policy](docs/asset-policy.md)

## Credits

Versus builds on the VIctory community, including [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX). Upstream history and contributor attribution are retained.

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**: the fork
  this one started from, and the people whose pull requests it carries.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray tracing fixes,
  cube gradients, sparse textures.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
  the ambient light idea, pipeline workers, and work by romainhedouin.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**.
- **[myokex/victory-amd](https://github.com/myokex/victory-amd)**: the guard over
  every view of a page (with Spexivus, joroyt and tightfall), and Joshua Odom's
  reads into title memory.
- **Geckomacabre** and **cxy0072019-sketch**: the texture read-back.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same
  title behaves under another layer; no code of it is here.
- This fork's changes were made with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
