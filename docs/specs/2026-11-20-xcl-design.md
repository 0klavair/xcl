# XCL — warstwa zgodności Xbox Series X|S → Windows (projekt)

Data: 2026-11-20 · Status: do akceptacji · Nazwa robocza: **XCL** (do zmiany)

## 1. Cel i dlaczego

Uruchamiać na zwykłym PC z Windows 11 (x64) gry zbudowane dla Xbox Series X|S
(GDK, „Scarlett”) **bez emulacji CPU** — kod gry to natywny x64, więc wykonuje
się bezpośrednio. Warstwa podstawia własne implementacje bibliotek, których na
PC nie ma (`kernelx`, XGameRuntime, D3D12.X, XAudio2 z XMA2, GameInput),
podobnie jak Wine/Proton robią to dla Windows na Linuksie, a WinDurango dla
Xbox One.

**Pierwszy sukces (słowami użytkownika):** „odpalenie gry poprawnie, czyli
obsłużenie plików (rpf itd.) i odpalenie gierki na kompie”. W praktyce:
exe gry ładuje się, wszystkie jego importy są podpięte, gra przechodzi
inicjalizację i czyta swoje pliki danych (archiwa takie jak RPF czyta kod
samej gry — warstwa musi tylko dostarczyć działające operacje na plikach).

Warstwa ma być **ogólna** — dla dowolnego tytułu z Series, nie pod jedną grę.

## 2. Cele, nie-cele, antycele

**Cele**
- Uniwersalny loader gier z Series (x64 PE) z automatycznymi zaślepkami dla
  niezaimplementowanych funkcji i pełnym raportem, czego brakuje.
- `kernelx` + podstawowy XGameRuntime wystarczające, by gra wystartowała,
  czytała pliki i zapisywała dane (zapisy, cache) poza folderem gry.
- Ścieżka rozwoju do grafiki (D3D12.X → D3D12), audio (XAudio2/XMA2) i
  wejścia (GameInput) — kolejne etapy.

**Nie-cele (poza zakresem)**
- Odszyfrowywanie paczek (XVC/MSIXVC) i jakiekolwiek obchodzenie DRM.
  Warstwa zakłada pliki gry już w postaci zwykłych plików PE/danych.
- Usługi online Xbox Live (XSAPI, multiplayer, osiągnięcia) — tylko tryb offline.
- Gry z Xbox One (XDK/ERA, D3D11.X) — to robi WinDurango.
- Tłumaczenie natywnego kodu GPU konsoli (ISA RDNA2), jeśli gra nie zawiera
  DXIL (zob. ryzyka).
- macOS i Linux. Platforma docelowa to wyłącznie Windows x64. Jedyny wymóg
  „na przyszłość”: backend grafiki da się podmienić na vkd3d-proton (§5.7).
  Działanie `xcl.exe` pod Wine **nie** jest wymagane, więc techniki hooków
  nie muszą być z nim zgodne. macOS na Apple Silicon wymagałby tłumaczenia
  CPU (Rosetta), co kłóci się z założeniem „bez emulacji CPU”.

**Antycele (porażka, nawet jeśli „działa”)**
- **Jakakolwiek zmiana lub usunięcie pliku w folderze gry.** Folder gry jest
  wyłącznie do odczytu; jeśli gra chce coś zmienić, kopiujemy pojedynczy plik
  do nakładki (overlay) i zmieniamy kopię. (Wymóg użytkownika.)
- Ciche błędy — każde wywołanie niezaimplementowanej funkcji musi być zalogowane.
- Kod lub nagłówki z wycieków SDK w repozytorium. Deklaracje piszemy sami na
  podstawie publicznej dokumentacji (docs.xbox.com, learn.microsoft.com).
- Udawanie licencji sklepu (XStore zwraca „offline/niedostępne”, nie „kupione”).

## 3. Ograniczenia

- Host: Windows 11 x64. Narzędzia dostępne na maszynie: Visual Studio 2022
  Community (MSVC, CMake i Ninja dołączone do VS), Git, Python 3.12, MSYS2 gcc.
- Na konsoli Series dostępne jest **tylko D3D12.X** (D3D11.X nie istnieje) —
  źródło: publiczna dokumentacja GDK.
- Pełna dokumentacja niektórych tematów (np. definicje kompilatora shaderów
  Xbox) jest za NDA; opieramy się tylko na publicznych stronach.
- Brak legalnych testowych binarek z Series → testujemy na własnym programie
  testowym (§7), który ma tablicę importów jak tytuł konsolowy.
- **Prawdziwe tytuły uruchamia użytkownik** na własnych plikach i na swoim
  komputerze. Wykonawca nie ma do nich dostępu i nie testuje na nich. Pętla
  pracy: użytkownik uruchamia `xcl inspect` / `xcl run` i przekazuje
  `report.json` + log, a wykonawca na tej podstawie dopisuje kolejne funkcje.
  Program testowy sprawdza mechanikę (loader, TLS, VFS…), ale nie wychwyci
  błędnych sygnatur, ordinali ani układów struktur, bo obie strony testu
  bazują na tych samych deklaracjach. To wychwycą dopiero raporty z
  prawdziwych tytułów.
