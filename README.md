# XCL – Xbox Series X|S → Windows compatibility layer

[![Discord](https://img.shields.io/badge/Discord-join%20the%20server-5865F2?logo=discord&logoColor=white)](https://discord.gg/yB5tstRHrn)
**Discord: <https://discord.gg/yB5tstRHrn>**

XCL runs Xbox Series X|S (GDK) game files on a Windows PC **without CPU emulation**: the game's
x64 code runs natively and XCL stands in for the console libraries the PC does not have
(`kernelx.dll`, XGameRuntime, `d3d12_xs.dll`, …) – in the spirit of Wine/Proton. The console's
GPU code (RDNA2 machine code) is recompiled into PC shaders (HLSL → DXIL).

**Project state:** retail games run their menus – see [Games](#games). The foundation is tested
against real data from retail games (shader packs, texture archives, movies and the console DLLs
they ship). Details, measurements and next steps:
[`HANDOFF.md`](HANDOFF.md); design: [`docs/specs/2026-11-20-xcl-design.md`](docs/specs/2026-11-20-xcl-design.md).

## Games

| Game | Package (version) | State | What works | Limits |
|---|---|---|---|---|
| **Minecraft Preview** | `Microsoft.MinecraftBeta` 1.18.3064.0 | ✅ menus | the whole menu, sound and controller input – fully | offline: no Xbox Live sign-in, Realms or Marketplace |
| **Fortnite** | `436609B6.FortniteClient` 2.0.1461.7919 | ✅ menus | everything up to the point where the game needs its online services – fully | the game itself needs multiplayer (online servers), so it cannot be tested further |
| **XDefiant** | `Ubisoft.XDefiant-Game` 1.0.0.186 | ✅ menus | the menu – fully | the game's servers are shut down, so it cannot be tested further |

XCL stays offline: no fake Xbox Live, sign-in, game backends or licences.

## Highlights

- **100 % of a retail game's shaders translate** – all 26 698 RDNA2 programs (26 503 records) of its shader pack
  become HLSL; random samples compile to DXIL with 0 errors.
- **The game's own console DLLs run on the PC** – its Bink 2 video codec decodes the game's
  movies, its ray-tracing BVH builder builds acceleration structures on a PC GPU, the AMD FSR 4
  console upscaler sets up and records its work, the GDK HTTP library initialises.
- **Console textures decode exactly** – the GFX10 tiling and mip layout reproduce 909 of 912
  textures of the game's texture archive byte for byte, its volume textures included.
- **150 automated tests**, including GPU tests that compare translated shaders against expected
  results instruction by instruction.

## What works

✅ done and verified · 🟡 works, with a gap listed under *What is missing*

### Running the game's code

| Feature | State | What it does | Verified by |
|---|---|---|---|
| **PE loader** | ✅ | relocations, imports by name / ordinal / delay-load, TLS callbacks, exception tables, game DLLs, data-driven import routing (HLE / Windows / stub) | the game's 16 console DLLs load with 0 unresolved imports |
| **kernelx** | ✅ | title memory (budget from MicrosoftGame.Config, physical pages), threads on the PC's processors, modules, OS / console info | unit + end-to-end tests |
| **Memory** | ✅ | title graphics memory shared by CPU and GPU without copies, up to 16 GB (4 GiB windows) | GPU tests: loads, stores and atomics across windows |
| **File system** | ✅ | console paths (`GAME:` …) → the game folder; writes go to an overlay, the game folder is never modified | end-to-end: the test title's folder stays byte-identical |

### System services

| Feature | State | What it does | Verified by |
|---|---|---|---|
| **XGameRuntime** | ✅ | the GDK thunk protocol real titles use, laid out from a table of all 396 GDK runtime functions; offline XUser, XGameSave, persistent local storage, XPackage, XGame (title id), XSystem – the title sees an Xbox Series X / S; task queues, async, networking and dialogs through the PC's XGameRuntime | the game's libHttpClient initialises; users, saves and storage through the real call path; slot layout cross-checked against the game's own thunks |
| **Input** | ✅ | GameInput → the system GameInput | – |
| **Audio** | ✅ | XAudio2 → system XAudio2.9 (PCM / float voices; Opus 0x704F decoded via `MSOpusDecoder.dll`; XMA1 / XMA2 decoded in pure Rust) | unit + end-to-end tests |

### Graphics

| Feature | State | What it does | Verified by |
|---|---|---|---|
| **Shader recompiler** | ✅ | RDNA2 → HLSL → DXIL: VS, PS, CS, mesh (NGG), GS; atomics, LDS, scratch, images, samplers incl. static ones, wave ops, ray-tracing instructions; structured control flow; parallel DXC, disk cache | 26 698 / 26 698 programs of the retail pack; 0 DXC errors on random samples; per-instruction GPU tests |
| **Pipelines** | ✅ | PSOs from console shaders: VS+PS, CS, MS+PS, VS+GS+PS; root signature with the shaders' static samplers; non-blocking creation – a pipeline whose shaders are not compiled yet gets a stand-in at once and is built in the background | GPU tests drawing the game's own VS / PS pairs, also built in the background |
| **D3D12.X compute** | ✅ | console device slots, console descriptor heaps (T# / V# / S#), buffers at their console addresses, compute command lists recorded in the console format and replayed (tables, barriers, clears, copies) | the game's BVH builder and FSR 4 record and run through XCL |
| **D3D12.X graphics** | 🟡 | device, resources, heaps and pipelines are in place | draw submission and present need the game exe |
| **Textures** | ✅ | GFX10 tiling (all 14 2D swizzle modes, the 6 3D thick modes, linear), mip chains with mip tail, arrays, cubes and 3D textures → PC textures | 909 / 912 retail textures exact (the 3 volume LUTs decode smooth); GPU upload bit exact |
| **Ray tracing** | ✅ | BLAS / TLAS built on the GPU by the game's own XboxBvhBuilder; RDNA2 BVH node format; BVH intersection instructions emulated in shaders | GPU traversal = brute force reference; the game's builder output checked on the GPU |
| **Upscaling** | ✅ | the game's AMD FSR 4 console DLL creates its context and records its dispatches | end-to-end with the real DLL |

### Media and tools

| Feature | State | What it does | Verified by |
|---|---|---|---|
| **Video** | ✅ | the game's Bink 2 console DLL decodes its movies (720p / 1080p) | a game movie decodes frame by frame, picture checked |
| **Tools** | ✅ | `xcl inspect`, `xcl run`, `xcl disasm` (RDNA2, console shader containers), `xcl shaders` (pack statistics, recompile / compile runs), `xcl rpf`; `tools/ttpf-viewer` (texture archive viewer) | – |

## What is missing

| Needs a game executable | Not done yet | Not planned |
|---|---|---|
| D3D12.X draw records and `PresentX` / frame pacing | performance work (GPU buffers live in system memory) | Xbox Live, Store licensing (offline by design) |
| root arguments → shader registers for graphics | hull / domain / amplification shaders (none in the sample game) | decrypting XVC packages or any DRM |
| texture creation in title memory (binding the detiler), MSAA tiled textures | | console SDK headers / binaries in the repository |
| Bink GPU decoding, rehydrating prebuilt BVHs | | |

Every run writes `report.json` (called stubs, unknown D3D12.X slots and interfaces, XGameRuntime
classes asked for, shader statistics, audio formats, crash) – the to-do list for the next step.

## Building and usage

Windows 10/11 x64, Rust (`stable-x86_64-pc-windows-msvc`), the Windows SDK (DXC: `dxcompiler.dll`
+ `dxil.dll`, looked up in `Windows Kits\10\bin\<version>\x64`, next to `xcl.exe` or in
`%XCL_DXC_DIR%`), a D3D12 GPU with Shader Model 6.6 (developed on AMD RDNA4, others untested),
and the system XGameRuntime (`System32\xgameruntime.dll`, part of Windows / Gaming Services).

```powershell
cargo build --release --workspace          # target\release\xcl.exe

xcl inspect D:\Game\game.exe               # the game's imports and what XCL has for them
xcl run D:\Game [-- game arguments]        # run
xcl disasm <shader.bin> [--hex]            # RDNA2 disassembler (console containers too)
xcl shaders D:\data\shaders.rpf --top 50   # pack statistics; --recomp / --compile N
xcl rpf D:\data\shaders.rpf --extract NAME --out folder
```

- XCL works on **unpacked** game files you own; it contains nothing for decrypting or
  circumventing DRM.
- **The game folder is never modified** – writes go to the overlay.
- Title data: `%LOCALAPPDATA%\XCL\<title>\` (`%XCL_DATA_ROOT%`): `xcl.toml`, `overlay\`, `saves\`,
  `logs\xcl.log`, `logs\report.json`.

## Tests

```powershell
.\scripts\test.ps1                         # cargo build --workspace + cargo test --workspace
```

Unit tests, an end-to-end test with our own "console title" (`tests/testtitle`) and GPU tests
(D3D12, SM 6.6; skipped without a GPU / DXC). Tests on real game data are opt-in through
environment variables pointing at your own files – `XCL_SAMPLE_RPF` (shader pack),
`XCL_SAMPLE_DLLS` (the game's console DLLs), `XCL_SAMPLE_TTPF` (texture archive),
`XCL_SAMPLE_BK2` (a movie), `XCL_SAMPLE_BVH4`, `XCL_SAMPLE_AWC` (a sound) – described in `HANDOFF.md`; without them those
tests pass without running anything (`scripts/test.ps1` lists the ones not set). No game data is
part of the repository.

## Licenses

**GPL-2.0-only** (`LICENSE`); third-party sources: `THIRD_PARTY.md` (including AMD's
machine-readable ISA, AMD addrlib swizzle patterns, LLVM disassembler tests, AnyPS5,
libHttpClient headers). WinDurango serves only as a reference – no code is ported from it. The
repository contains no headers or binaries from the console SDK.
