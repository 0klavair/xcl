# XCL – przekazanie pracy (stan na 2026-11-20, sesja 4)

Warstwa zgodności Xbox Series X|S → Windows (bez emulacji CPU), Rust, `<repo>`.
Projekt: `docs/specs/2026-11-20-xcl-design.md` (sekcje 5.x, w tym 5.7a – format shaderów konsoli, 5.7b – stan recompilera). Instrukcja: `README.md`.
Rozmowa z użytkownikiem po polsku. Brak repo git (użytkownik nie prosił o commity).

## Środowisko
Windows 11, AMD Radeon RX 9070 (RDNA4) + iGPU, Rust 1.96 `x86_64-pc-windows-msvc`, VS 2022, Python 3.12.
Build/test: `.\scripts\test.ps1` (= `cargo build --workspace` + `cargo test --workspace`; e2e potrzebuje zbudowanego tytułu testowego).
UWAGA:
- edycje plików przez PowerShell `Get-Content/Set-Content` psują UTF-8 (polskie znaki) – używać narzędzi Write/Edit
  albo `[IO.File]::WriteAllText(..., UTF8Encoding($false))`;
- w Git Bash heredoc `<<'EOF'` przekazany do Pythona zjada backslashe (`\\n` w stringu Rusta → prawdziwy znak nowej linii,
  ścieżki `C:\U…` → błąd składni) – tekst z backslashami edytować narzędziem Edit;
- `scripts\test.ps1` uruchomiony w tle z przekierowaniem `*>` w PowerShell 5.1 kończy się błędem (stderr cargo = wyjątek przy
  `ErrorActionPreference=Stop`) – w tle puszczać przez bash: `cargo build --workspace && cargo test --workspace -- --test-threads=4`.

## Zasady od użytkownika / ograniczenia
- Pracuj na plikach przykładowych i testuj: `<samples>\shaders.rpf` (shadery),
  `<samples>\audio.rpf` (audio, 28,5 GB – czytać fragmentami!). Obok `rpf-viewer.html` –
  przeglądarka paczki (format rekordów shaderów w RPF9, parser sygnatur ISG1/OSG1).
- `<samples>\recompiler` – fragment AnyPS5 (recompiler RDNA → SPIR-V dla PS5) od użytkownika, licencja
  **GPL-2.0-only** (github.com/boykopovar/AnyPS5). **Użytkownik zgodził się na zmianę licencji XCL na GPL-2.0-only (2026-11-20)**
  – zrobione (`LICENSE`, `Cargo.toml`, `THIRD_PARTY.md`), więc logikę AnyPS5 wolno przenosić z adnotacją pochodzenia w pliku.
  Skutki: kod WinDurango (LGPL-3.0) tylko jako referencja; zależności zgodne z GPL-2 (bez czystego Apache-2.0 / GPL-3).
  Technicznie AnyPS5 to Vulkan/SPIR-V + model SRT PS5 – przydatne części: deskryptory T#/S#/V#, obrazy, interpolacja, wejścia VS.
- **Użytkownik nie ma czasu na długie kompilacje/przebiegi (2026-11-20)**: unikać kroków trwających wiele minut
  (np. `--compile` na tysiącach próbek, wielkie pomiary DXC); sprawdzać na małych próbkach, pracować szybko.

## Co działa (zweryfikowane testami)
- **Loader PE** (`loader.rs`, `pe.rs`): mapowanie, relokacje, importy + delay-load, DllMain/TLS callbacks, DLL gry.
- **Statyczny TLS** (`tls.rs`): generowana „DLL-nośnik”; odkrycia Win11: loader przydziela slot tylko DLL z importami
  i ignoruje `SizeOfZeroFill` (cały rozmiar w „raw data”).