- Warunek wstępny dla prawdziwego tytułu: pliki są już zwykłymi plikami
  PE/danych, a nie zaszyfrowaną paczką XVC/MSIXVC. `xcl inspect` wykrywa i
  zgłasza plik, który nie jest poprawnym PE x64.
- Nazwy importów prawdziwych gier z Series nie są znane z góry (mogą to być
  `kernelx.dll`, API-sety `api-ms-win-*`, eksporty tylko konsolowe, importy
  po samym ordinalu). Dlatego resolver jest sterowany danymi (aliasy + baza
  ordinali/zaślepek), a nie zaszyte listy.

## 4. Podejścia

| | Opis | Plusy | Minusy |
|---|---|---|---|
| A | **Podmiana DLL** (jak WinDurango): Windows ładuje exe gry, obok leżą nasze `kernelx.dll` itd. | Najprościej; TLS, wyjątki, ładowanie robi Windows. | Jedna brakująca funkcja = gra nie startuje („nie znaleziono punktu wejścia”). Każdą funkcję trzeba dopisać i przebudować. DLL muszą leżeć obok exe → kopiowanie exe do folderu tymczasowego. |
| **B** | **Własny loader PE** (`xcl.exe`): sami mapujemy exe gry do pamięci i rozwiązujemy importy przez centralną tablicę; brakujące funkcje → automatyczne zaślepki logujące. | Gra zawsze idzie tak daleko, jak się da; pełny raport braków po 1. uruchomieniu; nic nie trzeba kopiować do folderu gry; jedno miejsce kontroli (ścieżki, pamięć, wątki). | Sami robimy to, co zwykle robi Windows: statyczny TLS, rejestracja wyjątków, widoczność obrazu dla systemu. |
| C | Hybryda: kopia exe z przepisaną tablicą importów, ładowana przez Windows. | Windows robi TLS. | Wady A (sztywne listy eksportów), bez przewagi nad B. |

**Wybór: B** (decyzja projektanta; użytkownik oddał „jak” — „rób ogólnie”).
Uzasadnienie: cel to ogólna warstwa i „gra startuje”; B jako jedyne daje
start mimo braków i raport, co dalej, dla każdego tytułu. Ryzyka B (TLS,
wyjątki C++) są sprawdzane jako pierwsze zadania etapu M1 na programie testowym.

**DXVK/DXMT** (sugestia użytkownika): rozważone, **nieużywane dla gier z
Series** — oba tłumaczą tylko D3D8–11 (DXVK → Vulkan, DXMT → Metal/macOS),
a gry z Series używają wyłącznie D3D12.X. Odpowiednikiem dla D3D12 jest
natywne D3D12 na Windows lub vkd3d-proton (D3D12 → Vulkan) — zob. §5.7.

## 5. Architektura

Jeden proces: `xcl.exe` (launcher + loader + wszystkie moduły HLE jako
biblioteki statyczne). Gra jest mapowana do tego procesu i wykonywana natywnie.

```
xcl.exe
 ├─ launcher   CLI: `xcl run <folder|exe>`, `xcl inspect <exe>`
 ├─ loader     mapowanie PE, relokacje, TLS, wyjątki, tabela modułów
 ├─ resolver   polityka importów, aliasy, generator zaślepek
 ├─ vfs        montowania, tłumaczenie ścieżek, overlay copy-on-write
 ├─ hle/kernelx        (M1)
 ├─ hle/xgameruntime   (M1, podzbiór offline)
 ├─ hle/d3d12x         (M2)
 ├─ hle/xaudio2        (M3)
 ├─ hle/gameinput      (M4)
 └─ common     log, konfiguracja, raport
```

### 5.1 Loader PE (M1)

1. Wczytaj exe gry (tylko odczyt), sprawdź: PE32+, `IMAGE_FILE_MACHINE_AMD64`.
2. Zarezerwuj pamięć pod preferowanym `ImageBase`; jeśli zajęte i są relokacje
   → relokuj; jeśli zajęte i brak relokacji → czytelny błąd. (`xcl.exe` jest
   budowany z ASLR, więc typowy adres `0x140000000` jest zwykle wolny.)
3. Zmapuj sekcje, zastosuj relokacje, rozwiąż importy **zwykłe i opóźnione
   (delay-load)** przez resolver (§5.2), ustaw ochronę stron sekcji.
4. **Wyjątki:** zarejestruj `.pdata` (`RtlAddFunctionTable`). Dodatkowo obraz
   musi być „widoczny” dla systemu: wyjątki C++ na x64 używają
   `RtlPcToFileHeader`, który szuka modułu na liście loadera Windows —
   zmapowany ręcznie obraz tam nie występuje. Rozwiązanie: hook
   `RtlPcToFileHeader` i `GetModuleHandleExW(FROM_ADDRESS)` (Microsoft
   Detours, MIT) albo wpis do list modułów w PEB — wybór dla wykonawcy;
   test: wyjątek C++ rzucony i złapany w kodzie gry musi działać.
