# XCL – Xbox Series X|S → Windows compatibility layer

XCL runs Xbox Series X|S (GDK) game files on a Windows PC **without CPU emulation**: the game's
x64 code runs natively and XCL stands in for the console libraries the PC does not have
(`kernelx.dll`, XGameRuntime, `d3d12_xs.dll`, …) – in the spirit of Wine/Proton. The console's
GPU code (RDNA2 machine code) is recompiled into PC shaders (HLSL → DXIL).

**Project state:** the foundation is in place and tested against real data from a retail game
(its shader pack, texture archive, movies and the 16 console DLLs it ships). No game reaches its
first frame yet: draw submission and present use the console's own D3D12.X command format, which
can only be worked out from a game executable. Details, measurements and next steps:
[`HANDOFF.md`](HANDOFF.md); design: [`docs/specs/2026-11-20-xcl-design.md`](docs/specs/2026-11-20-xcl-design.md).

## What works

| Area | State | Details | Verified by |
|---|---|---|---|
| **PE loader** | ✅ | relocations, imports (by name / ordinal, delay-load), TLS callbacks, exception tables, game DLLs, data-driven import routing (HLE / Windows / stub) | unit tests; the 16 console DLLs of a retail game load and run `DllMain`, 0 unresolved imports |
| **kernelx** | ✅ | memory (title memory budget from MicrosoftGame.Config, physical pages), threads, the PC's processors, modules (`LoadLibrary` / `GetProcAddress` routed to HLE), OS / console info | unit + e2e tests |
| **File system** | ✅ | console paths (`GAME:`, `G:` …) → game folder; every write goes to an overlay, the game folder stays untouched | e2e: the test title's folder is unchanged byte for byte |
| **XGameRuntime** | ✅ | the GDK thunk protocol (`InitializeApiImpl*` / `QueryApiImpl`, as GDK titles call it): XCL's own offline XUser (one local user), XGameSave (saves in the XCL data folder), XPackage, XGame (title id), XSystem; XTaskQueue / XAsync / XThread / XNetworking / dialogs forwarded to the PC's XGameRuntime; by-name exports for direct imports | a retail GDK library (libHttpClient.GDK) initialises; XUser and XGameSave through the thunk vtables on PC task queues |
| **Input** | ✅ | GameInput → the system `GameInput.dll` | – |
| **Audio** | 🟡 | XAudio2 → system XAudio2.9; PCM / float voices | unit tests; XMA2 / Opus voices play silence for now |
| **Video** | 🟡 | the game's own Bink 2 console DLL decodes its movies on the CPU | e2e: a game movie decodes (720p, picture checked) |
| **D3D12.X device** | 🟡 | real D3D12 device; console vtable slots; console descriptor heaps (32-byte T#/V#, 16-byte S#); buffers placed in a GPU-visible heap at their console addresses; compute command lists recorded in the console format and replayed (descriptor tables, barriers, clears, copies) | the game's XboxBvhBuilder and FSR4 DLLs record and run their work through XCL |
| **Shader recompiler** | ✅ | RDNA2 → HLSL → DXIL; VS, PS, CS; mesh (NGG) and geometry shaders as compute + pass-through mesh pass; atomics, LDS, scratch, images, samplers (incl. static samplers built in code), ray-tracing BVH instructions emulated; structured control flow; DXC worker pool, disk cache | all 24 064 programs of a retail shader pack translate (91.5 % fully structured); random 1 % / 10 % samples compile with 0 DXC errors; per-instruction GPU tests against expected results |
| **Pipelines** | ✅ | PSOs from console shaders: VS+PS, CS, MS+PS, VS+GS+PS; ABI root signature with static samplers | GPU tests drawing retail VS/PS pairs (textured, descriptor change between draws) |
| **Textures** | 🟡 | GFX10 tiling (14 2D swizzle modes), mip chains with mip tail, slices; detiled into PC textures | the model matches the layout of 908 / 912 textures of a retail texture archive; GPU round trip bit exact; binding to the game's texture API needs the exe |
| **Ray tracing** | 🟡 | the game's XboxBvhBuilder builds BLAS / TLAS on the GPU through XCL; RDNA2 BVH node format | GPU tests: traversal = brute force; prebuilt (rehydrated) BVHs not yet |
| **Tools** | ✅ | `xcl inspect` (imports and what XCL has for them), `xcl run`, `xcl disasm` (RDNA2, console containers), `xcl shaders` (pack statistics, recompile / compile runs), `xcl rpf` (extract); `tools/ttpf-viewer` (texture archive viewer) | – |

## What is missing

| Needs a game executable | Not done yet | Not planned |
|---|---|---|
| D3D12.X draw records and `PresentX` / frame pacing | Opus / XMA2 audio decoding | Xbox Live, Store licensing (offline by design) |
| root arguments → shader registers for graphics | GPU memory above 4 GB (shaders address one 4 GB window) | decrypting XVC packages or any DRM |
| texture creation in title memory (binding the detiler) | non-blocking pipeline creation (first run compiles shaders synchronously) | console SDK headers / binaries in the repository |
| the persistent-local-storage class and `XSystemGetDeviceType` slot of the thunk protocol | hull / domain / amplification shaders (none in the sample game), linear / 3D / MSAA tiled textures | |
| Bink GPU decoding, rehydrating prebuilt BVHs | performance work (GPU buffers live in system memory) | |

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
`XCL_SAMPLE_BK2` (a movie), `XCL_SAMPLE_BVH4` – described in `HANDOFF.md`. No game data is part
of the repository.

## Licenses

**GPL-2.0-only** (`LICENSE`); third-party sources: `THIRD_PARTY.md` (including AMD's
machine-readable ISA, AMD addrlib swizzle patterns, LLVM disassembler tests, AnyPS5,
libHttpClient headers). WinDurango serves only as a reference – no code is ported from it. The
repository contains no headers or binaries from the console SDK.
