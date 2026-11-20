# XCL – przekazanie pracy (stan na 2026-11-20, sesja 5)

Warstwa zgodności Xbox Series X|S → Windows (bez emulacji CPU), Rust, `<repo>`.
Projekt: `docs/specs/2026-11-20-xcl-design.md` (sekcje 5.x, w tym 5.7a – format shaderów konsoli, 5.7b – stan recompilera). Instrukcja: `README.md`.
Rozmowa z użytkownikiem po polsku. Repo git: `main`, jeden commit początkowy; commity tylko na prośbę użytkownika.

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

- **Emulacja GS** (`Mode::Geometry`, `GsInterface`, `recomp::geometry_program(es, gs)`): legacy on-chip GS z paczki
  (195 `GS_VoxelizeGeneric` + 195 wariantów ES `VS_*`, para po sufiksie haszu). ES i GS jednego potoku = jeden CS
  `[WaveSize(64)]`, grupa = do 21 trójkątów (63 wierzchołki ES); `s_setpc_b64` ES → kod GS; LDS = `groupshared`;
  element ESGS = 16 B na rejestr OSG1 ES (zmierzone: wariant 6be01b34 zapisuje co 16 B, 912dfba0 co 32 B – sterownik konsoli
  wylicza to z ES); wejścia GS: v0/v1 = offsety wierzchołków w dwordach (16-bit spakowane), s3 = wierzchołki | prymitywy<<8,
  s2 = obszar wyjścia GS po 64 elementach ES; `s_sendmsg` emit/cut → trójkąty z pasa (nieparzyste z zamianą), baza wyjścia
  wątku = najniższy adres zapisany przez niego w fazie GS (nie zależy od wartości spod [s8+0xB8]); wynik → rekord trybu mesh →
  `mesh_passthrough`. Atrybuty ES pobrane wcześniej: float4 na rejestr ISG1 w `mem` pod io[111], liczba prymitywów io[110].
  **Założenia do sprawdzenia na grze:** wartość [s8+0xB8] (baza wyjścia GS, w teście 0), znaczenie s19/s21/s16/s17 (dane
  użytkownika), tylko trójkąty → triangle strip, brak PSO/rysowania GS w backendzie (potrzebne przechwytywanie list poleceń).
- **Naprawiony błąd tłumacza:** `ds_read*` z rejestrem docelowym pokrywającym adres (`ds_read2_b32 v[0:1], v0`) liczył drugi
  adres z nadpisanego rejestru – teraz adres najpierw do zmiennej. Ujawnił go dopiero prawdziwy GS; test regresji
  `gpu_ds_read_destination_overlaps_address`.
- Korpus: **24 056 / 24 064 (99,97%)** – GS 390/390, MS, PS, VS 100%; zostało 8 CS z `image_bvh*_intersect_ray`.
- Testy GS: `gpu_geometry_mode_es_gs_strip` (syntetyczny ES+GS: 25 trójkątów = 2 grupy, pas 4 wierzchołków, każdy wierzchołek
  i indeksy dokładnie, rysowanie) i `retail_es_gs_voxelize_geometry_mode` (prawdziwa para z paczki: rzut na płaszczyznę osi
  dominującej dokładnie – z → (x, y), x → (z, y), głębokość 0,5·d+0,5, oś 4/0, TEXCOORD1 = (-1,-1,1,1), TEXCOORD3 = normalna
  jednostkowa, 496 pikseli). Wszystkie testy: 98 jednostkowych + e2e + 3 w `gpu_pso_e2e` – PRZESZŁY.

- **Ray tracing: `image_bvh(64)_intersect_ray`** (8 programów: `CS_RTShadowTrace*` wave32, `S_ScreenProbes_TraceRays_BVH`):
  funkcja HLSL `bvh_intersect` w PRELUDE_MEM wg publicznego formatu RDNA2/RADV – węzeł = id (typ w bitach 2:0) pod
  baza + (id & ~7)·8, baza z deskryptora w jednostkach 256 B (słowo 0), sortowanie = słowo 1 bit 31; box32 (5) / box16 (4,
  f16): test slab 4 dzieci, trafione posortowane po odległości (sieć 5 porównań), chybione 0xFFFFFFFF; trójkąt (0..3):
  (t, mianownik, i, j) jako liczniki (Möller–Trumbore), chybienie t = inf; instance/AABB → 0xFFFFFFFF (robi to shader).
  A16 (dir/inv_dir jako połówki) obsłużone. Kod gry potwierdza układ (adres węzła = (id >> 3) << 6, triangle_id pod +48).
  Test `gpu_bvh_intersect_ray` (ręczny BVH, 3 rodzaje promieni, sortowanie/bez, box16, trafienie t=5 i=j=0,5, chybienia,
  wariant 64-bit). Przybliżenia: brzegowe przypadki NaN/równości i wariant watertight sprzętu, tryb zwrotu inny niż IJ.
- **Korpus: 24 064 / 24 064 (100%)** programów tłumaczy się w całości; DXC kompiluje oba programy RT.
  Testy: 99 jednostkowych + e2e + 3 w `gpu_pso_e2e` – PRZESZŁY.