5. **Statyczny TLS** (`__declspec(thread)`, używany powszechnie): przydziel
   indeks, wpisz go do `AddressOfIndex` gry, dla każdego wątku zbuduj blok
   z szablonu TLS gry i wstaw go do tablicy `ThreadLocalStoragePointer` w TEB
   (rozszerzając ją). Nowe wątki: przez własny callback TLS `xcl.exe`
   (dostaje `DLL_THREAD_ATTACH` dla każdego nowego wątku, także z puli
   wątków systemu). Wątki istniejące przed załadowaniem: uzupełnić przy
   ładowaniu. Wywołać callbacki TLS gry (process/thread attach/detach).
   Dokładna technika — dla wykonawcy; wymagane testy wielowątkowe.
6. Ustaw `PEB->ImageBaseAddress` na obraz gry przed startem (zasoby,
   `GetModuleHandle(NULL)` w kodzie systemowym) — po weryfikacji, że nie psuje
   launchera; inaczej obsłużyć w shimach.
7. Wywołaj punkt wejścia gry na wątku z dużym stosem (rozmiar z nagłówka PE
   gry, `SizeOfStackReserve`).

DLL dostarczone **przez grę** (middleware itp.) ładowane są tym samym
loaderem, rekurencyjnie, z importami przez resolver.

### 5.2 Resolver importów (M1)

Dla każdego importu `dll!funkcja` (lub `dll!#ordinal`), w kolejności:

1. **Aliasy z konfiguracji** (np. `api-ms-win-core-*` → `kernelx` albo → system).
2. **Moduł HLE** o tej nazwie (`kernelx`, `xgameruntime`, …): jeśli funkcja
   zaimplementowana → nasza implementacja. Moduł może mieć **fallback do
   systemowej DLL** (np. `kernelx` → `kernelbase`/`kernel32` dla funkcji o
   tych samych nazwach, których nie nadpisujemy).
3. **DLL z folderu gry** → nasz loader (5.1).
4. **Systemowa DLL Windows z białej listy** (np. `ucrtbase`, `vcruntime140`,
   `msvcp140`, `user32`, `ws2_32`) → `LoadLibrary` + `GetProcAddress`.
5. **Brak** → **automatyczna zaślepka**: mały thunk x64 (np. `mov r10, info;
   jmp handler` — bez ruszania rejestrów argumentów), który przy pierwszym
   wywołaniu loguje `[STUB] dll!funkcja` z adresem wywołującego (RVA w
   obrazie gry) i zwraca wartość z bazy zaślepek (domyślnie `0`; dla funkcji
   znanych jako HRESULT — `E_NOTIMPL`; nadpisywalne w konfiguracji).

**Importy po ordinalu:** ordinali nie da się ustalić z publicznej dokumentacji.
Resolver ma bazę `dll → {ordinal → nazwa, wartość zwracana}` w pliku danych
(np. TOML/JSON w repo), uzupełnianą na podstawie raportów. Nieznany ordinal
dostaje zaślepkę `dll!#N`, a `inspect` i `report.json` wypisują go osobno.

Runtime: `LoadLibrary*`/`GetModuleHandle*`/`GetProcAddress` wywołane przez grę
trafiają do shimów, które używają tej samej tabeli modułów (moduły HLE mają
pseudo-uchwyty).

### 5.3 kernelx (M1)

Grupy funkcji (sygnatury z publicznej dokumentacji + raporty importów;
WinDurango `kernelx.cpp`, LGPL-3.0, jako referencja logiki):

- **Pamięć:** `VirtualAlloc*/Free/Protect/Query` z usuwaniem flag
  konsolowych (np. pamięć graficzna/tytułu), rodzina `XMemAlloc/XMemFree/
  XMemSetAllocationHooks`, API stron fizycznych (emulacja przez
  `VirtualAlloc`/file mapping), `TitleMemoryStatus` (budżet z konfiguracji).
- **Wątki:** nazwy wątków, maski koligacji (rdzenie konsoli → rdzenie PC),
  rodzina `XThread*`.
- **Pliki:** przez VFS (§5.4).
- **Moduły:** shimy z §5.2.
- **System:** typ konsoli (`XSystemGetDeviceType` itp. — konfigurowalnie
  Series X / Series S), wersja OS, info o procesorze.
- Reszta (synchronizacja, czas, …) → fallback do systemu.

### 5.4 System plików (VFS) (M1)

- **Montowania:** katalog główny gry = oryginalny folder gry, **tylko do
  odczytu**. Litery dysków / ścieżki konsolowe → mapowanie z konfiguracji.
  Każda nieznana ścieżka konsolowa jest logowana `[VFS] unmapped`, żeby
  dopisać mapowanie.
