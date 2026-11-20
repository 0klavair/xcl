//! RDNA2 → HLSL recompiler (design §5.7a / §5.7b).
//!
//! Execution model – exact without control-flow structurisation:
//! * every branch in AMD GPU code is scalar, i.e. uniform across the wave, so the program
//!   becomes a dispatcher loop over basic blocks (`switch (pc)`) with one `pc` per wave;
//!   branch-free programs are emitted as straight-line code,
//! * SGPRs, VCC, EXEC, M0 and SCC are ordinary variables that every lane computes alike
//!   (uniform); lane masks produced by vector instructions come from `WaveActiveBallot`,
//! * VGPRs are per-lane variables; vector writes are masked by the lane's EXEC bit,
//! * cross-lane reads use `WaveReadLaneAt`.
//!
//! Modes: `Kernel` – a compute shader that loads the initial register file from a buffer,
//! runs the program and stores registers and exports back (instruction tests on the real
//! GPU, see `runner`). `Vertex` / `Pixel` – graphics entry points whose inputs and outputs
//! follow the DXBC signatures (rules measured on retail data, design §5.7b):
//! * VS: the fetch shader puts ISG1 element r into v[4 + 4r + c] (ES variant: v[9 + 4r + c]),
//!   v0 = SV_VertexID, v3 = SV_InstanceID; `exp pos0` = SV_Position, `exp param N` = N-th
//!   non-system OSG1 element (register order), then SV_ClipDistance / SV_CullDistance,
//! * PS: the initial VGPRs follow SPI_PS_INPUT_ENA (FSSM +0x248); `v_interp_* attrN` reads the
//!   N-th non-system ISG1 element, interpolated by the PC rasteriser in the mode given by the
//!   I/J pair it uses; `exp mrtN` / `mrtz` = SV_Target / SV_Depth; lanes without a DONE export
//!   are discarded.
//! * MS (NGG, 9 282 programs in the retail pack): a compute shader (`[WaveSize(64)]`, the
//!   thread group of PSV0) runs the program; `s_sendmsg 9` (GS_ALLOC_REQ, M0 = primitives << 12
//!   | vertices), `exp pos/param` (vertex = thread index in the group) and `exp prim` (packed
//!   indices, bit 31 = null) go to a record per group in `res`; a generated pass-through mesh
//!   shader ([`mesh_passthrough`]) rasterises the records. DXC allows `WaveSize` only for
//!   compute, and the console code relies on 64-lane ballots, hence two passes. Inputs:
//!   s3[27:24] = wave index in the group, v5 = group · threads + thread (the group id is v5 >> 7
//!   in the retail code, as in RADV's gfx10.3 mesh emulation), other SGPRs from `io`.
//! Memory instructions use a flat model: descriptor/base addresses index one
//! `ByteAddressBuffer` (`mem`); initial SGPRs (user data) come from `io`. Textures come next.

pub mod dxc;
pub(crate) mod emit;
pub mod pso;
pub use emit::mesh_passthrough;
pub mod runner;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use super::dxbc::SigElement;
use super::rdna2::{self, Decoded};

/// Kernel ABI: register file slots in the `io` / `res` buffers (dwords).
pub const KERNEL_SGPRS: usize = 128;
pub const KERNEL_VGPRS: usize = 256;
pub const KERNEL_LANES: usize = 64;
/// SCC is passed in the slot of the `null` register (125).
pub const KERNEL_SCC_SLOT: usize = 125;
/// first dword of the export area in `res`: 64 targets × 64 lanes × 4 components
pub const KERNEL_EXPORTS: usize = KERNEL_SGPRS + KERNEL_LANES * KERNEL_VGPRS;
pub const KERNEL_RES_DWORDS: usize = KERNEL_EXPORTS + 64 * KERNEL_LANES * 4;

/// Vertex shader interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VsInterface {
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    /// first attribute VGPR: 4 (hardware VS) or 9 (ES variant)
    pub attr_base: u64,
}

