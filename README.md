# XCL – warstwa zgodności Xbox Series X|S → Windows

XCL uruchamia pliki gier z Xbox Series X|S (GDK, „Scarlett”) na zwykłym PC z Windows
**bez emulacji procesora** – kod gry (x64) wykonuje się natywnie, a XCL podstawia własne
implementacje bibliotek konsoli, których na PC nie ma (`kernelx.dll`, XGameRuntime,
`d3d12_xs.dll`, …). Podobnie działa Wine/Proton (Windows → Linux) i WinDurango (Xbox One).

Projekt: [`docs/specs/2026-11-20-xcl-design.md`](docs/specs/2026-11-20-xcl-design.md).

## Stan: etap M1 – „gra startuje i czyta swoje pliki”

| Działa | Jeszcze nie |
|---|---|
| Własny loader PE: mapowanie, relokacje, importy (też delay-load), statyczny TLS, wyjątki C++/SEH, DllMain/TLS callbacks, DLL dostarczane z grą | Grafika – D3D12.X to na razie **urządzenie zastępcze** (bez obrazu) → M2 |
| Automatyczne zaślepki dla brakujących funkcji + raport, co gra wywołała | Dekodowanie XMA2 / sprzętowego Opusa konsoli (na razie cisza) → M3 |
| **XAudio2** (`xaudio2_9.dll`) → systemowe XAudio2 2.9; głosy XMA2 i formaty nieznane Windows → PCM16 z ciszą tej samej długości | |
| `kernelx`: pamięć z flagami konsoli, XMem*, TitleMemoryStatus, wątki, moduły | GameInput, DirectStorage → M4 |
| VFS: `G:\`, `GAME:`, `T:\`; folder gry **tylko do odczytu**, zapisy → nakładka | Strony fizyczne (`MapTitlePhysicalPages`), XGameSave (blob API) |
| XGameRuntime offline: XTaskQueue, XAsync, XUser, XSystem, XPackage, zapisy, PLS | Usługi Xbox Live (tylko offline – z założenia) |
| **Shadery:** przechwytywanie i klasyfikacja (DXIL/DXBC/kod konsoli) + zrzut do `shaders\` | Recompiler mikrokod RDNA2 → shader PC (kolejne etapy, design §5.7a) |
| **Dekoder/dezasembler RDNA2** (`xcl disasm`) z oficjalnej specyfikacji AMD + rozszerzenia GPU konsoli | Realny backend D3D12 (przepuszczanie do sterownika) |
| **Kontenery shaderów konsoli** (DXIL → XDAT → XBCH → zlib → FSSM) + statystyki korpusu (`xcl shaders`) | |
| **Recompiler RDNA2 → HLSL** (tryb kernel, weryfikowany na GPU): 31% programów przykładowej gry tłumaczy się w całości | Tryby VS/PS (interpolacja, wyjścia), tekstury, model pamięci GPU konsoli |

## Budowanie

Wymagania: Windows 10/11 x64, Rust (toolchain `stable-x86_64-pc-windows-msvc`) z Visual Studio Build Tools.

```powershell
cargo build --release --workspace
# plik wykonywalny: target\release\xcl.exe
```

## Użycie

```powershell
# 1. analiza bez uruchamiania – co gra importuje i co XCL dla tego ma
xcl inspect D:\MojeGry\Gra\gra.exe

# 2. uruchomienie (folder z rozpakowanymi plikami gry albo konkretny .exe)
xcl run D:\MojeGry\Gra
xcl run D:\MojeGry\Gra\gra.exe -- -argument_gry

# 3. shadery przechwycone podczas uruchomienia – format i kod GPU konsoli (RDNA2)
xcl disasm %LOCALAPPDATA%\XCL\<tytuł>\shaders\<hash>_ps.bin
xcl disasm plik.bin --offset 0x40 --all      # wymuszone dekodowanie od offsetu
xcl disasm shader.dxbc --hex --fssm out.bin  # kontener konsolowy: pełny nagłówek FSSM + zapis FSSM