- **Wyjątki C++**: hook `RtlPcToFileHeader` (retour) + `RtlAddFunctionTable` (`eh.rs`).
- **Resolver** (`resolver.rs`, `hle/mod.rs`): HLE → DLL gry → Windows → auto-zaślepka z logiem (`stubs.rs`, `data/stubdb.toml`).
- **kernelx** (`hle/kernelx.rs`), **XGameRuntime offline** (`hle/xgameruntime/`: XTaskQueue, XAsync, XUser, XSystem, XPackage, PLS, save folder).
- **VFS** (`vfs.rs`, `vfs_hooks.rs`): `G:\`, `GAME:`, `T:\`, copy-on-write, whiteouty, hooki KERNELBASE (łapią też CRT).
- **Launcher/host/raport** (`launcher.rs`, `host.rs`, `report.rs`, `inspect.rs`): proces potomny z linią poleceń gry;
  nie wybiera samego `xcl.exe`; ostrzega gdy exe nie importuje DLL konsoli; `report.json` (called_stubs, shaders, d3d12, vfs…).
- **Dekoder RDNA2** (`gpu/rdna2.rs` + wygenerowany `gpu/rdna2_spec.rs` z AMD Machine-Readable ISA XML, MIT; generator
  `scripts/gen_rdna2.py`; XML do pobrania z gpuopen.com/machine-readable-isa): 23 812 instrukcji z testów LLVM
  (`tests/data/llvm-amdgpu`) – 100% zgodności nazw/długości; `xcl disasm`. `rdna2::branch_target`, `rdna2::all_instructions`.
- **Rozszerzenia ISA konsoli** (`gpu/rdna2_xbox.rs`, NOWE): `v_mac_f32` (VOP2 0x1F / VOP3 0x11F), `v_madmk_f32` (0x20),
  `v_madak_f32` (0x21), `v_mad_f32` (VOP3 0x141) – zachowane z gfx10.1, usunięte w gfx10.3; dekoder bierze je tylko na
  opcody wolne w spec. Literał VALU pobierany, gdy DOWOLNE pole SRC0/1/2 = 255, także nieużywane przez opcode
  (kompilator chowa tak hash w `v_nop`) – wyświetlane jako `v_nop ; lit 0x…`. SDWA/DPP: src0 z dworda rozszerzenia (było `?249`).
- **Przechwytywanie shaderów** (`gpu/shader.rs`): klasyfikacja DXIL/DXBC/**xbox-native**/kontener nieznany/ELF/surowy RDNA2,
  zrzut do `<dane>\shaders\` (+ `*.fssmN.bin`, `*.rdna2.txt`); `report.json → shaders`: `xbox_hw_stages`, `xbox_errors`,
  `rdna2_instructions`, `rdna2_undecoded_dwords`, `rdna2_mnemonics` (histogram posortowany malejąco).
- **Format shaderów konsoli** (`gpu/xbox.rs`, NOWE) – rozpracowany na `shaders.rpf` (23 869 shaderów, wszystkie etapy),
  pełny opis w designie §5.7a: DXIL (bitcode `NOBC`) → XDAT (+0x18; offsety od nagłówka programu) → XBCH v3 (zlib od +0x18)
  → FSSM v0x101 (kod: u64 +0xC0/+0xC8, offset liczony od +0x10; deskryptory etapów `0xC0_0002_AAHH` od +0x100 co 0x88,
  HH = etap sprzętowy PAL: 2 ES, 3 GS, 4 VS, 5 PS, 6 CS, 7 MS/NGG). Podział kodu: segmenty do `s_endpgm`, doklejane do
  programu, gdy połączone skokiem (wspólne wyjście „discard” w PS, trampoliny); bloki stałych `s_endpgm | N | N dwordów |
  s_endpgm` osobno (`data_blocks`). Każdy krok ma fallback (testy syntetyczne). Wynik na całej paczce: 24 064 FSSM =
  24 064 programy, 473 bloki danych, 0 błędów parsowania, 0 niezdekodowanych dwordów w 28,97 mln instrukcji, 395 mnemoników.
- **`xcl shaders <plik|folder|paczka RPF9> [--top N] [--json]`** (`shaders.rs` + `gpu/pack.rs`, NOWE – skaner rekordów
  RPF9 jak w viewerze): formaty, etapy DXIL i sprzętowe, histogram mnemoników, „w ilu shaderach”, kolejność implementacji
  (`--json → implementation_order`) i pokrycie, lista nietypowych kontenerów (z offsetem rekordu); wielowątkowo, cała paczka
  ok. 1,7 s (release). `xcl disasm` dla xbox-native: tabela XDAT, XBCH, etapy, qwordy nagłówka FSSM, `--hex`, `--fssm PLIK`.
- **Recompiler RDNA2 → HLSL** (`gpu/recomp/`, sesja 3; opis w designie §5.7b): `translate()` → dyspozytor `switch(pc)` po
  blokach (skoki AMD są jednolite dla fali → dokładnie bez strukturyzacji), SGPR jednolite, VGPR per wątek z maską EXEC,
  maski przez `WaveActiveBallot`, cross-lane przez `WaveReadLaneAt`. `emit.rs` – semantyka: SALU (dokładne SCC), VALU
  f32/u32/i32 + modyfikatory VOP3, SDWA, DPP16/DPP8, `ds_swizzle`, `v_permlane(x)16`, `v_fma_mix*`, cube, VOPC/VOPCX,
  saveexec/wrexec, `s_load`/`s_buffer_load` (V#), MUBUF/MTBUF (surowe + 78 formatów GFX10 z LLVM `UfmtSymbolicGFX10`,
  DST_SEL), `buffer_store_dword*`, eksporty; reszta → komentarz + licznik `unsupported`. `s_swappc_b64 s[n:n+1], s[n:n+1]`
  = wywołanie fetch shadera (ignorowane w trybie kernel). Tryb **kernel** (compute `[WaveSize(64)]`, rejestry z/do bufora,
  pamięć płaska `mem`; kod programu w `mem` od 0 dla stałych z `s_getpc`). `dxc.rs` – DXC w procesie (dxcompiler.dll z
  Windows SDK / obok exe / `XCL_DXC_DIR`). `runner.rs` – D3D12 compute na GPU wysokiej wydajności. `rdna2::encode` – asembler
  do testów (też układy SDWA/DPP); `rdna2::register_scale` – NAPRAWIONY błąd dezasemblera: SBASE liczy pary SGPR, SRSRC/SSAMP
  czwórki (było `s[8:9]` zamiast `s[16:17]`). Wynik: **`VS_BlitGenAmbCube` z paczki wykonuje się na RX 9070 z poprawnym
  SV_Position**; korpus: 24 064 programy, 0 błędów tłumaczenia, **7 442 (30,9%) w całości**, DXC kompiluje 500/500 próbek.
  `xcl shaders … --recomp [--compile N]` (brakujące instrukcje: w ilu programach + „jedyna brakująca”).
  Uwaga: DXC dzieli float w przybliżeniu → końce zakresu UNORM/SNORM liczone dokładnie osobno.
- **XAudio2** (`hle/xaudio2.rs`, NOWE): moduł HLE `xaudio2_9.dll` (+ `xaudio2_9redist.dll`) → systemowe XAudio2 2.9
  (ładowane dynamicznie). Create: flagi poza 0x2000|0x8000|0x10000 pomijane (UWAGA: stara notatka „0x18002, procesor=0”
  była błędna – wg xaudio2.h 0x2 to flaga głosu, procesor = XAUDIO2_DEFAULT_PROCESSOR = 1). Łatanie vtable W MIEJSCU
  (oryginały per (vtable, slot)): IXAudio2 #5 CreateSourceVoice (XMA/XMA2 → PCM16; inny format odrzucony przez Windows,
  np. Opus 0x704F → też PCM16), #7 CreateMasteringVoice (device id konsoli → domyślne; `CO_E_NOTINITIALIZED` →
  CoInitializeEx(MTA) + ponowienie – odkryte w e2e; potem 0/0 + kategoria GameEffects), SourceVoice #21 SubmitSourceBuffer
  (cisza o długości PlayBegin+PlayLength lub oszacowanej; kontekst/flagi/pętle zachowane), #18 DestroyVoice.
  Refaktor: `HleModule.system_fallback: bool` → `fallback: &[&str]` (`hle::SELF` = „@self”) + `aliases_are_system()`;
  reszta eksportów XAudio2 (X3DAudio*, CreateFX…) → systemowe xaudio2_9.dll. `report.json → audio`.
- **Realny backend D3D12** (`hle/d3d12x/real.rs`): prawdziwe obiekty sterownika z rozszerzoną vtable; układ z `d3d12.h`
  (`scripts/gen_d3d12_layout.py` → `hle/d3d12_layout.rs`). Odkrycie: vtable sterownika mają wewnętrzne wirtualne za publicznymi
  (Device 125 vs 104) → **strażnicy** (`hle/d3d12x/guard.rs`, kod maszynowy: wywołanie z obrazu gry = metoda konsolowa → log+0,
  inaczej → oryginał). Override: QI (nieznane IID), wszystkie Create* (adopcja), PSO/root signature z fallbackiem
  (zastępcze shadery gdy sterownik odrzuca konsolowe). Fallback `null.rs` gdy brak GPU / `graphics="null"`.
- **Prezentacja** (`hle/d3d12x/present.rs`): okno XCL + swap chain; back buffer wykrywany przez RTV/OMSetRenderTargets/
  ExecuteCommandLists; ramka na wywołanie konsolowej metody kolejki (prawdopodobnie PresentX). NIEPRZETESTOWANE na grze.
- Tytuł testowy `tests/testtitle` (+`tests/testgamedll`): 26 sprawdzeń, w tym trójkąt renderowany na RX 9070 przez nasze
  `d3d12_xs` i odczyt pikseli; w sesji 2 doszły: shader w formacie konsolowym (XDAT/XBCH/FSSM z `v_mac_f32`) przy tworzeniu
  PSO → `report.json` (xbox-native, histogram) oraz XAudio2 (flagi/urządzenie konsoli, głos XMA2 gra ciszę – bufor 0,1 s
  kończy się po ~95 ms, głos PCM bez zmian, callbacki z kontekstami; bez urządzenia audio część głosowa jest pomijana).
  Stan testów (koniec sesji 3): 75 jednostkowych (w tym 11 testów recompilera wykonywanych na GPU) + e2e (28 sprawdzeń
  w tytule) – wszystkie PRZESZŁY. Testy na prawdziwych danych (`shaders::tests::retail_pack_sample`,
  `gpu::recomp::tests::gpu_retail_vs_blit_gen_amb_cube`) działają tylko z
  `XCL_SAMPLE_RPF=<samples>\shaders.rpf` (bez zmiennej pomijane) – PRZESZŁY.

## Recompiler – sesja 4 (2026-11-20)
- **Pokrycie korpusu: 99,1% programów tłumaczy się w całości** (23 846 / 24 064; było 75%): MS 9 282 / 9 282 (100%),
  PS 99,9%, VS 92,0% (brakuje tylko 195 ES scalonych z GS), CS/kernel 96,4%. DXC: 300/300 próbek OK.
- Nowe w `emit.rs`: MIMG A16/D16 (układ adresu jak AnyPS5 `componentShape`), `image_get_lod`, `_cl` (clamp LOD);
  `buffer_store_format_*` (pakowanie 78 formatów, `st_bytes` z atomikami dla niepełnych dwordów), `buffer_atomic_*`;
  global/flat load/store; LDS (`ds_read/write*`, `read2/write2(st64)`, atomiki → `groupshared`, 32 KB = limit D3D12);
  `ds_bpermute`; VALU 16-bit (op_sel, zachowanie drugiej połowy jak gfx10 / AnyPS5 `write16Bits`), porównania f16/u16/i16,
  VOP3P `v_pk_*`, `v_dot*`; 64-bit (`v_cmp_*_u64/i64/f64`, przesunięcia b64, `v_mad_u64_u32`, f64 add/mul/fma/cvt, stałe
  inline 64-bit jako double); `v_movrel*` (switch po M0).
- **Tryb Mesh** (`Mode::Mesh`, `MsInterface` z PSV0: 128 wątków, ≤128 wierzchołków/trójkątów): DXC nie pozwala na
  `WaveSize` w MS, a kod konsoli zakłada wave64 → dwa przebiegi: program jako CS `[WaveSize(64)]` zapisuje rekord grupy
  (`s_sendmsg 9` GS_ALLOC_REQ M0 = prim<<12 | vert, `exp pos0/param/prim`, wierzchołek = wątek w grupie), potem
  `mesh_passthrough()` rasteryzuje rekordy. Wejścia zmierzone na korpusie (`xcl shaders --iface`): s3[27:24] = fala w grupie,
  v5 = grupa·128 + wątek (grupa = v5>>7, jak emulacja MS gfx10.3 w RADV), s16–s24 dane użytkownika (z `io`).
  Test `gpu_mesh_mode_two_passes`: 2 grupy × 2 fale → poprawne rekordy i piksele na RX 9070 (`runner::mesh_draw`).
- Przepływ sterowania: zamiast jednej pętli `switch(pc)` łańcuch `if (pc == blok)` w kolejności adresów; `while` tylko dla
  zakresów ze skokami wstecz (scalane do zagnieżdżenia). DXIL mniejszy (470 → 310 KB na największym programie).
- **DXC: optymalizator jest superliniowy na dużych programach** (mediana 0,6 s, ale MS z wrapperem DS/VS 0,7–1,2 MB HLSL:
  50–160 s przy -O1..-O3, 5 s przy -O0) → `dxc::compile` sam dodaje `-O0` dla HLSL > 256 KB (sterownik i tak optymalizuje).
  NIE uruchamiać `--compile 3000` (godzina); do szybkiej kontroli `--compile 64`.
- Narzędzia: `xcl shaders … --recomp --example MNEMONIK|name=FRAGMENT` (listing + wynik DXC; `XCL_EXAMPLE_HLSL=plik` zapisuje
  HLSL), `--iface` → wejścia/eksporty/`s_sendmsg`/PSV0 wg etapu, czasy DXC przy `--compile N`.
  `dxbc::psv()` (PSV0), `rdna2::sgpr_uses()`. Dezasembler: VGPR w 9-bitowym polu (`v_readfirstlane … v261` → `v5`).
- Testy: 86 jednostkowych (w tym 17 recompilera na GPU) – PRZESZŁY (`cargo test -p xcl --lib`, ~0,5 s).

- Dalej w sesji 4: `src_pops_exiting_wave_id` (POPS – czekanie przepuszczane, brak porządku ROV: znane ograniczenie),
  apertury flat = 0, `image_*_o` (offset z VGPR → przesunięcie współrzędnych o offset/rozmiar mipa; test GPU) → PS 12 097 /
  12 101. **Runtime:** przechwycony shader konsoli jest od razu tłumaczony (tryb z interfejsu), `.fssmN.pM.hlsl` obok zrzutów,
  `report.json → shaders.recomp_*` (programy, pełne, tryby, brakujące mnemoniki); `XCL_RECOMP_DXC=1` kompiluje przez
  `dxc::compile_cached` (cache DXIL w `%LOCALAPPDATA%\xcl\dxil` lub `XCL_DXIL_CACHE`). e2e to sprawdza.
- Galeria: `retail_ps_gallery` (ignorowany test, `XCL_GALLERY_DIR`) – 24 shadery PS z paczki narysowane na GPU (PNG + strona
  `gallery/index.html`), ok. 10 s. Wejścia/stałe zastępcze – nie są to obrazy z gry.
- Nagłówek FSSM: `--example name=…` wypisuje go (dwordy do kodu). Stage descriptor +0x110 wygląda na RSRC2 (USER_SGPR w
  bitach 5:1, np. 0x18 → 12); od ok. +0x25C w PS tablica łatek eksportu (warianty `exp`). Mapowanie root signature → user
  SGPR robi runtime D3D12.X (SGPR niosą wskaźniki do tabel) – do ustalenia dopiero z exe gry.
- GS (195 + 195 ES) odłożony: on-chip GS – ES zapisuje do LDS i `s_setpc` do GS, GS zapisuje wierzchołki do LDS
  (64 B/wierzchołek, baza = s2 + ([s8+0xB8] << 8)), `s_sendmsg` emit/cut/done; brak copy shadera w paczce.
- Testy: 90 jednostkowych + e2e (29 sprawdzeń w tytule) – PRZESZŁY (`XCL_SAMPLE_RPF=… cargo test --workspace -- --test-threads=4`, kilka sekund).

- **UMA – pamięć konsoli dostępna dla GPU** (`gpu/uma.rs`, NOWE): jeden region (domyślnie 2048 MB, `XCL_UMA_MB`,
  maks. 4095 MB) pod adresem wyrównanym do 4 GiB, w pełni zatwierdzony, otwierany przez
  `ID3D12Device3::OpenExistingHeapFromAddress` (zmierzone: częściowy commit odrzucany, 4 GiB odrzucane, 4095 MB OK w 0,5 s).
  Dolne 32 bity adresu konsoli = przesunięcie w buforze → płaski model `mem` rekompilatora działa na pamięci gry bez kopii.
  Alokator first-fit (64 KB, scalanie, zerowanie). Trafia tam: `VirtualAlloc` z `MEM_GRAPHICS` (+ nowe HLE `VirtualFree(Ex)`),
  `XMemAlloc` z typem pamięci ≠ 0. Backend `real` otwiera region jako stertę + bufor UAV przy tworzeniu urządzenia
  (`real::uma_gpu_va()`, `report.json → d3d12.uma_*`). Testy: `gpu_unified_memory_arena` (s_load/buffer_load/store wprost na
  pamięci procesu), `uma::tests`, check w tytule testowym, e2e sprawdza `uma_gpu_va`.
  Dalej: shadery z PSO gry wiązać z tym buforem jako `mem` (u2), a SGPR wypełniać z root arguments (wymaga exe gry).

- **Bez exe (sesja 4, koniec):**
  - `gpu/desc.rs` (NOWE): T#/S#/V# GFX10 → D3D12 (format DXGI z tabeli IMG_FORMAT GFX10 – 0–77 jak bufory, 128+ sRGB/
    spakowane, 169–182 BC1–BC7; SRV z mapowaniem kanałów z DST_SEL, wymiary/mipy/tablice/cube/3D/MSAA; sampler: adresowanie,
    filtry, aniso, porównanie, min/max, LOD, bias, ramka). Pola potwierdzone z AnyPS5. Testy jednostkowe + GPU
    (`gpu_tsharp_to_d3d12_resources`: 6 rodzajów tekstur tworzonych z dekodowanych T#). Jeszcze nie używane w runtime.
  - GameInput: import `gameinput.dll` trafia do systemowego `GameInput.dll` (eksport `GameInputCreate`) – działa bez
    emulacji, sprawdzone w tytule. Gdy wersja konsolowa API okaże się inna – alias w konfiguracji / HLE.
  - XGameSave (`hle/xgameruntime/gamesave.rs`, NOWE): provider/kontenery/bloby/aktualizacje (+ Async/Result), pliki w
    `<dane>\gamesave\<configId>\<kontener>\b\<blob>`, nazwy kodowane %XX, zapis przez plik tymczasowy, limit 256 MB.
  - `xcl rpf <paczka> [--find|--extract … --out …]` (NOWE): lista/wypakowanie rekordów; `textures.rpf` – 21 zagnieżdżonych
    RPF9 z zaszyfrowanym TOC (stały klucz z gry, dane bez sygnatur zstd/LZ4/zlib – pewnie Oodle) → bez exe nie do
    odczytania; gra i tak rozpakowuje archiwa sama, XCL dostanie tekstury w pamięci (tiling GFX10).
  - Celowo NIE zrobione (czekają na exe, żeby nie zgadywać): strony fizyczne (`MapTitlePhysicalPages` pod adres gry),
    dekoder tilingu GFX10 (bez prawdziwych danych do weryfikacji), Opus przez API konsoli, GS.
  - Testy: 96 jednostkowych + e2e (29 sprawdzeń w tytule) – PRZESZŁY.

- **Tłumacz podpięty pod graficzny PSO** (`gpu/recomp/pso.rs` NOWE, hak w `hle/d3d12x/real.rs`):
  `CreateGraphicsPipelineState` z natywnym VS konsoli (i PS) → `pso::console_to_dxil` (wariant sprzętowy VS/PS z FSSM,
  tryb z interfejsu, tylko pełne tłumaczenia, DXC z cache) → PSO z przetłumaczonymi shaderami i **root signature ABI XCL**
  (`pso::abi_root_signature`: root UAV u0 io / u1 res / u2 mem + sterty bezpośrednio indeksowane; `real::abi_root_signature()`).
  Porażka → dawna ścieżka (oryginał → zastępcze shadery). `report.json → d3d12.translated_pipelines / translation_errors`.
  GS/HS/DS jeszcze nie. **Brakuje:** wiązania przy rysowaniu – argumenty root gry → SGPR w `io`, `mem` → bufor UMA
  (wymaga układu user data runtime'u konsoli, czyli exe).
- Test E2E bez exe: `crates/xcl/tests/gpu_pso_e2e.rs` – `VS_Rage_TransformPosCpv` + `PS_Rage_Untextured` z `shaders.rpf`
  przez hak urządzenia backendu → `runner::draw_on` (nowe: rysowanie gotowym PSO na dowolnym urządzeniu) → 64×64 RGBA32F;
  wejścia jawne (s8 → tabela 2 V# → macierz świata 4×3 i widok-projekcja 4×4, wierzchołki POSITION+COLOR); 435 pikseli
  wewnątrz porównanych z interpolacją liczoną na CPU (±0,01), 3501 poza = kolor tła; licznik `translated_pipelines` +1,
  `fallback_pipelines` bez zmian. Sprawdzone sabotażem: z wyłączonym tłumaczeniem test pada (i na liczniku, i na pikselach).
  `real::init()` jest teraz idempotentne (wcześniej bezpośrednie wywołanie + `backend()` tworzyło dwa urządzenia).
- Testy: 96 jednostkowych + e2e tytułu + `gpu_pso_e2e` – PRZESZŁY.

- Drugi test w `gpu_pso_e2e.rs` – tekstury przez dekoder: `VS_Rage_TransformPosCpvTex` + `PS_Rage_Textured` (T# z tabeli
  s[8:9]+0x20, S# wbudowany w kod PS = statyczny sampler: aniso 4×, wrap). Dwie liniowe tekstury 64×64 RGBA8 o znanych
  wzorach w pamięci pod adresami z konsolowych T# → `desc::image` / `image_d3d12` (zasób + SRV, dane czytane spod adresu
  z T#), S# → `desc::sampler_d3d12`; tabele w formie ABI XCL (`desc::xcl_image_words`: adres → indeks SRV w stercie).
  Jeden PSO, jedna lista poleceń, dwa draw calle różniące się tylko wskaźnikiem tabeli w user data (lewa połowa: A,
  prawa: B z DST_SEL zyxw) – `runner::draw_on_multi` + `runner::ViewTexture`/`Draw` (NOWE). 4096 pikseli = teksel (±0,005),
  2048/2048 po prawej różne od A. Sabotaż sprawdzony: DST_SEL ignorowany → pada; ta sama tabela w obu draw → pada.

## W trakcie (niedokończone)
Nic otwartego.

## Audio – wnioski z `audio.rpf` (prawdziwa gra)
- Kontenery AWC („ADAT”, wersja 1): tabela chunków u64 (typ = bajt 7, rozmiar = bity 28–55, offset = bity 0–27, offset od
  początku AWC), typy: 0x55 dane, 0x76 format (32 B: próbki u32, pętla i32, częstotliwość u16 @8, …, **kodek @28**, kanały @29),
  0x48 format strumienia (bloki 512 KB), 0xA3 seek table (próbka początku każdego bloku 2048 B), 0x36 peak, 0x81, 0x5C, 0xBD.
- **Kodek 0x0B = Opus** w 100% próbek (92 formaty + 17 strumieni z 160 miejsc paczki): 48 kHz, bloki 2048 B
  `[u16 LE długość][pakiet Opus]…` + zera, ramki 20 ms (960 próbek; TOC 0x68/0x78 Hybrid, 0xF8 CELT, mono). Liczba
  pakietów w bloku = różnica w seek table / 960 – zweryfikowane. **Brak XMA2.**
- Wniosek: dekoder XMA2 ma dla tego tytułu niski priorytet. Otwarte: czy gra dekoduje Opus sama (wtedy PCM → XAudio2 już
  działa), czy używa sprzętowego dekodowania Opus konsoli (wtedy potrzebne API – sprawdzić `xcl inspect` na exe).
  Windows ma `C:\Windows\System32\MSOpusDecoder.dll` (MFT Media Foundation) – kandydat na dekoder bez dołączania libopus.

## Shadery konsoli – otwarte pytania (z analizy `shaders.rpf`)
- Nierozpoznane metadane FSSM: +0x10 (zawsze 6), +0x78/+0x80, +0xD0, +0xF8, +0x110 (przypomina SPI_SHADER_PGM_RSRC1,
  ale bity się nie zgadzają), za kodem tablica wpisów 5×u32 (3, PC co 8 B, 1, wartość, 8 – wygląda na mapowanie PC → linia
  źródła), potem nazwa shadera; w PS od ok. +0x238 tablica łatek eksportu (warianty `exp` dla formatów RT). Do user SGPR
  i wejść potrzebne zestawienie z ISG1/OSG1/PSV0 (te części są czytelne – parser sygnatur jest w `rpf-viewer.html`).
- GS: deskryptory GS + VS (copy shader), ale w FSSM jest tylko jeden program – copy shader powstaje poza FSSM?
- VS: na początku `s_swappc_b64 s[4:5], s[4:5]` = wywołanie fetch shadera (atrybuty wierzchołków → v4…, VS_BlitGenAmbCube
  potwierdza POSITION.xy w v4/v5); w trybie VS recompiler musi go zastąpić odczytem wejść według ISG1 (układ VGPR dla wielu
  atrybutów do ustalenia na korpusie: które v≥4 są czytane przed zapisem vs maski elementów ISG1).
- VOP2 0x06 / VOP3 0x140: w gfx10.1 MAC/MAD_LEGACY (bez fuzji), w gfx10.3 FMAC/FMA_LEGACY – nie wiadomo, które ma konsola
  (dekoder używa nazw ze spec gfx10.3).
- Kilka nazw w paczce jest uciętych (nazwy > 255 znaków nie mieszczą się w bajcie długości rekordu) – kosmetyka skanera RPF9.

## Dalsze kroki (priorytet)
1. GS: 195 VS w wariancie ES (zapis wyjść do LDS + `s_setpc_b64` do części GS) i 195 GS – emulacja GS (np. CS jak w trybie
   Mesh: ES → bufor, GS czyta wierzchołki, emituje do rekordu, przekaźnik MS rysuje).
2. Model zasobów: powiązanie z `hle/d3d12x/real.rs` – user SGPR (s16–s24 w MS, s8–s18 w VS/PS) z root signature konsoli,
   T#/S#/V# w pamięci deskryptorów konsoli, pamięć GPU konsoli → zasoby D3D12; cache DXIL (kompilacja dużych MS).
3. Resztki: `image_sample_o`/`gather4_*_o` (offset z VGPR – SM 6.7 lub przybliżenie), `image_bvh*` (8 programów),
   `s_add_i32: op` (10 programów – nieobsługiwany kod operandu), mniejszy HLSL (5,7 GB na korpus).
4. Gdy użytkownik będzie miał legalny exe gry: `xcl inspect`, `xcl run`, analiza `report.json`
   (`called_stubs` z `(spoza PC)` → mapowanie slotów konsolowych np. PresentX; `d3d12.unknown_iids`; brakujące XGameRuntime;
   `shaders.rdna2_mnemonics`; `audio` – formaty głosów, `unsupported_formats`; importy związane z Opus/XMA/AcpHal).
5. Audio: zależnie od pkt 4 – dekoder Opus (MSOpusDecoder MFT lub libopus) dla głosów/API konsoli, ewentualnie XMA2
   (FFmpeg libavcodec / port XmaDecoder z WinDurango, LGPL); IXAudio2 sloty > 12 (metody tylko konsolowe) – jak w D3D12
   strażnicy, gdy gra ich użyje.
6. Kolejne API XGameRuntime (XGameSave blob API, XGameUI, XNetworking…), GameInput (routing do systemowego GameInput.dll),
   strony fizyczne pamięci (`MapTitlePhysicalPages`).
