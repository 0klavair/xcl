# XCL – warstwa zgodności Xbox Series X|S → Windows

XCL uruchamia pliki gier z Xbox Series X|S (GDK) na PC z Windows **bez emulacji procesora**:
kod gry (x64) wykonuje się natywnie, a XCL podstawia biblioteki konsoli, których na PC nie
ma (`kernelx.dll`, XGameRuntime, `d3d12_xs.dll`, …) – jak Wine/Proton dla Linuksa.
Kod GPU konsoli (RDNA2) jest tłumaczony na shadery PC.

Szczegóły i bieżący stan prac: [`HANDOFF.md`](HANDOFF.md), projekt:
[`docs/specs/2026-11-20-xcl-design.md`](docs/specs/2026-11-20-xcl-design.md).

## Stan

| Działa | Jeszcze nie |
|---|---|
| Loader PE (relokacje, importy, TLS, wyjątki, DLL gry), zaślepki + raport wywołań | Rekordy rysowania D3D12.X i present (czeka na exe gry) |
| `kernelx`, XGameRuntime offline (XTaskQueue, XAsync, XUser, zapisy), VFS z nakładką, GameInput, XAudio2 | Dekodowanie XMA2 / Opus (dziś cisza) |
| D3D12.X na prawdziwym urządzeniu: sloty konsoli, zasoby i sterty deskryptorów w pamięci UMA, listy poleceń compute | Usługi Xbox Live – z założenia tylko offline |
| PSO z shaderów konsoli: VS+PS, CS, MS (NGG) i GS (dwa przebiegi) | Zaszyfrowane paczki XVC – z założenia nie |
| Recompiler RDNA2 → HLSL → DXIL: 100% z 24 064 programów przykładowej gry, cache DXIL | |
| DLL konsoli z gry: XboxBvhBuilder (BLAS/TLAS na GPU), Bink 2, kontekst FSR4 | |

## Budowanie i użycie

Windows 10/11 x64, Rust (`stable-x86_64-pc-windows-msvc`), Windows SDK (DXC: `dxcompiler.dll` +
`dxil.dll`, szukane w `Windows Kits\10\bin\<wersja>\x64`, obok `xcl.exe` albo w `%XCL_DXC_DIR%`).

```powershell
cargo build --release --workspace          # target\release\xcl.exe

xcl inspect D:\Gra\gra.exe                 # importy gry i co XCL dla nich ma
xcl run D:\Gra [-- argumenty gry]          # uruchomienie
xcl disasm <shader.bin> [--hex]            # dezasembler RDNA2 (też kontenery konsoli)
xcl shaders D:\dane\shaders.rpf --top 50   # statystyki korpusu; --recomp / --compile N
xcl rpf D:\dane\shaders.rpf --extract NAZWA --out folder
```

- XCL działa na **rozpakowanych** plikach gry; nie zawiera niczego do odszyfrowywania ani
  obchodzenia DRM.
- **Folder gry nigdy nie jest modyfikowany** – zapisy trafiają do nakładki.
- Dane tytułu: `%LOCALAPPDATA%\XCL\<tytuł>\` (`%XCL_DATA_ROOT%`): `xcl.toml`, `overlay\`,
  `saves\`, `logs\xcl.log`, `logs\report.json` (wywołane zaślepki, nieznane sloty D3D12.X,
  shadery, audio, awaria) – lista rzeczy do zrobienia po każdym uruchomieniu.

## Testy

```powershell
.\scripts\test.ps1                         # cargo build --workspace + cargo test --workspace
```

Testy jednostkowe, test tytułu (`tests/testtitle` – własny „tytuł konsolowy”; folder gry
musi zostać bajt w bajt niezmieniony) i testy na GPU (D3D12, SM 6.6; bez GPU/DXC pomijane).
Testy na prawdziwych danych włączają zmienne środowiskowe, m.in. `XCL_SAMPLE_RPF`
(`shaders.rpf`), `XCL_SAMPLE_DLLS` (DLL konsoli z gry) – opis w `HANDOFF.md`.

## Licencje

**GPL-2.0-only** (`LICENSE`); źródła zewnętrzne: `THIRD_PARTY.md` (m.in. AMD Machine-Readable
ISA, testy dezasemblera LLVM, AnyPS5, nagłówki libHttpClient, docs.xbox.com). WinDurango służy
tylko jako referencja – jego kodu nie przenosimy. Repozytorium nie zawiera nagłówków ani
binariów z SDK konsoli.