# 4. statystyki całego korpusu: plik, folder (np. <dane>\shaders\) albo paczka RPF9
xcl shaders %LOCALAPPDATA%\XCL\<tytuł>\shaders
xcl shaders D:\dane\shaders.rpf --top 100     # --json: pełny histogram + kolejność implementacji
xcl shaders D:\dane\shaders.rpf --recomp --compile 500   # tłumaczenie RDNA2 → HLSL + próbka DXC
```

Recompiler (`gpu/recomp/`) potrzebuje DXC: `dxcompiler.dll` + `dxil.dll` z Windows SDK
(szukane automatycznie w `Windows Kits\10\bin\<wersja>\x64`), obok `xcl.exe` albo w
`%XCL_DXC_DIR%`. Testy instrukcji uruchamiają się na prawdziwym GPU (D3D12, SM 6.6, wave64);
bez GPU/DXC są pomijane z komunikatem.

### Shadery z konsoli

Gry z Series dostarczają shadery jako gotowy kod maszynowy GPU konsoli (RDNA2)
zamiast przenośnego DXIL. XCL przy każdym tworzeniu PSO rozpoznaje format, zapisuje
blob do `shaders\` i – jeśli znajdzie kod RDNA2 – od razu zapisuje jego dezasemblację
(`*.rdna2.txt`) oraz rozpakowany FSSM (`*.fssm0.bin`). Podsumowanie jest w `report.json`
→ `shaders` (w tym `xbox_hw_stages` i histogram `rdna2_mnemonics`). To dane wejściowe dla
recompilera (RDNA2 → shader PC), który jest kolejnym etapem.

Format konsolowy (`gpu/xbox.rs`, rozpracowany na 23 869 shaderach z prawdziwej gry):
zwykły kontener DXBC, ale część `DXIL` nie ma bitcode'u (zaślepka `NOBC`) – zamiast niego
tabela `XDAT` z hashem, wpisami `XBCH` (zlib → `FSSM`: kod natywny + metadane, po jednym
na wariant etapu sprzętowego, np. VS jako ES i jako VS) i zagnieżdżonym nagłówkiem.

Dekoder jest generowany z oficjalnej specyfikacji AMD (`scripts/gen_rdna2.py`,
plik `amdgpu_isa_rdna2.xml` z [gpuopen.com/machine-readable-isa](https://gpuopen.com/machine-readable-isa/), MIT)
i sprawdzany na 23 812 instrukcjach z testów dezasemblera LLVM (`tests/data/llvm-amdgpu`).
GPU konsoli zachował z RDNA1 (gfx10.1) instrukcje `v_mac_f32`, `v_mad_f32`, `v_madmk_f32`,
`v_madak_f32`, usunięte w detalicznym RDNA2 – są dopisane w `gpu/rdna2_xbox.rs`. Z nimi
dekoder rozpoznaje 100% z 28,97 mln instrukcji przykładowego korpusu.

- XCL działa na **już rozpakowanych** plikach (zwykłe PE + dane). Zaszyfrowane paczki
  XVC/MSIXVC nie są obsługiwane, a XCL nie zawiera niczego, co by je odszyfrowywało
  ani obchodziło DRM (XStore zwraca „niedostępne”).
- **Folder gry nigdy nie jest modyfikowany.** Gdy gra chce coś zapisać w swoim folderze,
  XCL kopiuje ten jeden plik do nakładki i zmienia kopię. Usunięcie pliku gry zapisuje
  tylko znacznik („whiteout”).
- Dane XCL dla tytułu: `%LOCALAPPDATA%\XCL\<tytuł>\` (albo `%XCL_DATA_ROOT%\<tytuł>\`):

  | Folder / plik | Zawartość |
  |---|---|
  | `xcl.toml` | konfiguracja (tworzona automatycznie): Series X/S, montowania, aliasy DLL, wartości zaślepek |
  | `overlay\` | kopie plików gry zmienione przez grę, nowe pliki; `.xcl_whiteouts` = „usunięte” |
  | `saves\`, `pls\`, `temp\` | zapisy gry, persistent local storage, `T:\` |
  | `logs\xcl.log`, `logs\report.json` | log i raport z ostatniego uruchomienia |

### Raport – lista rzeczy do zrobienia

Po każdym uruchomieniu `xcl run` wypisuje podsumowanie, a `report.json` zawiera:

- `called_stubs` – niezaimplementowane funkcje, które gra **faktycznie wywołała**
  (z liczbą wywołań i miejscem, np. `gra.exe+0x1a2b3`) – w tej kolejności warto je implementować,
- `stubbed_imports` – wszystkie importy bez implementacji,
- `vfs.unmapped_paths` – ścieżki konsolowe, których nie umiemy zmapować (dopisz do `mounts`),
- `crash` – nieobsłużony wyjątek z modułem+RVA i rejestrami,
- wywołania metod D3D12.X spoza znanego układu vtable pojawiają się jako
  `ID3D12Device::#<slot>` – to dane wejściowe do etapu M2,
