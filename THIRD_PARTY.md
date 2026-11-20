# Źródła zewnętrzne XCL

XCL jest na licencji **GPL-2.0-only** (`LICENSE`). Poniżej wszystko, co pochodzi z zewnątrz,
wraz z licencją i sposobem użycia. Pliki, które zawierają logikę przeniesioną z innego projektu,
mają w nagłówku informację o pochodzeniu i dacie zmian (wymóg GPL-2.0 §2a).

| Źródło | Licencja | Co w XCL | Jak |
|---|---|---|---|
| [AnyPS5](https://github.com/boykopovar/AnyPS5) | GPL-2.0-only | recompiler (`crates/xcl/src/gpu/recomp/`) – fragmenty logiki oznaczone w plikach | port do Rusta/HLSL z adnotacją |
| [AMD Machine-Readable GPU ISA](https://gpuopen.com/machine-readable-isa/) | MIT | `crates/xcl/src/gpu/rdna2_spec.rs` (wygenerowany przez `scripts/gen_rdna2.py`) | tablice kodowań i instrukcji |
| [LLVM](https://github.com/llvm/llvm-project) | Apache-2.0 WITH LLVM-exception | `tests/data/llvm-amdgpu/*` (dane testowe, nie wchodzą do programu); wartości formatów buforów GFX10 (`UfmtSymbolicGFX10`) w `gpu/recomp/emit.rs` | wektory testowe; fakty sprzętowe |
| [WinDurango](https://github.com/WinDurango/WinDurango) | LGPL-3.0 | brak kodu; numery eksportów d3d12_x.dll (fakty) w `crates/xcl/data/stubdb.toml` | wyłącznie referencja |
| [libHttpClient](https://github.com/microsoft/libHttpClient) | MIT | brak kodu; kontrakt XTaskQueue/XAsync | specyfikacja |
| docs.xbox.com | dokumentacja | sygnatury, enumy, struktury pisane samodzielnie | referencja |
| Crates Rusta (`Cargo.lock`) | MIT / BSD-2 / Zlib / Unlicense (lub z alternatywą MIT) | zależności budowania | linkowane statycznie |
| DXC (`dxcompiler.dll`, `dxil.dll`) z Windows SDK | NCSA (dxcompiler) / Microsoft (dxil) | kompilacja HLSL → DXIL w czasie działania | ładowane z SDK użytkownika, **nie** dystrybuowane z XCL |
| Biblioteki systemowe Windows (d3d12, dxgi, xaudio2_9, ole32 …) | system | API docelowe | wyjątek „system library” GPL-2.0 |

Przy dodawaniu zależności: zgodne z GPL-2.0-only są MIT, BSD, Zlib, ISC, Unlicense, LGPL-2.1(+),
GPL-2.0(+), MPL-2.0 (przez klauzulę licencji wtórnych). Niezgodne: Apache-2.0 bez alternatywy,
GPL-3.0, LGPL-3.0, AGPL.