- **Dane zapisywalne** w `%LOCALAPPDATA%\XCL\<tytuł>\`: `overlay\`, `saves\`,
  `pls\` (persistent local storage), `temp\`, `logs\`.
- **Copy-on-write:** otwarcie pliku gry do zapisu → skopiuj **ten jeden plik**
  do `overlay\` i otwórz kopię. Odczyt: najpierw overlay, potem oryginał.
  Usunięcie → znacznik w overlay (oryginał nietknięty). Wyliczanie katalogów
  scala obie warstwy.
- **Zasięg:** VFS musi łapać także operacje z DLL ładowanych przez Windows
  (np. CRT: `fopen` → `ucrtbase` → `CreateFileW`), więc działa jako globalne
  hooki w procesie (Detours) na API plików (Win32 lub `Nt*` — wybór
  wykonawcy), a nie tylko na importach gry.
- Launcher nigdy nie tworzy plików w folderze gry.

### 5.5 XGameRuntime — podzbiór offline (M1)

- `XGameRuntimeInitialize/Uninitialize`.
- `XTaskQueue` + `XAsync` — port z **libHttpClient** (Microsoft, MIT), który
  zawiera ich implementację.
- `XSystem*` (typ urządzenia, ID konsoli — z konfiguracji).
- `XPackage*` (ścieżka montowania = katalog gry; DLC = podfoldery z konfiguracji).
- `XUser*` — jeden lokalny użytkownik offline (gamertag z konfiguracji).
- `XGameSave*` → `saves\`, `XPersistentLocalStorage*` → `pls\`.
- `XGameUI*` → zaślepki zwracające „anulowano/niedostępne”.
- `XStore*` → błąd „offline/niedostępne” (bez udawania licencji).

### 5.5a „Pusta” grafika D3D12.X — null device (M1)

Bez tego gra zakończyłaby się przy pierwszym wywołaniu grafiki, zanim zdąży
wczytać większość danych. Dlatego M1 zawiera minimalny `d3d12_xs`:

- Eksporty tworzące urządzenie/kolejki zwracają **atrapy obiektów**, które
  przyjmują każde wywołanie, logują je i zwracają sukces z poprawnymi
  atrapami parametrów wyjściowych (np. kolejne obiekty-atrapy, deskryptory,
  „spełnione” fence).
- Okno Win32 gry (tworzy je sama gra przez `user32`) i „prezentacja”, która
  co klatkę tylko czyści okno jednym kolorem. **W M1 nie ma obrazu gry**,
  obraz to M2.
- Układ vtable: na start kolejność metod z publicznych nagłówków PC D3D12
  (DirectX-Headers) + metody Xbox dopisane na końcu. Każda tablica wirtualna
  ma dodatkowo zapas slotów prowadzących do zaślepek, które logują **numer
  slotu i interfejs**. Dzięki temu raport z prawdziwego tytułu pokazuje,
  gdzie zgadywany układ się rozjeżdża (dane wejściowe do M2).

### 5.6 Logowanie i raport (M1)

- `logs\xcl.log` — poziomy, wątek, czas.
- `logs\report.json` po zakończeniu: brakujące importy (statycznie), wywołane
  zaślepki (dynamicznie, z licznikiem i miejscem wywołania), niezmapowane
  ścieżki, (M2) formaty shaderów i brakujące metody D3D12.X.
- `xcl inspect <exe>` — raport bez uruchamiania: nagłówek PE, TLS, relokacje,
  importy (też delay-load) ze statusem: zaimplementowane / fallback do
  systemu / zaślepka. Tylko odczyt.

### 5.7 Grafika D3D12.X → D3D12 (M2 — kierunek, osobna runda projektowa)

- Backend: **natywne D3D12** na Windows (Agility SDK, publiczne). Architektura
  pozwala podmienić `d3d12.dll` na **vkd3d-proton** (LGPL-2.1, D3D12 → Vulkan)
  w przyszłości (Linux/Vulkan).
- Eksporty `d3d12_xs.dll` i interfejsy D3D12.X — sygnatury z publicznej
  referencji docs.xbox.com; obiekty-opakowania przekazujące wywołania do
  D3D12 na PC; metody tylko-Xbox emulowane lub no-op z logiem.
- Prezentacja: konsolowe `PresentX`/zdarzenia klatek → swap chain DXGI
  (flip model) na oknie Win32 gry.
- Shadery: parsowanie kontenera; jeśli jest część **DXIL** → usunąć części
  Xbox, ponownie podpisać (walidator z DirectXShaderCompiler) i utworzyć PSO;
  jeśli tylko kod sprzętowy → log + raport (poza zakresem).
- Najtrudniejsze tematy do osobnego projektu: zunifikowana pamięć i adresy
  GPU nadawane przez aplikację, układ vtable interfejsów D3D12.X (nagłówki są
  za NDA).

### 5.7a Shadery konsolowe — recompiler RDNA2 (M2, decyzja z 2026-11-20)

Użytkownik wskazał (i zgadza się to z ryzykiem nr 1), że D3D12.X dostarcza
shadery skompilowane z góry do mikrokodu GPU konsoli (RDNA2), a nie tylko DXIL.
Żaden sterownik PC nie przyjmuje surowego ISA, a karta użytkownika (RDNA4) ma
inny zestaw instrukcji — potrzebny jest **recompiler mikrokod RDNA2 → shader PC**.

Etapy:
1. **Przechwytywanie** (zrobione): każdy blob z tworzenia PSO / root signature
   jest klasyfikowany (DXIL / DXBC / kontener z nieznanymi częściami / ELF /
   surowy RDNA2), zapisywany do `<dane>\shaders\` i liczony w `report.json`.
2. **Dekoder/dezasembler RDNA2** (zrobione): tablice generowane z oficjalnej
   specyfikacji AMD Machine-Readable ISA (MIT) skryptem `scripts/gen_rdna2.py`;
   walidacja na 23 812 instrukcjach z testów LLVM — 100% zgodności nazw i długości
   dla wszystkich instrukcji istniejących w RDNA2; `xcl disasm`.
3. Graf przepływu sterowania + strukturyzacja (EXEC mask → rozgałęzienia per-lane).
4. IR i semantyka instrukcji (najpierw te, które występują w zrzutach z gier).
5. Model zasobów: sprzętowe deskryptory V#/T#/S# → D3D12 bindless
   (SM 6.6 `ResourceDescriptorHeap`), konwersja formatów w czasie działania.
6. Backend: HLSL → `dxcompiler`/`d3dcompiler_47`, cache wyników.
7. Wejścia shadera (user SGPR, początkowe VGPR) — wymagają danych z prawdziwych
   kontenerów konsolowych (nieznane części) — stąd etap 1 jako pierwszy.

**Format kontenera konsolowego (zrobione 2026-11-20, `gpu/xbox.rs`)** — ustalony na
paczce shaderów prawdziwej gry (23 869 shaderów: PS 12 101, MS 9 282, VS 2 236, GS 195, CS 55):

- DXBC: SFI0, ISG1, OSG1, PSV0, HASH, DXIL, PRIV. Część DXIL ma zwykły
  `DxilProgramHeader` (SM 6.6), bitcode = 4-bajtowe `NOBC`; między nagłówkiem a nim
  `XDAT` (+0x18): liczba wpisów, offset tabeli, wpisy (id, offset, rozmiar) liczone od
  nagłówka programu. 0x12 = hash 16 B, 0x0D = XBCH, 0x07 = zagnieżdżony nagłówek (NOBC).
- `XBCH` v3: rozmiar skompresowany (+8), rozpakowany (+0x10), strumień zlib od +0x18.
- `FSSM` v0x101: rozmiar (+8); u64 offset/rozmiar kodu pod +0xC0/+0xC8 (offset od +0x10);
  deskryptory etapów `0x000000C0_0002_AAHH` od +0x100 (co 0x88): HH = etap sprzętowy PAL
  (2 ES, 3 GS, 4 VS, 5 PS, 6 CS, 7 = MS/NGG), AA = etap API (0 VS, 1 PS, 2 GS, 3 CS, 6 MS).
  VS używany z GS ma dwa XBCH (ES + VS); GS ma deskryptory GS + VS (copy shader), ale tylko
  jeden program — copy shader powstaje poza FSSM. Nieużywane pola = 0xDEADBEEF; w PS
  nagłówek zawiera tablicę wariantów eksportu (łatki `exp` dla formatów RT).
- Kod: jeden program na FSSM, może mieć kilka `s_endpgm` (wspólne wyjście „wszystkie
  piksele odrzucone” w PS, trampoliny wchodzone przez `s_setpc`); stałe osadzone w kodzie
  jako `s_endpgm | u32 N | N dwordów | s_endpgm` (adresowane przez `s_getpc_b64`).
  Kod zaczyna się od dwóch `v_nop` (VOP3 + literał), w których polach i literałach zaszyty
  jest MD5 z części HASH (bajty 0–2, 4–10, 12–15 widać wprost); VS zaczyna się dalej od
  `s_swappc_b64 s[4:5]` — wywołanie fetch shadera (atrybuty wierzchołków → VGPR wg ISG1).
- Za kodem: tablica wpisów (3, offset w kodzie, 1, wartość, 8) – wygląda na mapowanie
  PC → linia źródła; potem nazwa shadera.
- ISA: RDNA2 + zachowane z gfx10.1 `v_mac_f32` (VOP2 0x1F/VOP3 0x11F), `v_madmk_f32`
  (0x20), `v_madak_f32` (0x21), `v_mad_f32` (VOP3 0x141) — `gpu/rdna2_xbox.rs`. Sprzęt
  pobiera literał, gdy dowolne pole SRC VALU = 255 (również nieużywane przez opcode).
  Po tym dekoder rozpoznaje 100% z 28,97 mln instrukcji korpusu (395 mnemoników).
- Priorytety recompilera: `xcl shaders <paczka|folder>` — histogram, w ilu shaderach
  występuje mnemonik i pokrycie (ile shaderów ma wyłącznie K pierwszych mnemoników):
  K=100 → 6,8%, 150 → 29%, 200 → 54%, 300 → 97,5%, 395 → 100%. Najczęstsze:
  `v_mul_legacy_f32` (13,3% instrukcji, semantyka DX9: 0·x = 0), `v_fmac_f32`, `s_waitcnt`,
  `v_fma_f32`, `v_mov_b32`.

Referencje: shadPS4 (GCN → SPIR-V, GPL-2.0 — tylko jako wzorzec, bez kopiowania
kodu), AnyPS5 (RDNA → SPIR-V dla PS5, **GPL-2.0-only** — od 2026-11-20 XCL też jest
GPL-2.0-only, więc logikę można przenosić z adnotacją pochodzenia; inny backend: Vulkan/SPIR-V
i model zasobów PS5/SRT),
Mesa ACO (MIT, semantyka RDNA2), dokumentacja ISA RDNA2 (AMD), LLVM AMDGPU
(Apache-2.0 z wyjątkiem LLVM — tabela formatów buforów GFX10).

### 5.7b Recompiler — stan (2026-11-20, `gpu/recomp/`)

**Model wykonania (dokładny bez strukturyzacji grafu):** każdy skok w kodzie AMD jest
skalarny, czyli wspólny dla całej fali, więc program to pętla-dyspozytor po blokach
podstawowych (`switch (pc)`); programy bez skoków są emitowane liniowo. SGPR/VCC/EXEC/M0/SCC
to zwykłe zmienne liczone identycznie przez wszystkie wątki (jednolite), maski z instrukcji
wektorowych przez `WaveActiveBallot`, VGPR per wątek z zapisem maskowanym bitem EXEC,
odczyty między wątkami (`v_readlane`, DPP16/DPP8, `ds_swizzle`, `v_permlane(x)16`) przez
`WaveReadLaneAt` (zmienny indeks działa na RX 9070 + DXC 1.8). Wave64/wave32 wykrywane
z kodu (`detect_wave64`).

**Tryb kernel (weryfikacja):** compute shader `[WaveSize(64)]` — rejestry startowe z bufora,
końcowe rejestry i eksporty do bufora, pamięć płaska (`mem`: adresy z V#/wskaźników).
`runner` (D3D12 na GPU wysokiej wydajności) + `dxc` (dxcompiler.dll z Windows SDK lub
`XCL_DXC_DIR`, podpis przez dxil.dll). Testy instrukcji na prawdziwym GPU (asembler
`rdna2::encode`): ALU f32/u32/i32 z modyfikatorami VOP3, SCC, pętle, rozbieżność przez EXEC
(łącznie z pomijanym blokiem), SDWA (sel/sext/dst_sel/preserve), DPP16/DPP8, cross-lane,
`v_fma_mix*`, cube, `s_load`/`s_buffer_load`/`buffer_load`/`buffer_store`, typowane odczyty
(`buffer_load_format_*`, `tbuffer_*`: 78 formatów GFX10, DST_SEL, niewyrównane adresy,
dokładne końce zakresu UNORM/SNORM — DXC dzieli w przybliżeniu), eksporty (z COMPR).
**Prawdziwy shader konsoli `VS_BlitGenAmbCube` wykonuje się na RX 9070 z poprawnym
SV_Position** (`s_swappc_b64 s[4:5], s[4:5]` = wywołanie fetch shadera → atrybuty w VGPR).

**Korpus (`xcl shaders … --recomp --compile N`):** 24 064 programy, 0 błędów tłumaczenia,
**7 442 (30,9%) tłumaczy się w całości**, DXC kompiluje 500/500 próbek. Pozostałe bariery to
wyłącznie interfejs grafiki: `v_interp_*` (wejścia PS) i `image_*` (tekstury).

**Dalej:** (1) tryby VS/PS: wejścia VS z ISG1 zamiast fetch shadera, `v_interp_p1/p2/mov`
→ wejścia PS z ISG1 (indeks atrybutu = rejestr), `exp pos/param/mrt/mrtz` → wyjścia HLSL
(OSG1), (2) model zasobów: dekodowanie T#/S# i odwzorowanie pamięci GPU konsoli na zasoby
D3D12 (bindless SM 6.6 + bufory „płaskie”), (3) LDS (`ds_*` → groupshared w CS),
(4) zwięzłość HLSL (5,6 GB dla całego korpusu — eliminacja martwych rejestrów, scalanie
wyrażeń) i strukturyzacja dla wydajności, (5) cache DXIL.

### 5.8 Audio (M3 — kierunek)

- Domyślnie: konsolowe API XAudio2 → **systemowe XAudio2.9** Windows; źródła
  **XMA2** (`WAVE_FORMAT_XMA2`) dekodowane do PCM przez **FFmpeg libavcodec**
  (ma dekoder XMA2, LGPL-2.1+); port `XmaDecoder` z WinDurango odpada (LGPL-3.0 niezgodne z GPL-2.0-only XCL).
- AcpHal / APU / SHAPE (sprzęt audio konsoli, publiczna referencja istnieje)
  → najpierw zaślepki + raport; emulacja kontekstów SHAPE-XMA, jeśli tytuły
  ich używają bezpośrednio.

**Zrobione 2026-11-20 (`hle/xaudio2.rs`)**: `XAudio2Create` / `XAudio2CreateWithVersionInfo`
tworzą prawdziwy silnik Windows (flagi spoza 0x2000|0x8000|0x10000 i maska rdzeni konsoli
pomijane, procesor = `XAUDIO2_DEFAULT_PROCESSOR`), vtable łatane w miejscu: IXAudio2 #5
CreateSourceVoice (XMA/XMA2 → PCM16, te same kanały i częstotliwość; inny format, którego
Windows nie przyjmie, np. Opus 0x704F, też → PCM16), #7 CreateMasteringVoice (identyfikator
urządzenia konsoli → domyślne; `CO_E_NOTINITIALIZED` → `CoInitializeEx(MTA)` i ponowienie —
konsola nie wymaga COM; potem ponowienie z 0/0 i kategorią GameEffects), IXAudio2SourceVoice
#21 SubmitSourceBuffer (dla takich głosów bufor = cisza o długości PlayBegin+PlayLength albo
oszacowanej z SamplesEncoded/BlockCount·BytesPerBlock lub nAvgBytesPerSec; kontekst, flagi,
pętle zachowane → callbacki w czasie tytułu), #18 DestroyVoice. Reszta eksportów → systemowe
`xaudio2_9.dll` (lista `fallback` modułu HLE). Statystyki: `report.json → audio`.

**Dane z prawdziwej gry (`audio.rpf`, 28,5 GB, kontenery AWC „ADAT”)**: brak XMA2 — kodek AWC
0x0B = **Opus** 48 kHz (bloki 2048 B: `u16 LE długość + pakiet Opus`, ramki 20 ms, TOC
CELT/Hybrid fullband; strumienie muzyki: bloki 512 KB). Wniosek: dla tego tytułu dekoder XMA2
nie jest potrzebny; gra albo dekoduje Opus sama (PCM → XAudio2, działa), albo używa
konsolowego sprzętowego dekodowania Opus — do sprawdzenia importów exe (`xcl inspect`).
Windows ma dekoder Opus w Media Foundation (`MSOpusDecoder.dll`) — kandydat na backend.

### 5.9 Wejście i reszta (M4 — kierunek)

- GameInput → **GameInput na PC** (Microsoft, redistributable); fallback XInput.
- DirectStorage konsolowe → DirectStorage na PC lub własna implementacja
  (plik + zlib). BCPack — brak publicznego dekodera (ryzyko).

### 5.10 Konfiguracja

Plik TOML na tytuł w `%LOCALAPPDATA%\XCL\<tytuł>\xcl.toml`: typ konsoli,
montowania, aliasy DLL, nadpisania wartości zaślepek, poziom logów, gamertag.
Rozsądne wartości domyślne — gra ma się dać uruchomić bez pisania configu.

## 6. Etapy i kryteria sukcesu

| Etap | Zakres | Gotowe, gdy |
|---|---|---|
| **M0** | Szkielet repo, CMake+Ninja+MSVC, testy jednostkowe, CI lokalne (`ctest`). | `cmake --build` + `ctest` przechodzą. |
| **M1** | Loader, resolver, zaślepki, kernelx, VFS, XGameRuntime offline, null D3D12.X (§5.5a), log/raport, `inspect`. **= pierwszy sukces użytkownika (bez obrazu).** | Program testowy (§7) przechodzi wszystkie sprawdzenia przez `xcl run`; folder gry bajt w bajt niezmieniony; `xcl inspect` działa dla dowolnego PE x64. **Odbiór przez użytkownika** (nie jest warunkiem ukończenia, tylko kolejną iteracją): uruchomienie na własnym tytule, raport → kolejne poprawki. |
| M2 | D3D12.X → D3D12 (osobny projekt). | Program testowy renderuje przez nasze D3D12.X. |
| M3 | XAudio2 + XMA2. | Program testowy odtwarza klip XMA2. |
| M4 | GameInput, DirectStorage. | Pad steruje programem testowym. |

## 7. Testowanie

- **Testy jednostkowe** (doctest, MIT): parser PE, relokacje, polityka
  resolvera, generator zaślepek, tłumaczenie ścieżek, copy-on-write, TLS.
- **Program testowy `xcl_testtitle.exe`** — nasz własny „tytuł konsolowy”:
  linkowany z bibliotekami importów wygenerowanymi z plików `.def` o nazwach
  konsolowych DLL (`kernelx.dll` itd.), więc jego tablica importów wygląda
  jak u gry z Series; na samym Windows się nie uruchomi — tylko przez `xcl`.
  Sprawdza: statyczny TLS w wielu wątkach, wyjątki C++ i SEH, wątki z
  nazwami, `VirtualAlloc` z flagami konsolowymi, `XMemAlloc`, odczyt plików
  z zamontowanego folderu, próbę zapisu (→ overlay), `XTaskQueue`/`XAsync`,
  `XUser`, `XGameSave`, `LoadLibrary`/`GetProcAddress` DLL „gry” (testowa DLL
  ładowana naszym loaderem), wywołanie niezaimplementowanej funkcji
  (→ zaślepka, program działa dalej). Wynik: kod wyjścia + log.
- **Test nienaruszalności:** przed i po każdym uruchomieniu hash wszystkich
  plików folderu testowej gry + lista plików — muszą być identyczne.

## 8. Gotowe źródła i komponenty

| Co | Źródło | Licencja | Użycie |
|---|---|---|---|
| Sygnatury API konsoli (D3D12_XS, XGameRuntime, XMA2, SHAPE, DStorage, GameInput) | docs.xbox.com (publiczne, indeks `llms.txt`) | dokumentacja | deklaracje pisane samodzielnie |
| Implementacje kernelx, D3D11.X, XMA | github.com/WinDurango/WinDurango | LGPL-3.0 | wyłącznie referencja (od zmiany licencji XCL na GPL-2.0-only kodu nie wolno przenosić) |
| Recompiler RDNA → SPIR-V (PS5): dekodowanie deskryptorów, obrazy, interpolacja, strukturyzacja | github.com/boykopovar/AnyPS5 | GPL-2.0-only | port logiki z adnotacją pochodzenia |
| XTaskQueue/XAsync | github.com/microsoft/libHttpClient | MIT | port |
| Hooki API | github.com/microsoft/Detours | MIT | VFS, widoczność obrazu |
| Nagłówki D3D12 | github.com/microsoft/DirectX-Headers, Agility SDK | MIT / MS | M2 |
| Walidator/podpis DXIL | github.com/microsoft/DirectXShaderCompiler | NCSA/MS | M2 |
| D3D12 → Vulkan (opcja) | github.com/HansKristian-Work/vkd3d-proton | LGPL-2.1 | M2+ |
| Dekoder XMA2 | FFmpeg libavcodec | LGPL-2.1+ | M3 |
| Referencja loadera PE | github.com/fancycode/MemoryModule | MPL-2.0 | referencja |
| Testy / config / JSON | doctest, toml++, nlohmann/json | MIT | M0+ |

**Licencja projektu:** GPL-2.0-only (od 2026-11-20, decyzja użytkownika; wcześniej
LGPL-3.0-or-later). Powód: korzystanie z kodu AnyPS5 (GPL-2.0-only). Skutki: kod z WinDurango
(LGPL-3.0) tylko jako referencja; zależności muszą być zgodne z GPL-2 (MIT/BSD/Zlib/LGPL-2.1+;
nie: Apache-2.0 bez alternatywy, GPL-3/LGPL-3); DXC (`dxcompiler.dll`/`dxil.dll`) jest ładowany
z Windows SDK użytkownika, nie dystrybuowany z XCL. Lista: `THIRD_PARTY.md`.

## 9. Ryzyka

1. **Shadery bez DXIL** — czy gry z Series zawierają DXIL obok kodu
   sprzętowego, opisuje dokumentacja za NDA. Jeśli nie → grafika dla takiego
   tytułu zablokowana. Sprawdzane w M2 przez raport formatów shaderów.
2. **Układ vtable D3D12.X** nie jest publiczny → musi być ustalony na
   podstawie analizy binarek; błąd = crash. Dotyczy M2.
3. **Model pamięci GPU** (pamięć zunifikowana, adresy GPU = adresy CPU) —
   możliwe problemy z wydajnością na kartach z osobną pamięcią VRAM. M2.
4. **Statyczny TLS i wyjątki C++** w ręcznym loaderze — testowane jako
   pierwsze w M1.
5. **BCPack / sprzętowa dekompresja** w DirectStorage — brak publicznego
   dekodera.
6. Zabezpieczenia antytamper w grach — poza zakresem.
7. Wymagania sprzętowe: konsola ma 16 GB zunifikowanej pamięci; PC potrzebuje
   porównywalnej ilości RAM + VRAM.

## 10. Kto co zdecydował

**Użytkownik:** cel = gra startuje i czyta swoje pliki; warstwa ogólna, nie
pod jeden tytuł; **nic w folderze gry nie może być usunięte ani zmienione —
tylko kopiowanie pojedynczych plików**; preferencja DXVK/DXMT (rozważona —
§4, nie pasuje do D3D12.X).

**Zmiana po akceptacji (użytkownik):** język **Rust** zamiast C++, bez
osobnego planu wdrożenia. Konsekwencje: cargo workspace zamiast CMake;
hooki przez crate `retour` (BSD-2) zamiast Detours; XTaskQueue/XAsync
napisane w Rust według nagłówków libHttpClient (MIT) jako specyfikacji;
testy przez `cargo test`; program testowy też w Rust (importy „konsolowe”
przez `#[link(kind = "raw-dylib")]`). Statyczny TLS: tzw. „nośnik TLS” —
generowana w locie minimalna DLL z katalogiem TLS o rozmiarze szablonu
gry, którą ładuje Windows (zarządza slotem dla wszystkich wątków), a XCL
kopiuje do niej szablon gry przy starcie każdego wątku. Launcher uruchamia
grę w procesie potomnym `xcl.exe` z linią poleceń gry (argv[0] = exe gry).

**Projektant (do skorygowania):** podejście B; jeden proces `xcl.exe` z
modułami statycznymi; tylko x64; licencja LGPL-3.0 (od 2026-11-20: GPL-2.0-only, §8); XGameRuntime offline; XStore bez
udawania licencji; natywne D3D12 jako backend; XAudio2 → systemowe XAudio2.9
+ FFmpeg dla XMA2; dane w `%LOCALAPPDATA%\XCL\`; nazwa XCL; projekt
w `<repo>`; **null D3D12.X w M1, obraz dopiero w M2**;
tylko Windows (bez wymogu zgodności z Wine); prawdziwe tytuły testuje
użytkownik, a wykonawca pracuje na raportach; resolver sterowany danymi
(baza ordinali).

## 11. Pozostawione wykonawcy

Dokładna technika TLS i widoczności obrazu (hook vs wpis w PEB), poziom
hooków VFS (Win32 vs `Nt*`), implementacja thunków zaślepek, biblioteka logów,
układ katalogów repo, format `report.json`, domyślne wartości zwracane przez
zaślepki poza zasadą „HRESULT → E_NOTIMPL”.