/// Pixel shader interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PsInterface {
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    /// SPI_PS_INPUT_ENA: which system values occupy the initial VGPRs
    pub input_ena: u32,
}

/// Mesh (NGG) shader interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MsInterface {
    /// per-vertex outputs (OSG1)
    pub outputs: Vec<SigElement>,
    /// threads per group (PSV0 numthreads.x)
    pub threads: u32,
    pub max_vertices: u32,
    pub max_primitives: u32,
}

/// dwords before the vertices of a mesh record: vertex count, primitive count, 2 spare
pub const MESH_HEADER: usize = 4;

impl MsInterface {
    /// dwords per vertex: pos0 + every param (4 each, raw bits)
    pub fn vert_dwords(&self) -> usize {
        4 * (1 + ranked(&self.outputs, true).len())
    }
    /// dwords of one group's record: header, vertices, primitives (1 dword each)
    pub fn record_dwords(&self) -> usize {
        MESH_HEADER + self.max_vertices as usize * self.vert_dwords() + self.max_primitives as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Kernel,
    Vertex(VsInterface),
    Pixel(PsInterface),
    Mesh(MsInterface),
}

/// Non-system elements in register order – the order of `exp param N` / `v_interp attrN`.
pub fn ranked(sig: &[SigElement], with_clip: bool) -> Vec<&SigElement> {
    let mut user: Vec<&SigElement> = sig.iter().filter(|e| e.system_value == 0 && !e.name.starts_with("SV_")).collect();
    user.sort_by_key(|e| e.register);
    if with_clip {
        let mut clip: Vec<&SigElement> = sig.iter().filter(|e| matches!(e.system_value, 2 | 3)).collect();
        clip.sort_by_key(|e| e.register);
        user.extend(clip);
    }
    user
}

/// Hardware order and VGPR sizes of the PS inputs enabled in SPI_PS_INPUT_ENA (bit → size):
/// persp sample/center/centroid (2), pull model (3), linear sample/center/centroid (2),
/// line stipple, pos x/y/z/w, front face, ancillary, sample coverage, pos fixed (1 each).
pub const PS_INPUT_SIZES: [u64; 16] = [2, 2, 2, 3, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1];

/// First VGPR of PS input `bit` (None if not enabled).
pub fn ps_input_vgpr(ena: u32, bit: u32) -> Option<u64> {
    if ena >> bit & 1 == 0 {
        return None;
    }
    Some((0..bit).filter(|b| ena >> b & 1 == 1).map(|b| PS_INPUT_SIZES[b as usize]).sum())
}

/// Interface of a console program from its container (None: not a VS/PS we can drive).
pub fn interface(blob: &[u8], sh: &super::xbox::XboxShader, f: &super::xbox::Fssm) -> Option<Mode> {
    let (inputs, outputs) = super::dxbc::signatures(blob);
    let hw = f.stages.first()?.hw;
    match (sh.stage.as_str(), hw) {
        ("VS", 4) => Some(Mode::Vertex(VsInterface { inputs, outputs, attr_base: 4 })),
        ("VS", 2) => Some(Mode::Vertex(VsInterface { inputs, outputs, attr_base: 9 })),
        ("PS", 5) => {
            let ena = f.data.get(0x248..0x24C).map(|b| u32::from_le_bytes(b.try_into().unwrap()))?;
            Some(Mode::Pixel(PsInterface { inputs, outputs, input_ena: ena }))
        }
        ("MS", 7) => {
            let p = super::dxbc::psv(blob)?;
            let threads = p.numthreads.map(|t| t[0] * t[1].max(1) * t[2].max(1)).filter(|&t| t > 0).unwrap_or(128);
            if p.ms_topology != 2 {
                return None; // lines / points: not yet
            }
            Some(Mode::Mesh(MsInterface {
                outputs,
                threads,
                max_vertices: (p.ms_max_vertices as u32).clamp(1, 256),
                max_primitives: (p.ms_max_primitives as u32).clamp(1, 256),
            }))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub mode: Mode,
    /// wave64 (64-bit EXEC/VCC) or wave32 program
    pub wave64: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { mode: Mode::Kernel, wave64: true }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Translation {
    pub hlsl: String,
    /// instructions we cannot translate yet (mnemonic → count); the HLSL still compiles,
    /// these become comments
    pub unsupported: BTreeMap<String, usize>,
    /// "mnemonic: reason" → count
    pub unsupported_why: BTreeMap<String, usize>,
    /// instructions without an effect in this model (waits, prefetch, mode changes…)
    pub ignored: BTreeMap<String, usize>,
    pub instructions: usize,
    pub blocks: usize,
}

impl Translation {
    pub fn complete(&self) -> bool {
        self.unsupported.is_empty()
    }
}

struct Ctx<'a> {
    insts: &'a [Decoded],
    starts: &'a BTreeSet<u64>,
    leaders: &'a [u64],
    loops: &'a [(u64, u64)],
}

/// Emit the blocks starting in [lo, hi) at nesting `depth`; loop ranges become `while`.
fn emit_range(c: &Ctx, em: &mut emit::Em, body: &mut String, lo: u64, hi: u64, depth: usize, blocks: &mut usize) {
    let pad = "    ".repeat(depth);
    let mut i = c.leaders.partition_point(|&l| l < lo);
    while i < c.leaders.len() && c.leaders[i] < hi {
        let start = c.leaders[i];
        let inner = c.loops.iter().filter(|&&(a, b)| a == start && b <= hi && (a, b) != (lo, hi)).max_by_key(|x| x.1).copied();
        if let Some((a, b)) = inner {
            body.push_str(&format!("{pad}[loop] while (pc >= {a}u && pc < {b}u) {{\n"));
            emit_range(c, em, body, a, b, depth + 1, blocks);
            body.push_str(&format!("{pad}}}\n"));
            i = c.leaders.partition_point(|&l| l < b);
            continue;
        }
        let end = c.leaders.get(i + 1).copied().unwrap_or(u64::MAX);
        body.push_str(&format!("{pad}if (pc == {start}u) {{\n"));
        em.indent = depth + 1;
        let mut flow = emit::Flow::Next;
        let block: Vec<&Decoded> = c.insts.iter().filter(|d| d.pc >= start && d.pc < end).collect();
        for (k, d) in block.iter().enumerate() {
            let next = block.get(k + 1).map(|n| n.pc).unwrap_or(end);
            flow = em.inst(d, next);
            if !matches!(flow, emit::Flow::Next) {
                break;
            }
        }
        body.push_str(&em.take());
        let fallthrough = if end == u64::MAX || !c.starts.contains(&end) { "0xFFFFFFFFu".to_string() } else { format!("{end}u") };
        let tail = match flow {
            emit::Flow::Next => format!("pc = {fallthrough};"),
            emit::Flow::Jump(t) => format!("pc = {t}u;"),
            emit::Flow::Cond(cond, t) => format!("pc = ({cond}) ? {t}u : {fallthrough};"),
            emit::Flow::End => "pc = 0xFFFFFFFFu;".to_string(),
        };
        body.push_str(&format!("{pad}    {tail}\n{pad}}}\n"));
        *blocks += 1;
        i += 1;
    }
}

/// Wave size the program was compiled for: 64-bit EXEC/VCC writes mean wave64.
pub fn detect_wave64(code: &[u8]) -> bool {
    let mut off = 0;
    let (mut w64, mut w32) = (0, 0);
    while off + 4 <= code.len() {
        let Ok(d) = rdna2::decode(&code[off..], off as u64) else { break };
        let n = d.inst.name;
        if n.ends_with("_SAVEEXEC_B64") || n.ends_with("_WREXEC_B64") || n.starts_with("V_CMPX") {
            w64 += n.ends_with("_B64") as i32;
        }
        if n.ends_with("_SAVEEXEC_B32") || n.ends_with("_WREXEC_B32") {
            w32 += 1;
        }
        // s_mov_b64 exec / s_and_b64 exec …: SDST 126
        if let Some(sd) = rdna2::field_value(d.enc, &d.words, "SDST") {
            if sd == 126 && n.ends_with("_B64") {
                w64 += 1;
            }
            if sd == 126 && n.ends_with("_B32") {
                w32 += 1;
            }
        }
        off += d.len;
    }
    !(w32 > 0 && w64 == 0)
}

pub fn translate(code: &[u8], opt: &Options) -> Result<Translation, String> {
    let mut insts: Vec<Decoded> = Vec::new();
    let mut off = 0usize;
    while off + 4 <= code.len() {
        let d = rdna2::decode(&code[off..], off as u64).map_err(|e| format!("+{off:#x}: nie da się zdekodować ({e:?})"))?;
        off += d.len;
        insts.push(d);
    }
    let starts: BTreeSet<u64> = insts.iter().map(|d| d.pc).collect();
    let mut leaders: BTreeSet<u64> = BTreeSet::from([0]);
    let mut branches = 0usize;
    for d in &insts {
        let next = d.pc + d.len as u64;
        if let Some(t) = rdna2::branch_target(d) {
            if !starts.contains(&t) {
                return Err(format!("+{:#x}: skok do {t:#x}, który nie jest początkiem instrukcji", d.pc));
            }
            leaders.insert(t);
            leaders.insert(next);
            branches += 1;
        } else if d.inst.flags & rdna2::F_TERMINATOR != 0 {
            leaders.insert(next);
        }
    }
    let mut em = emit::Em::new(opt.clone());
    em.max_vgpr = insts
        .iter()
        .flat_map(|d| {
            let (r, w) = rdna2::vgpr_uses(d);
            r.into_iter().chain(w)
        })
        .max()
        .unwrap_or(0);
    if let Mode::Pixel(ps) = &opt.mode {
        em.ps_modes = emit::ps_attr_modes(&insts, ps.input_ena);
    }
    let mut body = String::new();
    let mut blocks = 0usize;
    if branches == 0 {
        // straight line: stop at the first s_endpgm (the rest is unreachable)
        em.indent = 1;
        for (i, d) in insts.iter().enumerate() {
            let next = insts.get(i + 1).map(|n| n.pc).unwrap_or(u64::MAX);
            if let emit::Flow::End = em.inst(d, next) {
                break;
            }
        }
        body.push_str(&em.take());
        blocks = 1;
    } else {
        // Blocks in address order as a chain of `if (pc == start)`: a forward branch is found
        // further down the chain, so only ranges spanned by backward branches need a loop
        // (`while (pc in range)`); ranges are merged until they nest. Same semantics as a
        // dispatcher loop over all blocks, but DXC sees an acyclic graph almost everywhere
        // (optimising one big dispatcher loop took minutes on large retail programs).
        let leaders: Vec<u64> = leaders.into_iter().filter(|l| starts.contains(l)).collect();
        let mut loops: Vec<(u64, u64)> = Vec::new();
        for d in &insts {
            if let Some(t) = rdna2::branch_target(d) {
                if t <= d.pc {
                    loops.push((t, d.pc + d.len as u64));
                }
            }
        }
        loop {
            let mut merged = false;
            'scan: for i in 0..loops.len() {
                for j in 0..loops.len() {
                    let (a, b) = (loops[i], loops[j]);
                    if i != j && a.0 < b.0 && b.0 < a.1 && a.1 < b.1 {
                        loops[i] = (a.0, b.1);
                        loops.remove(j);
                        merged = true;
                        break 'scan;
                    }
                }
            }
            if !merged {
                break;
            }
        }
        loops.sort();
        loops.dedup();
        let ctx = Ctx { insts: &insts, starts: &starts, leaders: &leaders, loops: &loops };
        body.push_str("    uint pc = 0;\n");
        emit_range(&ctx, &mut em, &mut body, 0, u64::MAX, 1, &mut blocks);
    }
    let hlsl = em.finish(&body);
    Ok(Translation { hlsl, unsupported: em.unsupported, unsupported_why: em.unsupported_why, ignored: em.ignored, instructions: insts.len(), blocks })
}