- `audio` – silniki XAudio2, głosy (w tym XMA2 i formaty nieobsługiwane przez Windows,
  które grają ciszę), pominięte flagi konsoli, błędy urządzenia audio.

## Testy

```powershell
.\scripts\test.ps1          # = cargo build --workspace + cargo test --workspace
```

- testy jednostkowe (parser PE, nośnik TLS, VFS, zaślepki, XTaskQueue/XAsync, XUser, null device…),
- test end-to-end `crates/xcl/tests/e2e.rs`: uruchamia **program testowy**
  (`tests/testtitle`) – własny „tytuł konsolowy”, który importuje `kernelx.dll`,
  `xgameruntime.dll`, `d3d12_xs.dll` i DLL „gry” (`tests/testgamedll`), więc na samym
  Windows się nie uruchomi. Test sprawdza 28 scenariuszy w tytule (w tym D3D12 na prawdziwym GPU, shader w formacie konsolowym i XAudio2), a potem, że folder gry
  jest **bajt w bajt niezmieniony**.
- test na prawdziwych danych (pomijany bez zmiennej): `$env:XCL_SAMPLE_RPF = 'D:\dane\shaders.rpf'`
  → pierwsze 300 shaderów paczki musi się sparsować bez błędów i niezdekodowanych dwordów.

## Jak to działa (skrót)

```
xcl run  ──►  launcher: config, foldery danych, boot.json
               └─► proces-host xcl.exe z linią poleceń gry (argv[0] = exe gry)
                     ├─ log, VFS + hooki plików (KERNELBASE), hook RtlPcToFileHeader
                     ├─ loader: mapuje exe gry → importy przez resolver:
                     │     HLE (kernelx / xgameruntime / d3d12_xs) → DLL z folderu gry
                     │     → DLL Windows → automatyczna zaślepka
                     ├─ statyczny TLS: „nośnik TLS” – generowana DLL z katalogiem TLS,
                     │     której slot zarządza Windows; XCL kopiuje szablon gry do bloku
                     │     każdego wątku (callback TLS xcl.exe)
                     └─ PEB.ImageBaseAddress = gra, start punktu wejścia na wątku z jej stosem
```

Dwa nieudokumentowane szczegóły Windows 11, ustalone testami (`tls.rs`):
loader przydziela slot TLS dynamicznie ładowanej DLL tylko wtedy, gdy ma ona importy,
i ignoruje `SizeOfZeroFill` – cały rozmiar bloku musi leżeć w zakresie „danych”.

## Źródła i licencje

- Licencja projektu: **GPL-2.0-only** (plik `LICENSE`; od 2026-11-20, wcześniej LGPL-3.0-or-later –
  zmiana, aby móc korzystać z kodu [AnyPS5](https://github.com/boykopovar/AnyPS5), GPL-2.0-only).
  Pełna lista źródeł zewnętrznych: `THIRD_PARTY.md`.
- [WinDurango](https://github.com/WinDurango/WinDurango) (LGPL-3.0) – wyłącznie referencja dla
  kernelx (stałe, struktury, zachowania) i ordinali D3D12.X Xbox One; **kodu z WinDurango nie
  wolno przenosić** (LGPL-3.0 jest niezgodne z GPL-2.0-only).
- [libHttpClient](https://github.com/microsoft/libHttpClient) (MIT) – nagłówki
  XTaskQueue/XAsync jako specyfikacja kontraktu (implementacja w XCL jest własna).
- [docs.xbox.com](https://docs.xbox.com/llms.txt) – publiczna referencja API GDK
  (sygnatury, enumy, struktury).
- [retour](https://crates.io/crates/retour) (BSD-2) – hooki inline.
- [AMD Machine-Readable GPU ISA](https://gpuopen.com/machine-readable-isa/) (MIT) – źródło
  tablic dekodera RDNA2 (`crates/xcl/src/gpu/rdna2_spec.rs`, wygenerowany).
- [LLVM](https://github.com/llvm/llvm-project) (Apache-2.0 with LLVM exception) – wektory
  testowe dezasemblera AMDGPU w `tests/data/llvm-amdgpu` (licencja obok plików).
- Repozytorium nie zawiera nagłówków ani binariów z SDK konsoli.