## Sesja 5 (2026-11-20) – wszystko, co da się zrobić bez exe
- **Tryb Compute** (`Mode::Compute`, `CsInterface::from_descriptor`): COMPUTE_PGM_RSRC2 z deskryptora etapu +0x38
  (USER_SGPR bity 5:1, TGID_X/Y/Z bity 7/8/9, TG_SIZE bit 10, TIDIG_COMP_CNT bity 12:11), numthreads z +0x3C/+0x40/+0x44
  (= PSV0 we wszystkich 36 sprawdzonych CS); id grup za user SGPR, v0..v2 = id wątku. `runner::dispatch(dxil, grupy, sgpr, mem)`.
- **Szerokość fali VS/PS** (wybiera ją sterownik PC): program wave32 w fali 64 działa jako dwie połówki (`xcl_w32`,
  `xcl_half`, `rla()`, `ballot()` świadome połówki); Kernel z `hw_wave == 64 && !wave64` tak samo. Program wave64 w fali 32
  psuje się tylko przy readlane linii ≥ 32, linii z SGPR i ds_bpermute – `recomp::wave_hazards` to raportuje, nie naprawia.
  Korpus: 1127 wave32 VS/PS obsłużonych, 749 wave64 z ryzykiem (447 linia ≥ 32, 353 linia z SGPR).
- `xcl shaders … --precompile [--limit N]`: DXIL do cache (`dxc::compile_cached`, `%LOCALAPPDATA%\xcl\dxil` albo
  `XCL_DXIL_CACHE`); 60 rekordów w 4,6 s, powtórka 0,2 s (z cache).
- **Wejścia VS:** SV_VertexID / SV_InstanceID z rejestrem w ISG1 trafiają jak atrybuty do v[4 + 4r] (`VInstanced*` czytają
  id instancji w v8) – NAPRAWIONE, test `vs_system_values_in_isg1_registers`. Analiza `--iface` liczy z VADDR MUBUF/MTBUF
  tylko używane rejestry (IDXEN + OFFEN) – większość „odstających” VS to były fałszywe alarmy (górna połowa 64-bit VADDR).
  Układ v4 + 4·rejestr: 779 / 788 VS. Zostało 9 `VS_PassthroughComposite_*_Wrapped`: `image_load v[8:11]` przy ustawionym
  tylko v8 – analiza liczy 4 adresy bez patrzenia na DIM, prawie na pewno też fałszywy alarm.
- **Strony fizyczne** (`hle/kernelx.rs`): Allocate/Free/MapTitlePhysicalPages w UMA – strony 64 KiB, przydział = ciągły blok
  UMA, numer strony = offset / 64 KiB; mapowanie kolejnych stron zwraca ich adres w UMA (widoczny dla GPU, bez kopii);
  mapowanie pod adres wybrany przez grę lub rozproszonych stron → odmowa + log (aliasowanie wymagałoby sekcji pamięci).
  Test `physical_pages_in_uma`.
