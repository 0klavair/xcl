# XCL – Xbox Series X|S → Windows compatibility layer

XCL runs Xbox Series X|S (GDK) game files on a Windows PC **without CPU emulation**:
the game's code (x64) runs natively, and XCL substitutes the console libraries that the PC does not
have (`kernelx.dll`, XGameRuntime, `d3d12_xs.dll`, …) – like Wine/Proton for Linux.
The console's GPU code (RDNA2) is translated into PC shaders.

Details and the current state of the work: [`HANDOFF.md`](HANDOFF.md), design:
[`docs/specs/2026-11-20-xcl-design.md`](docs/specs/2026-11-20-xcl-design.md).

## Status

| Works | Not yet |
|---|---|
| PE loader (relocations, imports, TLS, exceptions, game DLLs), stubs + call report | D3D12.X draw records and present (waiting for a game exe) |
| `kernelx`, offline XGameRuntime (XTaskQueue, XAsync, XUser, saves), VFS with overlay, GameInput, XAudio2 | XMA2 / Opus decoding (silence for now) |
| D3D12.X on a real device: console slots, resources and descriptor heaps in UMA memory, compute command lists | Xbox Live services – offline only by design |
| PSOs from console shaders: VS+PS, CS, MS (NGG) and GS (two passes) | Encrypted XVC packages – not by design |
| RDNA2 → HLSL → DXIL recompiler: 100% of the sample game's 24 064 programs, DXIL cache | |
| Console DLLs from the game: XboxBvhBuilder (BLAS/TLAS on the GPU), Bink 2, FSR4 context | |

## Building and usage

Windows 10/11 x64, Rust (`stable-x86_64-pc-windows-msvc`), Windows SDK (DXC: `dxcompiler.dll` +
`dxil.dll`, looked up in `Windows Kits\10\bin\<version>\x64`, next to `xcl.exe` or in `%XCL_DXC_DIR%`).

```powershell
cargo build --release --workspace          # target\release\xcl.exe

xcl inspect D:\Game\game.exe               # the game's imports and what XCL has for them
xcl run D:\Game [-- game arguments]        # run
xcl disasm <shader.bin> [--hex]            # RDNA2 disassembler (console containers too)
xcl shaders D:\data\shaders.rpf --top 50   # corpus statistics; --recomp / --compile N
xcl rpf D:\data\shaders.rpf --extract NAME --out folder
```

- XCL works on **unpacked** game files; it contains nothing for decrypting or
  circumventing DRM.
- **The game folder is never modified** – writes go to the overlay.
- Title data: `%LOCALAPPDATA%\XCL\<title>\` (`%XCL_DATA_ROOT%`): `xcl.toml`, `overlay\`,
  `saves\`, `logs\xcl.log`, `logs\report.json` (called stubs, unknown D3D12.X slots,
  shaders, audio, crash) – a to-do list after every run.

## Tests

```powershell
.\scripts\test.ps1                         # cargo build --workspace + cargo test --workspace
```

Unit tests, the title test (`tests/testtitle` – our own "console title"; the game folder
must remain unchanged byte for byte) and GPU tests (D3D12, SM 6.6; skipped without a GPU/DXC).
Tests on real data are enabled by environment variables, including `XCL_SAMPLE_RPF`
(`shaders.rpf`), `XCL_SAMPLE_DLLS` (console DLLs from the game) – described in `HANDOFF.md`.

## Licenses

**GPL-2.0-only** (`LICENSE`); third-party sources: `THIRD_PARTY.md` (including AMD Machine-Readable
ISA, LLVM disassembler tests, AnyPS5, libHttpClient headers, docs.xbox.com). WinDurango serves
only as a reference – we do not port its code. The repository contains no headers or
binaries from the console SDK.