- **Tiling GFX10** (`gpu/tiling.rs`; `gpu/tiling_tables.rs` z `scripts/gen_tiling.py` – wzorce AMD addrlib z Mesy, MIT, źródła
  ściągane z gitlab.freedesktop.org do scratchpada): 14 trybów 2D (256 B / 4 KB / 64 KB: S, D, S_T, D_T, S_X, D_X, Z_X, R_X)
  × 5 rozmiarów elementu × konfiguracja pipe/packer (tablice RB+, gfx10.3). Offset liniowy nad GF(2): `ox[x] ^ oy[y]`;
  `Layout::{new, offset, detile, tile, size}`, `tiling::element(IMG_FORMAT)`, `tiling::image_layout(&desc::Image, cfg)`
  (`base_xor` z niskich bitów adresu T#). **Zweryfikowane na prawdziwych danych:** wzorce, które przeglądarka TTPF
  (`tools/ttpf-viewer/src/tile.rs`) znalazła ręcznie na teksturach gry, to bit w bit SW_64KB_D (bloki 64 KB) i SW_4KB_S
  (bloki 4 KB) dla elementów 1/2/4/8/16 B. Wszystkie tryby × konfiguracje dają permutację bloku (test). Niewiadome:
  konfiguracja pipe/packer konsoli (GB_ADDR_CONFIG) dla trybów _X – parametr `PipeConfig`, do ustalenia na teksturze z gry
  (np. wybrać konfigurację, która daje najgładszy obraz); łańcuch mipów i mip tail – nie (każdy poziom osobno).
  `StandardSwizzle64KBSupported` na RX 9070 = 0, więc untiling i tak zostaje po naszej stronie.
- **Builder BVH** (`gpu/bvh.rs`): BLAS w formacie RDNA2 (box32 128 B + trójkąt 64 B, id prymitywu @48, geometrii @52),
  podział po medianie centroidów na najdłuższej osi, 4 dzieci na węzeł, 1 trójkąt na liść. `bvh::intersect_node` to lustro
  CPU emulacji HLSL, `Bvh::trace` to trawersacja stosowa do najbliższego trafienia. Testy: trawersacja = brute force
  (300 trójkątów, 2000 promieni) i `gpu_built_bvh_nodes_match_the_cpu_node_test` (64 linie, węzły z buildera przez
  przetłumaczony `image_bvh_intersect_ray` = wynik CPU). Brakuje: węzłów instancji (TLAS – układ sterownika konsoli, przechodzi
  go kod shadera, do odczytania z `CS_RTShadowTrace*`), box16 w builderze, podpięcia pod `BuildRaytracingAccelerationStructure`.
- **Opus odłożony do exe** (decyzja użytkownika: „okaże się przy exe”). Patrz sekcja Audio.
- Testy: 111 jednostkowych (7 ignorowanych pomocniczych) + e2e tytułu + 3 w `gpu_pso_e2e` – PRZESZŁY (pełny `cargo test --workspace`).

### Konsolowe DLL z paczki gry (sesja 5, bez exe)
Użytkownik dał 16 DLL z chunku 1002 (`<samples>\*.dll`, zwykłe PE): CRT konsoli (vcruntime140(_1),
msvcp140(_1/_2/_atomic_wait/_codecvt_ids), concrt140, vcomp140, vccorlib140), Bink2Scarlett, XCurl, libHttpClient.GDK,
amd_fidelityfx_loader_xbox / amd_fidelityfx_upscaler_xbox, XboxBvhBuilder. Exe gry (`game_scarlett_master.exe`) nadal brak.
- `xcl inspect`: po poprawkach **0 brakujących importów we wszystkich 16**. Dopisane:
  - `XMemVirtualQuery(proces, adres, info) → BOOL` (sygnatura z miejsc wywołania w XboxBvhBuilder, objdump z msys64):
    info = MEMORY_BASIC_INFORMATION + pole @48, bity 0–1 = pamięć czytelna dla CPU (komunikat przy błędzie:
    „pDesc->SourceAccelerationStructureData must be non-null and point to CPU-readable memory”);
  - `XMemGetWorkingSetStatistics(alokator, stats) → FALSE` (upscaler FSR czyta +8 tylko przy sukcesie → raportuje 0);
  - ordynały `xaudio2_9.dll` w `data/stubdb.toml` (z systemowej DLL; Bink importuje #1 = XAudio2Create).
- `crates/xcl/tests/console_dlls_e2e.rs` (`XCL_SAMPLE_DLLS` = folder z DLL, bez niej test pomijany):
  - CRT konsoli ładuje się loaderem XCL (importy HLE/Windows/DLL gry, TLS, DllMain) i **działa**: memcpy, `std::mutex`
    (`_Mtx_*`) z 2 wątkami (40 000 inkrementacji), `_Thrd_hardware_concurrency` / `omp_get_num_procs` /
    `Concurrency::GetProcessorCount` = liczba rdzeni;
  - pozostałe DLL (Bink, XCurl, libHttpClient.GDK, FFX, XboxBvhBuilder, msvcp140_*, vccorlib140): mapowanie + TLS + DllMain OK;
  - **prawdziwy builder BVH konsoli** (`GetRaytracingAccelerationStructurePrebuildInfo` liczy na CPU; wyzerowany obiekt
    = ustawienia domyślne) – rozmiar wyniku pasuje dokładnie (17/17 przypadków) do
    `align_1K(256 + N·liść + max(N−1,1)·128 + 4 B na każde 64 B węzłów)`: liść BLAS (trójkąt) 64 B, liść TLAS
    (**węzeł instancji**) 128 B, box32 128 B, nagłówek 256 B, wskaźnik rodzica 4 B na 64 B (układ jak AMD GPURT / RADV).
- XboxBvhBuilder.dll = klasa C++ `XboxBvhBuilder(ID3D12Device11*, bool)`: Build, BuildBatched2X, Copy, GetPrebuildInfo,
  `Rehydrate*` (wczytywanie gotowych BVH – pewnie dane z `*_bvh4.rpf`), Set64BitMortonCodes, CpuValidateResultX. Buduje na GPU
  (D3D12SerializeVersionedRootSignature → shadery compute w DLL).
- **Format AS odczytany z kodu RT gry** (`CS_RTShadowTrace32TemporalHQ_…_0c84ea88`, listing przez `xcl shaders … --example`):
  - adres AS z deskryptora (`s_load_dwordx4 [s80:81+0x40]`, słowa 0–1 >> 8 = baza dla `image_bvh`), **korzeń = dword @0x30
    nagłówka** (`s_load_dword s55, [baza+48]`); id węzłów liczone od początku AS (nagłówek wliczony); korzeń typu 6
    (instancja) też obsłużony;
  - **węzeł instancji (typ 6, 128 B)**, czytany od +48: @48 id (24 b) | maska << 24 (test `& 0xE5`), @52 contribution
    (24 b) | flagi << 24, @56 korzeń BLAS (30 b; 0x3FFFFFFF = brak) | bit 31 = sama translacja (shader dodaje t, nie
    obraca), @60 adres BLAS >> 8, @64 świat → obiekt jako [t.xyz, wiersz 0, wiersz 1, wiersz 2] (o' = M·o + t,
    d' = M·d); trójkąt BLAS: id prymitywu @48 (zgodne). Shader czyta też nagłówek BLAS @52 i @56
    (`[52] >> 6 − [56] >> 2`) – znaczenie nieznane, piszemy 0. Założone (shader ich nie czyta): @0 obiekt → świat 3×4
    jak w D3D12_RAYTRACING_INSTANCE_DESC, @112..127 zero.
  - `gpu::bvh`: nagłówek 256 B z korzeniem @0x30, korzeń zawsze box32, `build_top(&[Instance])` (TLAS, pudełka instancji =
    przekształcone granice BLAS, macierz odwrotna w f64), `Bvh::trace_scene(lookup adresu BLAS, …)` = trawersacja jak
    w shaderze, `Bvh::bounds`. Testy: `scene_trace_matches_brute_force` (3 BLAS, 24 instancje obrócone/skalowane/
    przesunięte, część tylko przesunięta, 3000 promieni = brute force po trójkątach w świecie), `instance_node_layout`
    (każde pole), rozmiary ≤ model konsoli bez wskaźników rodzica. Wskaźników rodzica nie piszemy (miejsce nieznane,
    żaden widziany shader ich nie czyta).
- **FSR – decyzja (sesja 5): nie emulować konsolowego FSR4.** Upscaler z paczki to model ML w fp8
  (`fsr4_model_v07_fp8_…`) z pipeline'ami skompilowanymi pod konsolę (części `ACCC`, `DeserializeComputePipelineStateX`,
  „ACO-cooked”, z zapasową ścieżką „NewBE”). Gra nie potrzebuje go do działania – przy exe: HLE API FFX
  (ffxCreateContext/ffxDispatch → prosty upscaling naszym shaderem albo przekazanie do PC-towego FSR na RDNA4).
  Tabele deskryptorów i tekstury – do ustalenia na grze.
- FSR: loader ma standardowe API FFX (ffxCreateContext/Configure/Query/Dispatch/DestroyContext) jak PC-towy
  `amd_fidelityfx_loader_dx12.dll`; upscaler to **FSR4** (`…fsr4_full_720p1080p_input_sanitize_scarlett_opt2.dll`). Plan:
  alias loadera → FFX DX12 z PC (FSR4 na RDNA4 = RX 9070 użytkownika); sprawdzić dopiero z exe.
- **Bink działa:** prawdziwa `Bink2Scarlett.dll` pod XCL dekoduje film z `movies.rpf` (`bink2_console_dll_decodes_a_game_movie`,
  `XCL_SAMPLE_BK2` = wyciągnięty `.bk2`: BinkOpen → BinkDoFrame/BinkCopyToBuffer(BINKSURFACE32 = 3)/BinkNextFrame; 1280×720,
  60 klatek, obraz poprawny – sprawdzony na oko). Filmy w paczce: Bink 2 rewizja `n`, 720p/1080p, 30 fps, nieskompresowane w RPF.
- **`core_bvh4.rpf` = skompresowane BLAS w formacie konsoli** (sesja 5): zasób RAGE = nagłówek wirtualny 208 B + blok
  fizyczny (osobne strumienie zlib); blok fizyczny +0x100 = źródło dla `XboxBvhBuilder::Rehydrate…X` (funkcja czysto CPU,
  ignoruje urządzenie): dword 0 = 0x81020006 (bit 24 = skompresowane, rehydracja go zeruje), @0x04 1503 (znaczenie?),
  @0x08 CRC32 (tablica standardowa, start 0, bez końcowego XOR) z NumDescs + per geometria Flags, IndexFormat, IndexCount,
  VertexCount (sprawdza tylko Validate…), @0x0C liczba węzłów trójkątów, @0x10 AABB, @0x28 rozmiar, @0x30 korzeń,
  @0x34 offset pierwszego węzła trójkątów, @0x38 ?, @0x3C wykładniki skali per oś (pozycje w BVH = pozycja · 2^(e−127)).
  Blob nie ma wierzchołków – rehydracja czyta bufory wierzchołków / indeksów gry z opisów geometrii (standardowy
  D3D12_RAYTRACING_GEOMETRY_DESC; bez indeksów 3p+k). Test `xbox_bvh_builder_rehydrates_game_geometry`
  (`XCL_SAMPLE_BVH4` = paczka, zasób zlib @0xa4e26070, syntetyczny bufor wierzchołków): 4 geometrie (644/252/744/4
  trójkątów), 287 box32, 54 box16, 832 węzły par – każdy trójkąt sprawdzony (wierzchołki, obrót I/J, prymityw unikalny,
  zakresy prymitywów bez dziur).
- **Format węzłów = AMD GPURT gfx10** (źródła MIT pobrane do scratchpada: `TriangleNode1_0.hlsli`, `IntersectCommon.hlsl`,
  `Common.hlsl`), potwierdzony na wyjściu konsolowego buildera i kodzie RT gry:
  - box16 (typ 4) w użyciu obok box32; trójkąt = **para**: v0..v3, typ 0 = (v0, v1, v2), typ 1 = (v1, v3, v2); @52/@56
    prymityw 0/1 (indeks w obrębie geometrii), @60 triangle id (bajt na trójkąt: bity 1:0 I SRC, 3:2 J SRC, 7 opaque) –
    sprzęt przestawia barycentryki (`SwizzleBarycentrics`);
  - **@48 w wersji Xbox: geometria << 8 | drugi trójkąt << 7 | opaque** (różni się od GPURT): box-y wskazują tylko typ 0,
    shader gry po teście typu 0 sprawdza bit 7 (`v76 = [węzeł+48] >> 7`) i testuje `węzeł | 1`; bit 0 idzie do decyzji
    opaque/any-hit.
  - Poprawione: emulacja HLSL `bvh_intersect` (typ 1, przestawianie barycentryk wg @60), lustro CPU (`bvh::intersect_node`:
    box16, pary), `Bvh::trace` (bit 7 → typ 1), builder pisze @48 = geometria << 8 | 1, @52 prymityw, @56 0xFFFFFFFF,
    @60 = 0x89. Testy: `gpu_triangle_pair_and_box16_nodes` (GPU = CPU, barycentryki obróconego trójkąta 0 i trójkąta 1 dają
    dokładny punkt trafienia), stare testy BVH z triangle id 9.
- **XboxBvhBuilder na urządzeniu XCL (sonda, `XCL_PROBE_BVH_DEVICE=1`, test `xbox_bvh_builder_on_xcl_device_probe`)**:
  konstruktor na prawdziwym backendzie (RX 9070) → `D3D12SerializeVersionedRootSignature` z **flagami tylko konsolowymi
  0x3000000** (Windows: „Unsupported bit-flag set”) – HLE zdejmuje bity spoza 0xFFF i loguje → root signature tworzona →
  potem proces kończy się 0xC0000409 (fail-fast, bez wyjątku SEH – podejrzenie: metoda urządzenia zapisuje większą
  strukturę niż konsolowa / ciasteczko /GS). `QueryInterface(0742a90b-c387-483f-b946-30a7e4e61458)` w logu nie pochodzi
  z buildera (raczej runtime PC). **Następny krok:** śledzić wywołania vtable urządzenia z buildera (który slot, argumenty).
- `OutputDebugStringA/W` (kernelx HLE) → log XCL `[debug] …` (konsolowe DLL i gry dużo tam piszą; tak wyszedł błąd XDXR).
- **Konstruktor XboxBvhBuilder przechodzi w całości na urządzeniu XCL** (sonda `XCL_PROBE_BVH_DEVICE=1`):
  - `XCL_D3D12_TRACE=1` (NOWE, `guard::make_tracer`): każda publiczna metoda vtable wołana z kodu konsoli → log
    `D3D12 trace: Iface::Metoda (#slot) ← moduł+offset`. Tak wyszła sekwencja CreateRootSignature → SetName → slot 62.
  - **Konsolowy vtable urządzenia różni się od PC:** slot 62 na konsoli = `CreateComputePipelineState(desc, NULL, NULL,
    riid, ppv)` (dwa dodatkowe argumenty), na PC = CreateStateObject. `guard::make_split` (NOWE): wywołanie z obrazu gry →
    wersja konsolowa, z runtime'u → PC; tabela `real::console_slots` (na razie tylko urządzenie #62).
  - Compute PSO z konsolowym CS → `pso::console_to_dxil(…, Stage::Compute)` (cs_6_6) + root signature ABI XCL
    (`real::translated_compute_pso`). Konsolowego bajtkodu nie wolno podawać sterownikowi: kompilator AMD (amdxc64.dll)
    wywala się na DXIL z bitcode NOBC zamiast go odrzucić – nieudane tłumaczenie → od razu shader zastępczy.
  - Nowe instrukcje w tłumaczu (z shaderów buildera, nie było ich w `shaders.rpf`): `global/flat_atomic_*` (add, sub, and,
    or, xor, swap, s/umin, s/umax, cmpswap, **fmin/fmax pętlą CAS**), `buffer_load_ubyte/sbyte/ushort/sshort`,
    `buffer_store_byte/short`, `ds_write_b8/b16`, `ds_read_u8/i8/u16/i16` (atomowe AND/OR na groupshared). Wynik: 53/53
    shaderów buildera przetłumaczonych, 0 zastępczych. **Brak testów jednostkowych tych instrukcji** – do dopisania.
  - **INCYDENT:** największy shader buildera (1589 instrukcji, HLSL < 256 KB → pełna optymalizacja DXC) kompilował się
    ~6 min i wyczerpał 38 GB pamięci zadeklarowanej (32 GB RAM + 7 GB pliku stronicowania) – Windows ubił sesję
    użytkownika (dziennik: „System ma za mało pamięci wirtualnej”, 23:27). Próg `-O0` w `dxc::compile` obniżony z 256 KB
    do 48 KB HLSL. DXIL tego shadera (314 KB) jest już w cache DXC. Przed kolejnymi sondami z dużymi shaderami: limit
    pamięci procesu testu (Job Object) albo kompilacja DXC w osobnym procesie.
  - Testy nowych instrukcji: `gpu_global_atomics_and_sub_dword_access` (64 linie na tych samych adresach: add z GLC,
    sub, fmax/fmin od ±inf, smax; short/byte w buforze i LDS z sąsiadami w jednym dwordzie, odczyt sąsiada po barierze).
  - Testy z konsolowymi DLL działają pod **limitem pamięci 8 GB** (Job Object w `setup`, test `memory_cap_is_active`).

### Wiązanie argumentów root na konsoli (ustalone na 52 shaderach XboxBvhBuilder.dll)
- **s[0:1] = wskaźnik (40 bit, s1 & 0xFF) do tablicy argumentów root**: parametry spakowane w kolejności z root signature,
  stała root = 4 B, deskryptor root (CBV/SRV/UAV) = 8 B adresu GPU, bez wyrównania (UAV po 19 stałych leży pod 76).
  Shader sam składa V# z adresu (num_records −1). Sprawdzone automatycznie: 123 odczyty z tablicy w 50 shaderach – wszystkie
  na granicach parametrów (skrypt `check_layout.py` w scratchpadzie; shadery wyciągane z DLL po sygnaturze DXBC).
  Tabele deskryptorów – jeszcze nie widziane (builder ich nie używa). Id grup za user SGPR (s4 = TGID.x przy 4 user SGPR).
- Dla XCL: przy dispatchu z przetłumaczonym PSO – złożyć tę tablicę z zapamiętanych argumentów root w UMA, s0/s1 = jej adres
  w `io`, reszta jak w trybie Compute.

### D3D12.X: lista poleceń jest częściowo „wkompilowana” w kod tytułu (sonda `XCL_PROBE_BVH_RECORD=1`)
- Nagranie `BuildRaytracingAccelerationStructure` na liście z urządzenia XCL: builder woła **metodę konsolową listy #148**
  = `AllocateUploadMemory(rozmiar, wyrównanie) → wskaźnik CPU` (u nas strażnik zwraca 0 → „D3D12 XDXR Error: Failed to
  allocate upload buffer” → wyjątek C++), a potem **pisze bezpośrednio do wnętrza obiektu listy**: +0x18 wskaźnik zapisu,
  +0x30 koniec bufora (dopisuje rekord 28 B), +0x140 flaga, +0x544 stan root signature; slot 33 to tu raczej
  SetComputeRootSignature (na PC #29). W kodzie nie ma nagłówków PM4 – rekordy są w programowym formacie D3D12.X,
  zamieniane na polecenia GPU przez sterownik konsoli przy Close/Execute.
- **Wniosek architektoniczny (dotyczy też gry):** podmiana vtable na prawdziwym obiekcie PC nie wystarczy – kod konsoli
  zapisałby po wnętrzu obiektu sterownika PC. Kod konsoli musi dostawać **własne obiekty listy XCL** o konsolowym układzie
  pamięci (vtable przekazujący do listy PC + obszar „cienia” na pola i bufor rekordów), a XCL musi **interpretować rekordy**
  przy Close/ExecuteCommandLists. Układ pól i format rekordów do ustalenia z kodu inline (najpierw builder: tylko compute,
  potem gra). To prawdopodobnie największy pozostały kawałek pracy nad renderowaniem.

### Format listy poleceń D3D12.X (compute) – ustalony na XboxBvhBuilder.dll
Metoda: atrapa listy (`xbox_bvh_builder_records_into_a_fake_console_list`, `XCL_PROBE_BVH_FAKELIST=1`: vtable 600 thunków
logujących slot i argumenty, #148 przydziela z UMA, +0x18/+0x30 = bufor rekordów) – builder nagrał całą budowę BLAS bez
awarii; plus przegląd statyczny wszystkich 141 miejsc zapisu (`record_writers.py` w scratchpadzie).
- **Pola obiektu listy:** +0x00 vtable, +0x18 wskaźnik zapisu, +0x28 granica dla rekordów dispatch, +0x30 koniec bufora,
  +0x140 flaga „brudne” (ustawiana, gdy nagłówek ma bity 31:30 = 0), +0x141 bit z pipeline (bit 20 słowa +0x20 rekordu),
  +0x143 flaga, **+0x344 + 4·i = nagłówek parametru root i** (wpisuje go metoda – u nas #33, najpewniej
  SetComputeRootSignature – kod inline tylko go kopiuje), +0x544 bieżący pipeline (16 B).
- **Rekordy** (bez wspólnej ramki; długość wynika z nagłówka / rodzaju):
  - pipeline 28 B (×34): kopia +0x10..+0x2B obiektu „info” buildera = [wskaźnik PSO (nasz adoptowany obiekt)][wskaźnik
    obiektu D3D12Core][8 B flag][4 B]; zastępczo slot 25;
  - parametr root i: nagłówek = [+0x344 + 4i] (+ (n−1) << 16 dla wielu stałych), potem: deskryptor = adres 64-bit
    (12 B, zastępczo slot 43/45), jedna stała (8 B, slot 37), n stałych (4 + 4n B, slot 39: (i, n, dane, offset));
  - Dispatch: `0x4002C400` (albo `(0x1000BA00 | flaga) << 2`) + x, y, z = 16 B, zastępczo slot 14 (= PC Dispatch);
  - `0x4003CC00`, 20 B, zastępczo slot 80 (pewnie dispatch pośredni);
  - metody: #148 AllocateUploadMemory(rozmiar, wyrównanie) → wskaźnik CPU (= adres GPU w UMA), #146 bariera
    (1, wskaźnik na opis), #33 (wskaźnik obiektu – root signature / PSO), #14 Dispatch.
- Nagłówki PM4 typu 3 nie występują jako stałe – nagłówki parametrów pochodzą z listy, więc **XCL może wpisać tam własne
  znaczniki** (np. 0xC0000000 | i << 8 | znacznik) i jednoznacznie czytać strumień; rekord pipeline rozpoznawać po znanym
  wskaźniku PSO.

### ZROBIONE: konsolowa lista poleceń (compute) – konsolowy builder BVH działa na GPU PC
`hle/d3d12x/console_list.rs` (NOWE, włączane `XCL_CONSOLE_LISTS=1`): test `xbox_bvh_builder_builds_on_the_gpu`
(`XCL_CONSOLE_LISTS=1 XCL_PROBE_BVH_GPU=1`, ~1 min przy pustym cache DXC, potem sekundy) – **prawdziwy XboxBvhBuilder.dll
buduje BLAS z 64 losowych trójkątów na RX 9070 przez XCL**: kod inline D3D12.X nagrywa do listy w układzie konsoli,
`console_list::close` zamienia rekordy na polecenia PC (10 dispatchy przetłumaczonych shaderów konsoli, 9 barier, 0
nieznanych rekordów), wynik czytany `bvh::Bvh::trace` = brute force na 500 promieniach.
- Obiekt listy: vtable 600 slotów (znane: 0–2, 14 Dispatch, 25 SetPipelineState, 33 SetComputeRootSignature,
  37/39 stałe, 41/43/45 deskryptory, 146 bariera, 148 AllocateUploadMemory z UMA; reszta – logujące zaślepki), pola jak
  w formacie wyżej, rekordy w kawałkach po 1 MB (zapas 4 KB za granicą, bo kod inline sprawdza `write < end` i dopiero
  pisze).
- Nagłówki parametrów root: `0xC0000000 | i << 10`; kod inline dodaje **przesunięcie stałej · 4** (bity 9:2) i
  **(liczba − 1) << 16**; własne znaczniki `0x8C0FE0xx` (pipeline, root signature, bariera); dispatch `0x4002C400` /
  `0x4002E800|4`.
- PSO dla kodu konsoli: obiekt w kształcie konsoli (`wrap_pso`, pod +0x10 znacznik + wskaźnik na siebie, metody 3–8 →
  PSO PC); tworzone na ścieżce konsolowej (urządzenie slot 62), gdy listy konsolowe są włączone. Układ argumentów root
  zapamiętywany przy CreateRootSignature (`note_root_signature`, parser RTS0).
- Dispatch: tablica argumentów root (układ z RTS0) w UMA, `io[0..1]` = jej adres, root signature ABI XCL, u0 io / u1 res /
  u2 `mem` = cały bufor UMA, PSO przetłumaczony.
- **Ważne: `mem` w trybie Compute jest `globallycoherent`** – bez tego budowa drzewa od dołu (atomiki + odczyt tego, co
  zapisały inne grupy) dawała złe wskaźniki dzieci (na konsoli kod używa GLC / unieważniania cache). Grafika ma zwykły widok.
- **Podpięte automatycznie** (gdy `XCL_CONSOLE_LISTS=1`): urządzenie #12 CreateCommandList – thunk split: kod konsoli
  dostaje listę konsolową (początkowy PSO rozpakowany do PC), kod XCL listę PC; lista: #9 Close (interpretacja),
  #10 Reset, #3–8 → lista PC; kolejka ExecuteCommandLists zamienia listy konsolowe na PC; pamięć UMA nagrania zwalniana
  przy Reset jej alokatora (`alloc_reset` → `console_list::allocator_reset`). Test robi 2 rundy przez vtable (64 i 200
  trójkątów, Reset alokatora i listy między nimi) – obie zgodne z brute force.
- Jeszcze nie: rekord `0x4003CC00` (dispatch pośredni, slot 80), opis bariery #146 (teraz zawsze globalna
  UAV), tabele deskryptorów, grafika (rekordy rysowania – do ustalenia na grze), zwalnianie pamięci UMA po wykonaniu
  (lista trzyma bloki do Release).

### TLAS na GPU – DZIAŁA (sesja 5): format węzła instancji potwierdzony na wyniku konsoli
- `xbox_bvh_builder_builds_a_tlas_on_the_gpu` (`XCL_CONSOLE_LISTS=1 XCL_PROBE_BVH_TLAS=1`): konsolowy builder buduje na GPU
  2 BLAS + TLAS z 16 instancji (obrócone/skalowane i tylko przesunięte), `bvh::Bvh::trace_scene` (układ instancji z kodu
  RT gry: @48 id|maska, @56 korzeń BLAS | bit „tylko translacja”, @60 adres BLAS >> 8, @64 świat→obiekt [t, wiersze])
  = brute force w świecie na 800 promieniach. Odblokowało to **wyrzucenie gałęzi z generowanego HLSL** (niżej).
- **Kod bez gałęzi (najważniejsza zmiana wydajności tłumacza):** w HLSL 2021 `?:`, `&&`, `||` na skalarach są skracane,
  więc każde było prawdziwą gałęzią – `lanebit` (aktywność linii, przy każdej instrukcji), `scc = (…) ? 1u : 0u`,
  `v_cndmask`, DPP/permlane, warunki zakresu buforów. Teraz `select()`/`and()`/`or()`/`(uint)(bool)` (odczyty/zapisy
  pamięci nadal za `if`), zapis VGPR `v = select(aktywna, wartość, v)`, ścieżka VALU bez nazwanej zmiennej tymczasowej.
  Efekt na shaderach buildera: 15 720 instrukcji – DXC `-O3` 5,8 s, 585 KB DXIL (wcześniej > 150 s / sterownik:
  brak pamięci); shader z wczorajszej awarii (1 589 instr.) – 0,3 s, 48 KB (wcześniej ~6 min i > 30 GB).
  **DXC znów optymalizuje wszystko** (`-O0` tylko > 8 MB HLSL). Próbka 64 shaderów gry: 0 błędów; mediana 0,29 s, p90 ~11 s.

### (archiwum) TLAS na GPU – blokada rozmiarem jednego shadera
- Test `xbox_bvh_builder_builds_a_tlas_on_the_gpu` (`XCL_CONSOLE_LISTS=1 XCL_PROBE_BVH_TLAS=1`): 2 BLAS + 16 instancji
  (część tylko przesunięta), `trace_scene` vs brute force w świecie. BLAS-y budują się, TLAS wychodzi pusty: 1 dispatch
  pominięty, bo **sterownik AMD odrzuca PSO z przetłumaczonym CS 15 720 instrukcji (0x8007000E, brak pamięci)** – także
  przy limicie 20 GB; drugi duży (6 302 instr.) przechodzi przy 20 GB, nie przy 8 GB. DXC `-O1`/`-O3` na nim > 150 s.
- Ulepszenia tłumacza zrobione przy okazji (wszystkie testy zielone, budowa BLAS na GPU nadal 500/500):
  - **strukturalny przepływ sterowania**: program w całości strukturalny (skoki do przodu zagnieżdżone, if/else przez
    `s_branch`, pętle z jednym skokiem powrotnym na końcu, `s_endpgm` na końcu) → zwykłe `if`/`for(;;)` bez `pc`;
    inaczej **hybryda**: najdłuższe odcinki z jednym wejściem/wyjściem strukturalnie pod jednym strażnikiem `pc`
    (#19: 628 → 93 strażników). `XCL_RECOMP_NO_STRUCTURED=1` wyłącza, `XCL_RECOMP_WHY=1` mówi, czemu nie strukturalny;
  - `global_atomic_fmin/fmax` jako jeden atomik całkowity (dodatnie → int, ujemne → uint odwrotnie) zamiast pętli CAS;
  - zapis VGPR `v = select(aktywna, wartość, v)` zamiast `if` (kod liniowy);
  - `pso::console_to_hlsl` + testy pomocnicze `blob_hlsl_shape` / `blob_dxc_cost` (`XCL_BLOB=…`).
- **Do zrobienia:** chudszy HLSL (mniej zmiennych tymczasowych: przy `-O0` każda to osobna komórka pamięci, dziesiątki
  tysięcy na tym shaderze) albo inny podział kompilacji; to samo przyspieszy kompilację dużych shaderów gry.

### Plan: konsolowa lista poleceń w XCL (następne sesje)
1. `hle/d3d12x/console_list.rs`: obiekt o układzie konsoli (pola jak wyżej, bufor rekordów, nagłówki parametrów), opakowujący
   listę PC; vtable konsolowy: 14 Dispatch, 25 SetPipelineState, 33 SetComputeRootSignature (wpisuje znaczniki, pamięta
   układ RS), 37/39/43/45 zastępcze parametry (dopisują te same rekordy), 80, 146 bariera, 148 AllocateUploadMemory
   (UMA), Close/Reset; reszta – logujące zaślepki. CreateCommandList z kodu konsoli → ten obiekt.
2. PSO: przy tworzeniu zapamiętać PSO → (przetłumaczony?, układ RS z RTS0).
3. ExecuteCommandLists: interpretacja rekordów → lista PC: przy każdym dispatchu tablica argumentów root (układ z sekcji
   „Wiązanie argumentów root”) w UMA, `io` = [adres lo, hi, …], root signature ABI + io/res/mem, PSO przetłumaczony, Dispatch;
   bariery → bariera UAV na `mem`.
4. Test: prawdziwy XboxBvhBuilder buduje BLAS na GPU przez tę listę → wynik czytany `bvh::Bvh::trace` = brute force.

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
  Zarejestrowany jako „Microsoft Opus Audio Decoder MFT” {63e17c10-2d43-4c42-8fe3-8d8b63e46a6a} (sesja 5).
- AWC strumieniowe (sesja 5, z fragmentów paczki): flagi 0xFF25, przy bicie 0 indeksy chunków u16 na strumień, potem 12 B
  o nieznanym znaczeniu; info strumieni u32 (liczba chunków = bity 31:29) leżą tuż przed tabelą chunków, a tabela kończy się
  na `data_offset` z nagłówka. Chunk 0x48 = {liczba bloków u32, rozmiar bloku u32 (512 KB), kanały u32, na kanał 16 B: id,
  próbki, u16, częstotliwość u16, kodek u8 …}; 0xA3 = próbka startu każdego bloku 512 KB. Blok 512 KB = nagłówek 2048 B
  (−1, 254 podbloki, 0, próbki w bloku, …, od +24 próbka startu każdego podbloku) + podbloki 2048 B `[u16 długość][pakiet]…`.
  Niewyjaśnione: 6 pakietów na podblok przy różnicy w tablicy 12·960 – TOC 0xFC to CELT FB 20 ms **stereo**, więc tablica
  liczy chyba próbki × kanały.

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
Bez exe zostało niewiele – reszta potrzebuje prawdziwego wywołania gry.
1. Gdy użytkownik będzie miał legalny exe gry: `xcl inspect`, `xcl run`, analiza `report.json`
   (`called_stubs` z `(spoza PC)` → mapowanie slotów konsolowych np. PresentX; `d3d12.unknown_iids`; brakujące XGameRuntime;
   `shaders.rdna2_mnemonics`; `audio` – formaty głosów, `unsupported_formats`; importy związane z Opus/XMA/AcpHal).
2. Wiązanie przy rysowaniu: argumenty root gry → SGPR w `io`, `mem` → bufor UMA; przechwytywanie list poleceń dla GS
   i dispatchy Compute; `XboxBvhBuilder` (lub `BuildRaytracingAccelerationStructure`) → `gpu::bvh::build/build_top`
   (geometria z buforów GPU w UMA; Rehydrate = gotowe BVH z `*_bvh4.rpf` – format do sprawdzenia).
3. Tekstury: konfiguracja pipe/packer konsoli dla trybów _X (`tiling::PipeConfig`), untiling przy tworzeniu zasobów
   z pamięci konsoli, układ łańcucha mipów GFX10.
4. Audio: Opus (MFT wyżej) dla głosów XAudio2 0x704F lub API konsoli – zależnie od tego, co pokaże exe; XMA2 niski priorytet.
5. Mniejsze: 749 programów wave64 z ryzykiem w fali 32 (dopiero jeśli się objawią), `image_sample_o` z offsetem z VGPR,
   mniejszy HLSL, kolejne API XGameRuntime (XGameUI, XNetworking…), GameInput → systemowe GameInput.dll,
   mapowanie stron fizycznych pod adres gry.
