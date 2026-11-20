//! Instruction semantics: one RDNA2 instruction → HLSL statements.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::{ps_input_vgpr, ranked, Mode, MsInterface, Options, KERNEL_EXPORTS, KERNEL_LANES, KERNEL_SCC_SLOT, KERNEL_SGPRS, KERNEL_VGPRS, MESH_HEADER};
use crate::gpu::dxbc::SigElement;
use crate::gpu::rdna2::{self, Decoded, Encoding, Operand, Variant};

pub(super) enum Flow {
    Next,
    Jump(u64),
    Cond(String, u64),
    End,
}

pub(super) struct Em {
    opt: Options,
    out: String,
    pub indent: usize,
    sgprs: BTreeSet<u64>,
    vgprs: BTreeSet<u64>,
    pub unsupported: BTreeMap<String, usize>,
    /// "mnemonic: reason" → count
    pub unsupported_why: BTreeMap<String, usize>,
    pub ignored: BTreeMap<String, usize>,
    tmp: usize,
    /// extra write enable of the current DPP16 instruction
    dpp_enable: Option<String>,
    /// PS: interpolation modifier per attribute ("", "centroid", "nointerpolation" …)
    pub ps_modes: BTreeMap<u32, String>,
    /// PS: an export with DONE was emitted (lanes without it are discarded)
    ps_done: bool,
    /// LDS used (compute): declare the groupshared array
    uses_lds: bool,
    /// highest VGPR the program references (v_movrel* ranges)
    pub max_vgpr: u64,
}

/// Instructions without an effect in this execution model.
const NOOPS: &[&str] = &[
    "s_nop", "s_waitcnt", "s_waitcnt_vscnt", "s_waitcnt_vmcnt", "s_waitcnt_expcnt", "s_waitcnt_lgkmcnt", "s_waitcnt_depctr",
    "s_inst_prefetch", "s_clause", "s_code_end", "s_setprio", "s_sleep", "s_icache_inv", "s_dcache_inv", "s_gl1_inv", "s_ttracedata",
    "s_ttracedata_imm", "v_nop", "s_denorm_mode", "s_round_mode", "s_setreg_imm32_b32", "s_setreg_b32", "s_sendmsg", "s_sendmsghalt",
    "s_set_gpr_idx_off", "s_version",
];

fn sname(code: u64) -> Option<String> {
    Some(match code {
        0..=105 => format!("s{code}"),
        106 => "vcc_lo".into(),
        107 => "vcc_hi".into(),
        108..=123 => format!("ttmp{}", code - 108),
        124 => "m0".into(),
        126 => "exec_lo".into(),
        127 => "exec_hi".into(),
        _ => return None,
    })
}

fn f32_const(code: u64) -> Option<f32> {
    Some(match code {
        240 => 0.5,
        241 => -0.5,
        242 => 1.0,
        243 => -1.0,
        244 => 2.0,
        245 => -2.0,
        246 => 4.0,
        247 => -4.0,
        248 => 0.159_154_94,
        _ => return None,
    })
}

fn f16_const(code: u64) -> Option<u32> {
    Some(match code {
        240 => 0x3800,
        241 => 0xB800,
        242 => 0x3C00,
        243 => 0xBC00,
        244 => 0x4000,
        245 => 0xC000,
        246 => 0x4400,
        247 => 0xC400,
        248 => 0x3118,
        _ => return None,
    })
}

fn hex(v: u32) -> String {
    format!("0x{v:08X}u")
}

#[derive(Default, Clone, Copy)]
struct Mods {
    abs: [bool; 3],
    neg: [bool; 3],
    omod: u64,
    clamp: bool,
    opsel: u64,
}

/// Layout of the SDWA / DPP extension dword for this instruction's base encoding.
fn ext_encoding(d: &Decoded) -> Option<&'static Encoding> {
    let base = match d.enc.name {
        "ENC_VOP1" => "VOP1",
        "ENC_VOP2" => "VOP2",
        "ENC_VOPC" => "VOPC",
        _ => return None,
    };
    let name = match (d.variant, base) {
        (Variant::Sdwa, "VOPC") => "VOPC_VOP_SDWA_SDST_ENC".to_string(),
        (Variant::Sdwa, b) => format!("{b}_VOP_SDWA"),
        (Variant::Dpp16, b) => format!("{b}_VOP_DPP16"),
        (Variant::Dpp8, b) => format!("{b}_VOP_DPP8"),
        _ => return None,
    };
    rdna2::encoding(&name)
}

/// Source lane of a DPP16 control (−1: no source), as an HLSL expression.
fn dpp16_src(ctrl: u64) -> Option<String> {
    let n = ctrl & 15;
    Some(match ctrl {
        0x000..=0x0FF => format!("(int)((lane_id & ~3u) | (({ctrl}u >> (2u * (lane_id & 3u))) & 3u))"),
        0x101..=0x10F => format!("(((lane_id & 15u) + {n}u < 16u) ? (int)(lane_id + {n}u) : -1)"),
        0x111..=0x11F => format!("(((lane_id & 15u) >= {n}u) ? (int)(lane_id - {n}u) : -1)"),
        0x121..=0x12F => format!("(int)((lane_id & ~15u) | (((lane_id & 15u) - {n}u) & 15u))"),
        0x140 => "(int)((lane_id & ~15u) | (15u - (lane_id & 15u)))".into(),
        0x141 => "(int)((lane_id & ~15u) | (lane_id & 8u) | (7u - (lane_id & 7u)))".into(),
        0x150..=0x15F => format!("(int)((lane_id & ~15u) | {n}u)"),
        0x160..=0x16F => format!("(int)(lane_id ^ {n}u)"),
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BufKind {
    Load,
    Store,
    FormatLoad,
    FormatStore,
}

fn buffer_op(name: &str) -> Option<(BufKind, u64)> {
    let n = |suffix: &str| -> Option<u64> {
        Some(match suffix {
            "dword" | "x" => 1,
            "dwordx2" | "xy" => 2,
            "dwordx3" | "xyz" => 3,
            "dwordx4" | "xyzw" => 4,
            _ => return None,
        })
    };
    if let Some(r) = name.strip_prefix("buffer_load_format_").or_else(|| name.strip_prefix("tbuffer_load_format_")) {
        return Some((BufKind::FormatLoad, n(r)?));
    }
    if let Some(r) = name.strip_prefix("buffer_store_format_").or_else(|| name.strip_prefix("tbuffer_store_format_")) {
        return Some((BufKind::FormatStore, n(r)?));
    }
    if let Some(r) = name.strip_prefix("buffer_load_") {
        return Some((BufKind::Load, n(r)?));
    }
    if let Some(r) = name.strip_prefix("buffer_store_") {
        return Some((BufKind::Store, n(r)?));
    }
    None
}

/// GFX10 unified buffer formats (index = FORMAT value; LLVM `UfmtSymbolicGFX10`).
pub const BUF_FORMATS: [&str; 78] = [
    "INVALID", "8_UNORM", "8_SNORM", "8_USCALED", "8_SSCALED", "8_UINT", "8_SINT", "16_UNORM", "16_SNORM", "16_USCALED",
    "16_SSCALED", "16_UINT", "16_SINT", "16_FLOAT", "8_8_UNORM", "8_8_SNORM", "8_8_USCALED", "8_8_SSCALED", "8_8_UINT", "8_8_SINT",
    "32_UINT", "32_SINT", "32_FLOAT", "16_16_UNORM", "16_16_SNORM", "16_16_USCALED", "16_16_SSCALED", "16_16_UINT", "16_16_SINT",
    "16_16_FLOAT", "10_11_11_UNORM", "10_11_11_SNORM", "10_11_11_USCALED", "10_11_11_SSCALED", "10_11_11_UINT", "10_11_11_SINT",
    "10_11_11_FLOAT", "11_11_10_UNORM", "11_11_10_SNORM", "11_11_10_USCALED", "11_11_10_SSCALED", "11_11_10_UINT", "11_11_10_SINT",
    "11_11_10_FLOAT", "10_10_10_2_UNORM", "10_10_10_2_SNORM", "10_10_10_2_USCALED", "10_10_10_2_SSCALED", "10_10_10_2_UINT",
    "10_10_10_2_SINT", "2_10_10_10_UNORM", "2_10_10_10_SNORM", "2_10_10_10_USCALED", "2_10_10_10_SSCALED", "2_10_10_10_UINT",
    "2_10_10_10_SINT", "8_8_8_8_UNORM", "8_8_8_8_SNORM", "8_8_8_8_USCALED", "8_8_8_8_SSCALED", "8_8_8_8_UINT", "8_8_8_8_SINT",
    "32_32_UINT", "32_32_SINT", "32_32_FLOAT", "16_16_16_16_UNORM", "16_16_16_16_SNORM", "16_16_16_16_USCALED",
    "16_16_16_16_SSCALED", "16_16_16_16_UINT", "16_16_16_16_SINT", "16_16_16_16_FLOAT", "32_32_32_UINT", "32_32_32_SINT",
    "32_32_32_FLOAT", "32_32_32_32_UINT", "32_32_32_32_SINT", "32_32_32_32_FLOAT",
];

/// Packed format info for the HLSL table: component count [2:0], numeric type [5:3]
/// (0 unorm, 1 snorm, 2 uscaled, 3 sscaled, 4 uint, 5 sint, 6 float), component widths
/// (x first = least significant bits; names list the most significant component first)
/// at [8 + 6k +: 6].
pub fn buf_format_info(idx: usize) -> u32 {
    let name = BUF_FORMATS[idx];
    let Some((sizes, ty)) = name.rsplit_once('_') else { return 0 };
    let ty = match ty {
        "UNORM" => 0,
        "SNORM" => 1,
        "USCALED" => 2,
        "SSCALED" => 3,
        "UINT" => 4,
        "SINT" => 5,
        "FLOAT" => 6,
        _ => return 0,
    };
    let mut w: Vec<u32> = sizes.split('_').filter_map(|x| x.parse().ok()).collect();
    w.reverse();
    let mut info = w.len() as u32 | ty << 3;
    for (k, x) in w.iter().enumerate() {
        info |= x << (8 + 6 * k);
    }
    info
}

/// A destination register (pair).
#[derive(Clone)]
enum Dst {
    V(u64),
    S(u64),
    Null,
}

impl Em {
    pub fn new(opt: Options) -> Self {
        Em {
            opt,
            out: String::new(),
            indent: 1,
            sgprs: BTreeSet::new(),
            vgprs: BTreeSet::new(),
            unsupported: BTreeMap::new(),
            unsupported_why: BTreeMap::new(),
            ignored: BTreeMap::new(),
            tmp: 0,
            dpp_enable: None,
            ps_modes: BTreeMap::new(),
            ps_done: false,
            uses_lds: false,
            max_vgpr: 0,
        }
    }

    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.out)
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn t(&mut self) -> String {
        self.tmp += 1;
        format!("t{}", self.tmp)
    }

    fn act(&self) -> &'static str {
        if self.opt.wave64 {
            "lanebit(exec_lo, exec_hi)"
        } else {
            "lanebit(exec_lo, 0u)"
        }
    }

    fn unsupported(&mut self, d: &Decoded, why: &str) {
        let m = rdna2::mnemonic(d);
        *self.unsupported_why.entry(format!("{m}: {why}")).or_default() += 1;
        *self.unsupported.entry(m).or_default() += 1;
        let txt = rdna2::format(d);
        self.line(&format!("/* NIEOBSŁUGIWANE ({why}): {txt} */"));
    }

    // ---------------------------------------------------------------- registers

    fn sreg(&mut self, code: u64) -> String {
        match sname(code) {
            Some(n) => {
                self.sgprs.insert(code);
                n
            }
            None => "0u".into(),
        }
    }

    fn vreg(&mut self, n: u64) -> String {
        self.vgprs.insert(n);
        format!("v{n}")
    }

    /// 32-bit value of a scalar source code (0..255) or VALU source (256+ = VGPR).
    fn code32(&mut self, code: u64, lit: Option<u32>, f16: bool) -> Option<String> {
        Some(match code {
            0..=124 | 126 | 127 => self.sreg(code),
            125 => "0u".into(),
            128..=192 => format!("{}u", code - 128),
            193..=208 => hex((-((code - 192) as i64)) as u32),
            240..=248 => {
                if f16 {
                    hex(f16_const(code)?)
                } else {
                    hex(f32_const(code)?.to_bits())
                }
            }
            251 => {
                let (l, h) = (self.sreg(106), self.sreg(107));
                format!("((({l} | {h}) == 0u) ? 1u : 0u)")
            }
            252 => {
                let (l, h) = (self.sreg(126), self.sreg(127));
                format!("((({l} | {h}) == 0u) ? 1u : 0u)")
            }
            253 => "scc".into(),
            255 => hex(lit?),
            256..=511 => self.vreg(code - 256),
            // flat apertures (shared/private base and limit): the flat memory model has none
            235..=238 => "0u".into(),
            // POPS exiting wave id: every overlapping earlier wave has already left, so the
            // ordered-section wait passes at once (no ROV ordering yet – known limitation)
            239 => "0x7FFFFFFFu".into(),
            _ => return None,
        })
    }

    /// 64-bit value (lo, hi) of a scalar source code.
    fn code64(&mut self, code: u64, lit: Option<u32>) -> Option<(String, String)> {
        Some(match code {
            0..=123 | 126 => (self.sreg(code), self.sreg(code + 1)),
            125 => ("0u".into(), "0u".into()),
            128..=192 => (format!("{}u", code - 128), "0u".into()),
            193..=208 => (hex((-((code - 192) as i64)) as u32), hex(0xFFFF_FFFF)),
            248 => (hex(0x6DC9_C882), hex(0x3FC4_5F30)), // 1/(2*pi) as a double
            240..=247 => {
                let b = (f32_const(code)? as f64).to_bits();
                (hex(b as u32), hex((b >> 32) as u32))
            }
            253 => ("scc".into(), "0u".into()),
            255 => (hex(lit?), "0u".into()),
            256..=510 => (self.vreg(code - 256), self.vreg(code - 255)),
            _ => return None,
        })
    }

    fn field(d: &Decoded, f: &str) -> u64 {
        rdna2::field_value(d.enc, &d.words, f).unwrap_or(0) * rdna2::register_scale(f)
    }

    /// Value of an input operand as one 32-bit expression.
    fn read(&mut self, d: &Decoded, o: &Operand, f16: bool) -> Option<String> {
        match o.field {
            Some("LITERAL") => Some(hex(d.literal?)),
            Some(f) => {
                let v = Self::field(d, f);
                let width = d.enc.field_width(f);
                if o.ty.starts_with("OPR_VGPR") && width <= 8 {
                    return Some(self.vreg(v));
                }
                match o.ty {
                    "OPR_SIMM16" => Some(format!("{}u", v)),
                    "OPR_SIMM32" => Some(hex(d.literal?)),
                    _ if width >= 9 => self.code32(v, d.literal, f16),
                    _ => self.code32(v, d.literal, f16),
                }
            }
            None => match o.ty {
                "OPR_VCC" => Some(self.sreg(106)),
                "OPR_EXEC" | "OPR_SDST_EXEC" => Some(self.sreg(126)),
                "OPR_SDST_M0" => Some(self.sreg(124)),
                "OPR_SSRC_SPECIAL_SCC" => Some("scc".into()),
                _ => None,
            },
        }
    }

    fn read64(&mut self, d: &Decoded, o: &Operand) -> Option<(String, String)> {
        match o.field {
            Some(f) => {
                let v = Self::field(d, f);
                if o.ty.starts_with("OPR_VGPR") && d.enc.field_width(f) <= 8 {
                    return Some((self.vreg(v), self.vreg(v + 1)));
                }
                self.code64(v, d.literal)
            }
            None => match o.ty {
                "OPR_VCC" => Some((self.sreg(106), self.sreg(107))),
                "OPR_EXEC" | "OPR_SDST_EXEC" => Some((self.sreg(126), self.sreg(127))),
                _ => None,
            },
        }
    }

    fn dst_of(d: &Decoded, o: &Operand) -> Dst {
        match o.field {
            Some(f) => {
                let v = Self::field(d, f);
                match o.ty {
                    "OPR_EXEC" => Dst::S(126),
                    "OPR_VCC" => Dst::S(106),
                    t if t.starts_with("OPR_VGPR") => Dst::V(v),
                    _ if v == 125 => Dst::Null,
                    _ if v > 127 => Dst::Null,
                    _ => Dst::S(v),
                }
            }
            None => match o.ty {
                "OPR_VCC" => Dst::S(106),
                "OPR_EXEC" => Dst::S(126),
                _ => Dst::Null,
            },
        }
    }

    /// Explicit operands: (outputs, inputs) in spec order.
    fn ops(d: &Decoded) -> (Vec<&'static Operand>, Vec<&'static Operand>) {
        let mut outs = Vec::new();
        let mut ins = Vec::new();
        for o in d.ienc.operands {
            if o.implicit && !(o.field.is_none() && o.ty == "OPR_VCC") {
                continue;
            }
            if o.output {
                outs.push(o);
            } else if o.input {
                ins.push(o);
            }
        }
        (outs, ins)
    }

    fn write32(&mut self, dst: &Dst, val: &str) {
        match dst {
            // select, not `if`: straight-line code (a branch per VALU write was more than the
            // driver's compiler could take on a 15 000-instruction console shader)
            Dst::V(n) => {
                let v = self.vreg(*n);
                let a = self.act();
                self.line(&format!("{v} = select({a}, {val}, {v});"));
            }
            Dst::S(n) => {
                let s = self.sreg(*n);
                self.line(&format!("{s} = {val};"));
            }
            Dst::Null => {}
        }
    }

    fn write64(&mut self, dst: &Dst, lo: &str, hi: &str) {
        match dst {
            Dst::V(n) => {
                let (a, b) = (self.vreg(*n), self.vreg(*n + 1));
                let act = self.act();
                self.line(&format!("{a} = select({act}, {lo}, {a}); {b} = select({act}, {hi}, {b});"));
            }
            Dst::S(n) => {
                let (a, b) = (self.sreg(*n), self.sreg(*n + 1));
                self.line(&format!("{a} = {lo}; {b} = {hi};"));
            }
            Dst::Null => {}
        }
    }

    /// Lane mask from a per-lane condition (inactive lanes give 0) into a register pair.
    fn write_mask(&mut self, dst: &Dst, cond: &str) {
        let a = self.act();
        let m = self.t();
        self.line(&format!("uint2 {m} = ballot({a} && ({cond}));"));
        if self.opt.wave64 {
            self.write64(dst, &format!("{m}.x"), &format!("{m}.y"));
        } else {
            self.write32(dst, &format!("{m}.x"));
        }
    }

    fn lane_of(&mut self, lo: &str, hi: &str) -> String {
        if self.opt.wave64 {
            format!("lanebit({lo}, {hi})")
        } else {
            format!("lanebit({lo}, 0u)")
        }
    }

    // ---------------------------------------------------------------- one instruction

    pub fn inst(&mut self, d: &Decoded, next: u64) -> Flow {
        let name = d.inst.name.to_ascii_lowercase();
        let text = rdna2::format(d);
        self.line(&format!("// {:04x}: {text}", d.pc));
        // geometry mode: the ES's s_setpc_b64 continues into the GS code (ES alone: the end)
        if name == "s_setpc_b64" {
            if let Mode::Geometry(g) = &self.opt.mode {
                return match g.gs_start {
                    Some(s) if s > 0 => Flow::Jump(s),
                    _ => Flow::End,
                };
            }
        }
        // s_sendmsg is a no-op except GS_ALLOC_REQ (mesh) and GS emit/cut (geometry)
        let mesh_msg = name == "s_sendmsg" && matches!(self.opt.mode, Mode::Mesh(_) | Mode::Geometry(_));
        if NOOPS.contains(&name.as_str()) && !mesh_msg {
            *self.ignored.entry(name).or_default() += 1;
            return Flow::Next;
        }
        let r = match d.enc.name {
            "ENC_SOPP" => return self.sopp(d, &name, next),
            "ENC_SOP1" | "ENC_SOP2" | "ENC_SOPC" | "ENC_SOPK" => self.salu(d, &name),
            "ENC_SMEM" => self.smem(d, &name),
            "ENC_MUBUF" | "ENC_MTBUF" => self.mubuf(d, &name),
            "ENC_EXP" => self.export(d),
            "ENC_DS" => self.ds(d, &name),
            "ENC_VINTRP" => self.interp(d, &name),
            "ENC_MIMG" => self.mimg(d, &name),
            "ENC_FLAT" | "ENC_FLAT_GLBL" => self.flat(d, &name),
            e if e.starts_with("ENC_VOP") || e.starts_with("VOP") => self.valu(d, &name),
            _ => Err("kodowanie"),
        };
        if let Err(why) = r {
            self.unsupported(d, why);
        }
        Flow::Next
    }

    fn sopp(&mut self, d: &Decoded, name: &str, _next: u64) -> Flow {
        let vccz = |s: &mut Self| {
            let (l, h) = (s.sreg(106), s.sreg(107));
            if s.opt.wave64 {
                format!("({l} | {h}) == 0u")
            } else {
                format!("{l} == 0u")
            }
        };
        let execz = |s: &mut Self| {
            let (l, h) = (s.sreg(126), s.sreg(127));
            if s.opt.wave64 {
                format!("({l} | {h}) == 0u")
            } else {
                format!("{l} == 0u")
            }
        };
        match name {
            "s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" => Flow::End,
            "s_barrier" => {
                if matches!(self.opt.mode, Mode::Kernel | Mode::Mesh(_) | Mode::Geometry(_) | Mode::Compute(_)) {
                    self.line("GroupMemoryBarrierWithGroupSync();");
                }
                Flow::Next
            }
            "s_sendmsg" if matches!(self.opt.mode, Mode::Geometry(_)) => {
                // MSG_GS (2): op 1 cut, 2 emit, 3 emit + cut (stream 0); GS_DONE (3) – nothing to do
                let simm = rdna2::field_value(d.enc, &d.words, "SIMM16").unwrap_or(0);
                let (msg, op, stream) = (simm & 0xF, (simm >> 4) & 3, (simm >> 8) & 3);
                if let (2, 0, Mode::Geometry(g)) = (msg, stream, &self.opt.mode) {
                    let maxt = g.max_vertex_count.saturating_sub(2).max(1);
                    let act = self.act();
                    if op & 2 != 0 {
                        // strip triangle j = (j, j+1, j+2), odd j with the first two swapped
                        self.line(&format!(
                            "if ({act}) {{ uint k = xcl_emitted; if (xcl_strip >= 2u && xcl_ntri < {maxt}u) {{ xcl_tri[xcl_ntri] = ((xcl_strip & 1u) == 0u) ? ((k - 2u) | ((k - 1u) << 10) | (k << 20)) : ((k - 1u) | ((k - 2u) << 10) | (k << 20)); xcl_ntri++; }} xcl_emitted++; xcl_strip++; }}"
                        ));
                    }
                    if op & 1 != 0 {
                        self.line(&format!("if ({act}) xcl_strip = 0u;"));
                    }
                }
                Flow::Next
            }
            "s_sendmsg" if matches!(self.opt.mode, Mode::Mesh(_)) => {
                // GS_ALLOC_REQ: M0 = primitives << 12 | vertices (scalar: whatever EXEC is)
                if rdna2::field_value(d.enc, &d.words, "SIMM16").unwrap_or(0) & 0xF == 9 {
                    let m0 = self.sreg(124);
                    self.line(&format!(
                        "if (lane_id == 0u) {{ res.Store(xcl_rec * 4u, {m0} & 0x3FFu); res.Store(xcl_rec * 4u + 4u, ({m0} >> 12) & 0x7FFu); }}"
                    ));
                }
                Flow::Next
            }
            _ => {
                let Some(t) = rdna2::branch_target(d) else {
                    if NOOPS.contains(&name) {
                        return Flow::Next;
                    }
                    self.unsupported(d, "SOPP");
                    return Flow::Next;
                };
                match name {
                    "s_branch" => Flow::Jump(t),
                    "s_cbranch_scc0" => Flow::Cond("scc == 0u".into(), t),
                    "s_cbranch_scc1" => Flow::Cond("scc != 0u".into(), t),
                    "s_cbranch_vccz" => Flow::Cond(vccz(self), t),
                    "s_cbranch_vccnz" => Flow::Cond(format!("!({})", vccz(self)), t),
                    "s_cbranch_execz" => Flow::Cond(execz(self), t),
                    "s_cbranch_execnz" => Flow::Cond(format!("!({})", execz(self)), t),
                    _ => {
                        self.unsupported(d, "skok warunkowy");
                        Flow::Next
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- scalar ALU

    fn salu(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let (outs, ins) = Self::ops(d);
        let wide = name.ends_with("_b64") || name.ends_with("_u64") || name.ends_with("_i64");
        let dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
        let simm = || rdna2::field_value(d.enc, &d.words, "SIMM16").unwrap_or(0) as u16;
        let base = name.trim_end_matches("_b32").trim_end_matches("_b64");
        // SOPK: SDST is also the first source
        if d.enc.name == "ENC_SOPK" {
            let k_s = simm() as i16 as i32 as u32;
            let k_u = simm() as u32;
            let sd = Self::field(d, "SDST");
            let cur = self.code32(sd, None, false).ok_or("rejestr")?;
            let dst = Dst::S(sd);
            match name {
                "s_movk_i32" => self.write32(&dst, &hex(k_s)),
                "s_cmovk_i32" => self.write32(&dst, &format!("(scc != 0u) ? {} : {cur}", hex(k_s))),
                "s_addk_i32" => {
                    let t = self.t();
                    self.line(&format!("uint {t} = {cur} + {};", hex(k_s)));
                    self.line(&format!("scc = ((({cur} ^ {t}) & ({} ^ {t})) >> 31);", hex(k_s)));
                    self.write32(&dst, &t);
                }
                "s_mulk_i32" => self.write32(&dst, &format!("(uint)((int){cur} * {})", k_s as i32)),
                _ if name.starts_with("s_cmpk_") => {
                    let (op, signed) = cmp_op(&name[7..]).ok_or("porównanie")?;
                    let (a, b) = if signed { (format!("(int){cur}"), format!("{}", k_s as i32)) } else { (cur.clone(), format!("{k_u}u")) };
                    self.line(&format!("scc = ({a} {op} {b}) ? 1u : 0u;"));
                }
                _ => return Err("SOPK"),
            }
            return Ok(());
        }
        if d.enc.name == "ENC_SOPC" {
            let a64 = name.ends_with("_u64");
            if a64 {
                let (a, b) = (self.read64(d, ins[0]).ok_or("op")?, self.read64(d, ins[1]).ok_or("op")?);
                let op = match name {
                    "s_cmp_eq_u64" => "==",
                    "s_cmp_lg_u64" => "!=",
                    _ => return Err("SOPC"),
                };
                let j = if op == "==" { "&&" } else { "||" };
                self.line(&format!("scc = ({} {op} {} {j} {} {op} {}) ? 1u : 0u;", a.0, b.0, a.1, b.1));
                return Ok(());
            }
            let (a, b) = (self.read(d, ins[0], false).ok_or("op")?, self.read(d, ins[1], false).ok_or("op")?);
            if let Some(rest) = name.strip_prefix("s_cmp_") {
                let (op, signed) = cmp_op(rest).ok_or("porównanie")?;
                let (a, b) = if signed { (format!("(int){a}"), format!("(int){b}")) } else { (a, b) };
                self.line(&format!("scc = ({a} {op} {b}) ? 1u : 0u;"));
                return Ok(());
            }
            match name {
                "s_bitcmp0_b32" => self.line(&format!("scc = ((({a} >> ({b} & 31u)) & 1u) == 0u) ? 1u : 0u;")),
                "s_bitcmp1_b32" => self.line(&format!("scc = (({a} >> ({b} & 31u)) & 1u);")),
                _ => return Err("SOPC"),
            }
            return Ok(());
        }
        // SOP1 / SOP2
        let saveexec = name.contains("saveexec") || name.contains("wrexec");
        if saveexec {
            let s0 = self.read64(d, ins[0]).ok_or("op")?;
            let (el, eh) = (self.sreg(126), self.sreg(127));
            let op = |a: &str, e: &str| -> Option<String> {
                Some(match base.split('_').nth(1)? {
                    "and" => format!("{a} & {e}"),
                    "or" => format!("{a} | {e}"),
                    "xor" => format!("{a} ^ {e}"),
                    "andn2" => format!("{a} & ~{e}"),
                    "orn2" => format!("{a} | ~{e}"),
                    "nand" => format!("~({a} & {e})"),
                    "nor" => format!("~({a} | {e})"),
                    "xnor" => format!("~({a} ^ {e})"),
                    "andn1" => format!("~{a} & {e}"),
                    "orn1" => format!("~{a} | {e}"),
                    _ => return None,
                })
            };
            let (nl, nh) = (op(&s0.0, &el).ok_or("saveexec")?, op(&s0.1, &eh).ok_or("saveexec")?);
            let (t0, t1, o0, o1) = (self.t(), self.t(), self.t(), self.t());
            self.line(&format!("uint {o0} = {el}; uint {o1} = {eh};"));
            self.line(&format!("uint {t0} = {nl}; uint {t1} = {nh};"));
            if !self.opt.wave64 || name.ends_with("_b32") {
                self.line(&format!("{el} = {t0};"));
                self.line(&format!("scc = ({t0} != 0u) ? 1u : 0u;"));
                let v = if name.contains("wrexec") { t0.clone() } else { o0.clone() };
                self.write32(&dst, &v);
            } else {
                self.line(&format!("{el} = {t0}; {eh} = {t1};"));
                self.line(&format!("scc = (({t0} | {t1}) != 0u) ? 1u : 0u;"));
                let (a, b) = if name.contains("wrexec") { (t0, t1) } else { (o0, o1) };
                self.write64(&dst, &a, &b);
            }
            return Ok(());
        }
        if name == "s_swappc_b64" || name == "s_setpc_b64" {
            // s_swappc_b64 s[n:n+1], s[n:n+1] at the start of a VS: call of the fetch shader
            // the console driver builds from the input layout; here the attributes are
            // already in their VGPRs (kernel mode) – other indirect jumps are not supported
            let (sd, s0) = (Self::field(d, "SDST"), Self::field(d, "SSRC0"));
            if name == "s_swappc_b64" && sd == s0 {
                self.line("/* wywołanie fetch shadera – atrybuty wejściowe są już w VGPR */");
                *self.ignored.entry("s_swappc_b64 (fetch shader)".into()).or_default() += 1;
                return Ok(());
            }
            return Err("skok pośredni");
        }
        if name == "s_getpc_b64" {
            // the code region is addressed from 0 in this model
            let pc = d.pc + d.len as u64;
            self.write64(&dst, &hex(pc as u32), "0u");
            return Ok(());
        }
        if wide {
            return self.salu64(d, name, &dst, &ins);
        }
        let a = match ins.first() {
            Some(o) => self.read(d, o, false).ok_or("op")?,
            None => String::new(),
        };
        let b = match ins.get(1) {
            Some(o) => self.read(d, o, false).ok_or("op")?,
            None => String::new(),
        };
        let t = self.t();
        // (result expression, SCC expression over the result `t` or None)
        let (res, scc): (String, Option<String>) = match name {
            "s_mov_b32" => (a.clone(), None),
            "s_cmov_b32" => {
                let cur = match &dst {
                    Dst::S(n) => self.sreg(*n),
                    _ => "0u".into(),
                };
                (format!("(scc != 0u) ? {a} : {cur}"), None)
            }
            "s_not_b32" => (format!("~{a}"), Some(format!("{t} != 0u"))),
            "s_brev_b32" => (format!("reversebits({a})"), None),
            "s_bcnt0_i32_b32" => (format!("32u - countbits({a})"), Some(format!("{t} != 0u"))),
            "s_bcnt1_i32_b32" => (format!("countbits({a})"), Some(format!("{t} != 0u"))),
            "s_ff0_i32_b32" => (format!("firstbitlow(~{a})"), None),
            "s_ff1_i32_b32" => (format!("firstbitlow({a})"), None),
            "s_flbit_i32_b32" => (format!("(({a}) == 0u ? 0xFFFFFFFFu : 31u - firstbithigh({a}))"), None),
            "s_abs_i32" => (format!("(uint)abs((int){a})"), Some(format!("{t} != 0u"))),
            "s_sext_i32_i8" => (format!("(uint)(((int)({a} << 24)) >> 24)"), None),
            "s_sext_i32_i16" => (format!("(uint)(((int)({a} << 16)) >> 16)"), None),
            "s_and_b32" => (format!("{a} & {b}"), Some(format!("{t} != 0u"))),
            "s_or_b32" => (format!("{a} | {b}"), Some(format!("{t} != 0u"))),
            "s_xor_b32" => (format!("{a} ^ {b}"), Some(format!("{t} != 0u"))),
            "s_andn2_b32" => (format!("{a} & ~{b}"), Some(format!("{t} != 0u"))),
            "s_orn2_b32" => (format!("{a} | ~{b}"), Some(format!("{t} != 0u"))),
            "s_nand_b32" => (format!("~({a} & {b})"), Some(format!("{t} != 0u"))),
            "s_nor_b32" => (format!("~({a} | {b})"), Some(format!("{t} != 0u"))),
            "s_xnor_b32" => (format!("~({a} ^ {b})"), Some(format!("{t} != 0u"))),
            "s_add_u32" => (format!("{a} + {b}"), Some(format!("{t} < {a}"))),
            "s_sub_u32" => (format!("{a} - {b}"), Some(format!("{b} > {a}"))),
            "s_addc_u32" => (format!("{a} + {b} + scc"), Some(format!("((uint64_t){a} + (uint64_t){b} + (uint64_t)scc) > 0xFFFFFFFFull"))),
            "s_subb_u32" => (format!("{a} - {b} - scc"), Some(format!("((uint64_t){b} + (uint64_t)scc) > (uint64_t){a}"))),
            "s_add_i32" => (format!("{a} + {b}"), Some(format!("((({a} ^ {t}) & ({b} ^ {t})) >> 31) != 0u"))),
            "s_sub_i32" => (format!("{a} - {b}"), Some(format!("((({a} ^ {b}) & ({a} ^ {t})) >> 31) != 0u"))),
            "s_mul_i32" => (format!("(uint)((int){a} * (int){b})"), None),
            "s_mul_hi_u32" => (format!("(uint)(((uint64_t){a} * (uint64_t){b}) >> 32)"), None),
            "s_mul_hi_i32" => (format!("(uint)(((int64_t)(int){a} * (int64_t)(int){b}) >> 32)"), None),
            "s_lshl_b32" => (format!("{a} << ({b} & 31u)"), Some(format!("{t} != 0u"))),
            "s_lshr_b32" => (format!("{a} >> ({b} & 31u)"), Some(format!("{t} != 0u"))),
            "s_ashr_i32" => (format!("(uint)((int){a} >> ({b} & 31u))"), Some(format!("{t} != 0u"))),
            "s_lshl1_add_u32" | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32" => {
                let k = &name[6..7];
                (format!("({a} << {k}u) + {b}"), Some(format!("((((uint64_t){a}) << {k}) + (uint64_t){b}) > 0xFFFFFFFFull")))
            }
            "s_bfm_b32" => (format!("((({a} & 31u) == 0u ? 0u : (0xFFFFFFFFu >> (32u - ({a} & 31u))))) << ({b} & 31u)"), None),
            "s_bfe_u32" => (format!("sbfe_u({a}, {b})"), Some(format!("{t} != 0u"))),
            "s_bfe_i32" => (format!("(uint)sbfe_i({a}, {b})"), Some(format!("{t} != 0u"))),
            "s_min_u32" => (format!("min({a}, {b})"), Some(format!("{a} < {b}"))),
            "s_max_u32" => (format!("max({a}, {b})"), Some(format!("{a} > {b}"))),
            "s_min_i32" => (format!("(uint)min((int){a}, (int){b})"), Some(format!("(int){a} < (int){b}"))),
            "s_max_i32" => (format!("(uint)max((int){a}, (int){b})"), Some(format!("(int){a} > (int){b}"))),
            "s_cselect_b32" => (format!("(scc != 0u) ? {a} : {b}"), None),
            "s_wqm_b32" => (format!("wqm({a})"), Some(format!("{t} != 0u"))),
            "s_bitset0_b32" | "s_bitset1_b32" => {
                let cur = match &dst {
                    Dst::S(n) => self.sreg(*n),
                    _ => "0u".into(),
                };
                if name == "s_bitset1_b32" {
                    (format!("{cur} | (1u << ({a} & 31u))"), None)
                } else {
                    (format!("{cur} & ~(1u << ({a} & 31u))"), None)
                }
            }
            "s_absdiff_i32" => (format!("(uint)abs((int){a} - (int){b})"), Some(format!("{t} != 0u"))),
            "s_pack_ll_b32_b16" => (format!("({a} & 0xFFFFu) | ({b} << 16)"), None),
            "s_pack_lh_b32_b16" => (format!("({a} & 0xFFFFu) | ({b} & 0xFFFF0000u)"), None),
            "s_pack_hh_b32_b16" => (format!("({a} >> 16) | ({b} & 0xFFFF0000u)"), None),
            _ => return Err("SALU"),
        };
        self.line(&format!("uint {t} = {res};"));
        if let Some(c) = scc {
            self.line(&format!("scc = ({c}) ? 1u : 0u;"));
        }
        self.write32(&dst, &t);
        Ok(())
    }

    fn salu64(&mut self, d: &Decoded, name: &str, dst: &Dst, ins: &[&Operand]) -> Result<(), &'static str> {
        let a = match ins.first() {
            Some(o) => Some(self.read64(d, o).ok_or("op")?),
            None => None,
        };
        let b = match ins.get(1) {
            Some(o) => {
                // shift amounts / bfe controls are 32-bit
                if o.size == 32 {
                    let v = self.read(d, o, false).ok_or("op")?;
                    Some((v, "0u".into()))
                } else {
                    Some(self.read64(d, o).ok_or("op")?)
                }
            }
            None => None,
        };
        let (al, ah) = a.clone().unwrap_or_default();
        let (bl, bh) = b.clone().unwrap_or_default();
        let (t0, t1) = (self.t(), self.t());
        let nz = format!("({t0} | {t1}) != 0u");
        let bitwise = |op: &str| -> Option<(String, String)> {
            Some(match op {
                "and" => (format!("{al} & {bl}"), format!("{ah} & {bh}")),
                "or" => (format!("{al} | {bl}"), format!("{ah} | {bh}")),
                "xor" => (format!("{al} ^ {bl}"), format!("{ah} ^ {bh}")),
                "andn2" => (format!("{al} & ~{bl}"), format!("{ah} & ~{bh}")),
                "orn2" => (format!("{al} | ~{bl}"), format!("{ah} | ~{bh}")),
                "nand" => (format!("~({al} & {bl})"), format!("~({ah} & {bh})")),
                "nor" => (format!("~({al} | {bl})"), format!("~({ah} | {bh})")),
                "xnor" => (format!("~({al} ^ {bl})"), format!("~({ah} ^ {bh})")),
                _ => return None,
            })
        };
        let (lo, hi, scc): (String, String, Option<String>) = match name {
            "s_mov_b64" => (al.clone(), ah.clone(), None),
            "s_cmov_b64" => {
                let cur = match dst {
                    Dst::S(n) => (self.sreg(*n), self.sreg(*n + 1)),
                    _ => ("0u".into(), "0u".into()),
                };
                (format!("(scc != 0u) ? {al} : {}", cur.0), format!("(scc != 0u) ? {ah} : {}", cur.1), None)
            }
            "s_not_b64" => (format!("~{al}"), format!("~{ah}"), Some(nz.clone())),
            "s_cselect_b64" => (format!("(scc != 0u) ? {al} : {bl}"), format!("(scc != 0u) ? {ah} : {bh}"), None),
            "s_wqm_b64" => (format!("wqm({al})"), format!("wqm({ah})"), Some(nz.clone())),
            "s_brev_b64" => (format!("reversebits({ah})"), format!("reversebits({al})"), None),
            "s_bcnt1_i32_b64" => (format!("countbits({al}) + countbits({ah})"), "0u".into(), Some(format!("{t0} != 0u"))),
            "s_ff1_i32_b64" => {
                (format!("({al} != 0u) ? firstbitlow({al}) : (({ah} != 0u) ? 32u + firstbitlow({ah}) : 0xFFFFFFFFu)"), "0u".into(), None)
            }
            "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" => {
                let v = format!("(((uint64_t){ah} << 32) | (uint64_t){al})");
                let e = match name {
                    "s_lshl_b64" => format!("({v} << ({bl} & 63u))"),
                    "s_lshr_b64" => format!("({v} >> ({bl} & 63u))"),
                    _ => format!("(uint64_t)((int64_t){v} >> ({bl} & 63u))"),
                };
                let w = self.t();
                self.line(&format!("uint64_t {w} = {e};"));
                (format!("(uint){w}"), format!("(uint)({w} >> 32)"), Some(nz.clone()))
            }
            "s_bfm_b64" => {
                let w = self.t();
                self.line(&format!(
                    "uint64_t {w} = ((({al} & 63u) == 0u) ? 0ull : (0xFFFFFFFFFFFFFFFFull >> (64u - ({al} & 63u)))) << ({bl} & 63u);"
                ));
                (format!("(uint){w}"), format!("(uint)({w} >> 32)"), None)
            }
            _ => {
                let op = name.trim_start_matches("s_").trim_end_matches("_b64");
                match bitwise(op) {
                    Some((l, h)) => (l, h, Some(nz.clone())),
                    None => return Err("SALU64"),
                }
            }
        };
        self.line(&format!("uint {t0} = {lo}; uint {t1} = {hi};"));
        if let Some(c) = scc {
            self.line(&format!("scc = ({c}) ? 1u : 0u;"));
        }
        if name == "s_bcnt1_i32_b64" || name == "s_ff1_i32_b64" {
            self.write32(dst, &t0);
        } else {
            self.write64(dst, &t0, &t1);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- memory (flat model)

    fn smem(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let count = match name.trim_start_matches("s_buffer_load_").trim_start_matches("s_load_") {
            "dword" => 1,
            "dwordx2" => 2,
            "dwordx4" => 4,
            "dwordx8" => 8,
            "dwordx16" => 16,
            _ => return Err("SMEM"),
        };
        let sdata = Self::field(d, "SDATA");
        let sbase = Self::field(d, "SBASE");
        let imm = rdna2::field_value(d.enc, &d.words, "OFFSET").unwrap_or(0) as u32;
        let soff = rdna2::field_value(d.enc, &d.words, "SOFFSET").unwrap_or(125);
        let soff = if soff == 125 { "0u".to_string() } else { self.code32(soff, None, false).ok_or("op")? };
        let t = self.t();
        let mut vals = Vec::new();
        if name.starts_with("s_buffer_load") {
            // V#: base = dw0 | dw1[15:0] << 32, stride = dw1[29:16], num_records = dw2
            let (d0, d1, d2) = (self.sreg(sbase), self.sreg(sbase + 1), self.sreg(sbase + 2));
            self.line(&format!("uint {t}_off = {} + {soff};", hex(imm)));
            self.line(&format!("uint {t}_lim = ((({d1} >> 16) & 0x3FFFu) == 0u) ? {d2} : {d2} * (({d1} >> 16) & 0x3FFFu);"));
            for i in 0..count {
                let v = format!("{t}_{i}");
                self.line(&format!("uint {v} = ({t}_off + {}u < {t}_lim) ? mem.Load(({d0} + {t}_off + {}u) & ~3u) : 0u;", 4 * i, 4 * i));
                vals.push(v);
            }
        } else {
            let imm = ((imm << 11) as i32 >> 11) as u32; // 21-bit signed
            let b = self.sreg(sbase);
            self.line(&format!("uint {t}_a = {b} + {} + {soff};", hex(imm)));
            for i in 0..count {
                let v = format!("{t}_{i}");
                self.line(&format!("uint {v} = mem.Load(({t}_a + {}u) & ~3u);", 4 * i));
                vals.push(v);
            }
        }
        for (i, v) in vals.iter().enumerate() {
            let s = self.sreg(sdata + i as u64);
            self.line(&format!("{s} = {v};"));
        }
        Ok(())
    }

    /// MUBUF / MTBUF: raw loads and stores, typed (format) loads.
    fn mubuf(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        if name.starts_with("buffer_atomic_") {
            return self.buffer_atomic(d, name);
        }
        // sub-dword accesses: (bytes, signed load?, store?)
        let small = match name {
            "buffer_load_ubyte" => Some((1u32, false, false)),
            "buffer_load_sbyte" => Some((1, true, false)),
            "buffer_load_ushort" => Some((2, false, false)),
            "buffer_load_sshort" => Some((2, true, false)),
            "buffer_store_byte" => Some((1, false, true)),
            "buffer_store_short" => Some((2, false, true)),
            _ => None,
        };
        let (kind, count) = match small {
            Some(_) => (BufKind::Load, 1),
            None => buffer_op(name).ok_or("MUBUF")?,
        };
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        if f("LDS") != 0 || f("TFE") != 0 {
            return Err("MUBUF lds/tfe");
        }
        let (vdata, vaddr, srsrc) = (f("VDATA"), f("VADDR"), Self::field(d, "SRSRC"));
        let soff = self.code32(f("SOFFSET"), None, false).ok_or("op")?;
        let (offen, idxen) = (f("OFFEN") != 0, f("IDXEN") != 0);
        let (d0, d1, d2, d3) = (self.sreg(srsrc), self.sreg(srsrc + 1), self.sreg(srsrc + 2), self.sreg(srsrc + 3));
        let index = if idxen { self.vreg(vaddr) } else { "0u".into() };
        let voff = if offen { self.vreg(vaddr + idxen as u64) } else { "0u".into() };
        let t = self.t();
        self.line(&format!("uint {t}_stride = ({d1} >> 16) & 0x3FFFu;"));
        self.line(&format!("uint {t}_off = {voff} + {}u;", f("OFFSET")));
        self.line(&format!("uint {t}_a = {d0} + {soff} + {t}_off + {t}_stride * {index};"));
        let act = self.act();
        let in_range = |bytes: String| format!("(({t}_stride == 0u) ? ({t}_off + {bytes} <= {d2}) : ({index} < {d2}))");
        if let Some((n, signed, store)) = small {
            let ok = in_range(format!("{n}u"));
            let v = self.vreg(vdata);
            if store {
                self.line(&format!("if ({act} && {ok}) st_bytes({t}_a, uint4({v}, 0u, 0u, 0u), {n}u);"));
            } else {
                let bits = 8 * n;
                let val = if signed {
                    format!("(uint)(((int)(ld_raw({t}_a).x << {}u)) >> {}u)", 32 - bits, 32 - bits)
                } else {
                    format!("(ld_raw({t}_a).x & {}u)", (1u64 << bits) - 1)
                };
                self.line(&format!("if ({act}) {v} = {ok} ? {val} : 0u;"));
            }
            return Ok(());
        }
        match kind {
            BufKind::Load => {
                let mut assigns = Vec::new();
                for i in 0..count {
                    let ok = in_range(format!("{}u", 4 * i + 4));
                    let v = self.vreg(vdata + i);
                    assigns.push(format!("{v} = {ok} ? mem.Load(({t}_a + {}u) & ~3u) : 0u;", 4 * i));
                }
                self.line(&format!("if ({act}) {{ {} }}", assigns.join(" ")));
            }
            BufKind::Store => {
                let mut st = Vec::new();
                for i in 0..count {
                    let ok = in_range(format!("{}u", 4 * i + 4));
                    let v = self.vreg(vdata + i);
                    st.push(format!("if ({ok}) mem.Store(({t}_a + {}u) & ~3u, {v});", 4 * i));
                }
                self.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
            }
            BufKind::FormatLoad => {
                // format from the instruction (tbuffer) or from the V# (dword3 [18:12]); DST_SEL from the V#
                let fmt = if d.enc.name == "ENC_MTBUF" { format!("{}u", f("FORMAT")) } else { format!("(({d3} >> 12) & 0x7Fu)") };
                self.line(&format!("uint {t}_info = FMT_INFO[min({fmt}, 77u)];"));
                self.line(&format!("bool {t}_int = ((({t}_info >> 3) & 7u) == 4u) || ((({t}_info >> 3) & 7u) == 5u);"));
                self.line(&format!("bool {t}_ok = ({t}_info & 7u) != 0u && {};", in_range(format!("fmt_bytes({t}_info)"))));
                self.line(&format!("uint4 {t}_r = ld_raw({t}_a);"));
                self.line(&format!(
                    "uint4 {t}_c = uint4(fmt_comp({t}_r, {t}_info, 0u), fmt_comp({t}_r, {t}_info, 1u), fmt_comp({t}_r, {t}_info, 2u), fmt_comp({t}_r, {t}_info, 3u));"
                ));
                let mut assigns = Vec::new();
                for i in 0..count {
                    let v = self.vreg(vdata + i);
                    assigns.push(format!("{v} = {t}_ok ? dst_sel({t}_c, ({d3} >> {}u) & 7u, {t}_int) : 0u;", 3 * i));
                }
                self.line(&format!("if ({act}) {{ {} }}", assigns.join(" ")));
            }
            BufKind::FormatStore => {
                let fmt = if d.enc.name == "ENC_MTBUF" { format!("{}u", f("FORMAT")) } else { format!("(({d3} >> 12) & 0x7Fu)") };
                self.line(&format!("uint {t}_info = FMT_INFO[min({fmt}, 77u)];"));
                self.line(&format!("bool {t}_ok = ({t}_info & 7u) != 0u && {};", in_range(format!("fmt_bytes({t}_info)"))));
                let vals: Vec<String> = (0..4).map(|i| if i < count { self.vreg(vdata + i) } else { "0u".into() }).collect();
                self.line(&format!("if ({act} && {t}_ok) st_bytes({t}_a, fmt_pack(uint4({}), {t}_info), fmt_bytes({t}_info));", vals.join(", ")));
            }
        }
        Ok(())
    }

    fn export(&mut self, d: &Decoded) -> Result<(), &'static str> {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let (en, tgt, compr) = (f("EN"), f("TGT"), f("COMPR") != 0);
        let srcs: Vec<String> = (0..4).map(|i| self.vreg(f(&format!("VSRC{i}")))).collect();
        if !matches!(self.opt.mode, Mode::Kernel) {
            return self.export_graphics(d, en, tgt, compr, &srcs);
        }
        let act = self.act();
        let mut st = Vec::new();
        for c in 0..4u64 {
            let val = if compr {
                let r = &srcs[(c / 2) as usize];
                if c % 2 == 0 {
                    format!("({r} & 0xFFFFu)")
                } else {
                    format!("({r} >> 16)")
                }
            } else {
                srcs[c as usize].clone()
            };
            let bit = if compr { 1 << (c / 2 * 2) | 2 << (c / 2 * 2) } else { 1 << c };
            if en & bit == 0 {
                continue;
            }
            st.push(format!("res.Store(({}u + (({tgt}u * {}u + xcl_hw_lane) * 4u + {c}u)) * 4u, {val});", KERNEL_EXPORTS, KERNEL_LANES));
        }
        if !st.is_empty() {
            self.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
        }
        Ok(())
    }

    /// Raw component `c` of an export (compressed: 16-bit halves of two registers).
    fn exp_comp(srcs: &[String], compr: bool, c: usize) -> String {
        if compr {
            let r = &srcs[c / 2];
            if c % 2 == 0 {
                format!("({r} & 0xFFFFu)")
            } else {
                format!("({r} >> 16)")
            }
        } else {
            srcs[c].clone()
        }
    }

    fn export_graphics(&mut self, d: &Decoded, en: u64, tgt: u64, compr: bool, srcs: &[String]) -> Result<(), &'static str> {
        let act = self.act();
        let done = rdna2::field_value(d.enc, &d.words, "DONE") == Some(1);
        let enabled = |c: usize| if compr { en >> (c / 2 * 2) & 3 != 0 } else { en >> c & 1 == 1 };
        let mut st = Vec::new();
        match (&self.opt.mode, tgt) {
            (Mode::Vertex(_), 12) => {
                for c in 0..4 {
                    if enabled(c) {
                        st.push(format!("xo.pos[{c}] = asfloat({});", Self::exp_comp(srcs, compr, c)));
                    }
                }
            }
            (Mode::Vertex(_), 13..=15) => {} // misc vector / clip distances: SV_ClipDistance comes from its param
            (Mode::Vertex(vs), 32..=63) => {
                let k = (tgt - 32) as usize;
                let params = ranked(&vs.outputs, true);
                let Some(e) = params.get(k) else {
                    self.line("/* param bez elementu OSG1 (atrapa) */");
                    return Ok(());
                };
                let n = out_comps(e);
                for c in 0..n {
                    if enabled(c) {
                        let v = Self::exp_comp(srcs, compr, c);
                        let v = if e.component_type == 3 { format!("asfloat({v})") } else { format!("{v}") };
                        let comp = if n == 1 { String::new() } else { format!("[{c}]") };
                        st.push(format!("xo.p{k}{comp} = {v};"));
                    }
                }
            }
            (Mode::Pixel(ps), 0..=7) => {
                let Some(e) = ps.outputs.iter().find(|e| e.system_value == 64 && e.index as u64 == tgt) else {
                    self.line("/* eksport do nieopisanego SV_Target */");
                    if done {
                        self.ps_done = true;
                        self.line(&format!("if ({act}) xcl_live = true;"));
                    }
                    return Ok(());
                };
                for c in 0..4 {
                    if !enabled(c) {
                        continue;
                    }
                    let raw = Self::exp_comp(srcs, compr, c);
                    let v = match (e.component_type, compr) {
                        (3, true) => format!("f16tof32({raw})"),
                        (3, false) => format!("asfloat({raw})"),
                        (2, true) => format!("(((int)({raw} << 16)) >> 16)"),
                        (2, false) => format!("(int){raw}"),
                        (_, _) => raw,
                    };
                    st.push(format!("xo.t{tgt}[{c}] = {v};"));
                }
            }
            (Mode::Pixel(ps), 8) => {
                if en & 1 == 1 && ps.outputs.iter().any(|e| matches!(e.system_value, 65 | 67 | 68)) {
                    st.push(format!("xo.xcl_depth = asfloat({});", srcs[0]));
                }
            }
            (Mode::Pixel(_), 9) => {}
            (Mode::Mesh(_), 12) => {
                for c in 0..4 {
                    if enabled(c) {
                        st.push(format!("res.Store((xcl_vtx + {c}u) * 4u, {});", Self::exp_comp(srcs, compr, c)));
                    }
                }
            }
            (Mode::Mesh(_), 13..=15) => {} // misc vector / clip distances (from their params)
            (Mode::Mesh(ms), 32..=63) => {
                let k = (tgt - 32) as usize;
                if k >= ranked(&ms.outputs, true).len() {
                    self.line("/* param bez elementu OSG1 (atrapa) */");
                    return Ok(());
                }
                for c in 0..4 {
                    if enabled(c) {
                        st.push(format!("res.Store((xcl_vtx + {}u) * 4u, {});", 4 * (k + 1) + c, Self::exp_comp(srcs, compr, c)));
                    }
                }
            }
            (Mode::Mesh(_), 20) => st.push(format!("res.Store(xcl_prm * 4u, {});", srcs[0])),
            (Mode::Mesh(_), 9) => {}
            _ => return Err("cel eksportu"),
        }
        if !st.is_empty() {
            self.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
        }
        if done && matches!(self.opt.mode, Mode::Pixel(_)) {
            self.ps_done = true;
            self.line(&format!("if ({act}) xcl_live = true;"));
        }
        Ok(())
    }

    /// (vdst, I/J VGPR or interp_mov parameter, attribute, channel)
    fn interp_fields(d: &Decoded) -> (u64, u64, u64, u64) {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        if d.enc.name == "ENC_VINTRP" {
            (f("VDST"), f("VSRC"), f("ATTR"), f("ATTRCHAN"))
        } else {
            let s1 = f("SRC1");
            (f("VDST"), if s1 >= 256 { s1 - 256 } else { s1 }, f("SRC0") & 0x3F, f("SRC0") >> 6 & 3)
        }
    }

    /// v_interp_*: attributes come interpolated from the PC rasteriser (PS mode).
    fn interp(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let Mode::Pixel(ps) = &self.opt.mode else { return Err("interpolacja poza PS") };
        let n_attrs = ranked(&ps.inputs, false).len() as u64;
        let (vdst, ij, attr, chan) = Self::interp_fields(d);
        if attr >= n_attrs {
            return Err("atrybut poza ISG1");
        }
        let src = format!("xi.a{attr}[{chan}]");
        let act = self.act();
        match name {
            "v_interp_p1_f32" | "v_interp_p1ll_f16" | "v_interp_p1lv_f16" => {
                self.line("/* pierwsza połowa interpolacji – wartość daje p2 */");
            }
            "v_interp_p2_f32" => {
                let v = self.vreg(vdst);
                self.line(&format!("if ({act}) {v} = asuint((float){src});"));
            }
            "v_interp_mov_f32" => {
                if ij != 2 {
                    return Err("v_interp_mov p10/p20");
                }
                let v = self.vreg(vdst);
                self.line(&format!("if ({act}) {v} = asuint((float){src});"));
            }
            "v_interp_p2_f16" => {
                let v = self.vreg(vdst);
                let hi = rdna2::field_value(d.enc, &d.words, "OP_SEL").unwrap_or(0) >> 3 & 1 == 1;
                let h = format!("(f32tof16((float){src}) & 0xFFFFu)");
                let e = if hi { format!("({v} & 0xFFFFu) | ({h} << 16)") } else { format!("({v} & 0xFFFF0000u) | {h}") };
                self.line(&format!("if ({act}) {v} = {e};"));
            }
            _ => return Err("interpolacja"),
        }
        Ok(())
    }

    // ---------------------------------------------------------------- images (MIMG)

    /// MIMG → SM 6.6 dynamic resources. XCL image descriptors (T#, 8 dwords) keep the GFX10
    /// layout, but BASE_ADDRESS (word 0) holds the index of the SRV in the CBV/SRV/UAV heap
    /// (UAV at +1); FORMAT (word 1 [28:20]) selects the typed view (float/uint/sint).
    /// Sampler descriptors (S#, 4 dwords) hold the sampler heap index in BORDER_COLOR_PTR
    /// (word 3 [11:0]). Address layout with A16 (16-bit bias/coordinates/lod, 32-bit offset,
    /// compare and gradients) as in AnyPS5's `componentShape` (GPL-2.0, ported 2026-11-20).
    fn mimg(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        if f("TFE") != 0 || f("LWE") != 0 {
            return Err("MIMG tfe/lwe");
        }
        let (a16, d16) = (f("A16") == 1, f("D16") == 1);
        let dim = f("DIM");
        let dmask = f("DMASK");
        let (vdata, srsrc, ssamp) = (f("VDATA"), Self::field(d, "SRSRC"), Self::field(d, "SSAMP"));
        let nsa = f("NSA");
        // non-sequential addresses: VADDR, then VADDRA, VADDRB … (4 per extra dword)
        let nsa_enc = rdna2::encoding(&format!("MIMG_NSA{nsa}"));
        let addr_reg = |j: u64| -> u64 {
            match nsa_enc {
                Some(e) if nsa > 0 && j > 0 => {
                    let letter = (b'A' + (j - 1).min(25) as u8) as char;
                    rdna2::field_value(e, &d.words, &format!("VADDR{letter}")).unwrap_or(0)
                }
                _ => f("VADDR") + j,
            }
        };
        let base = name.trim_start_matches("image_");
        if base == "bvh_intersect_ray" || base == "bvh64_intersect_ray" {
            // ray tracing: one BVH node per instruction (public RDNA2 / RADV node format)
            let r = |s: &mut Self, j: u64| s.vreg(addr_reg(j));
            let wide = base == "bvh64_intersect_ray";
            let node = r(self, 0);
            let k = if wide { 2 } else { 1 };
            let fl = |s: &mut Self, j: u64| format!("asfloat({})", r(s, j));
            let extent = fl(self, k);
            let origin = format!("float3({}, {}, {})", fl(self, k + 1), fl(self, k + 2), fl(self, k + 3));
            let (dir, inv) = if a16 {
                // A16: dir.xy | dir.z inv.x | inv.yz as half pairs
                let (h0, h1, h2) = (r(self, k + 4), r(self, k + 5), r(self, k + 6));
                (
                    format!("float3(f16tof32({h0}), f16tof32({h0} >> 16), f16tof32({h1}))"),
                    format!("float3(f16tof32({h1} >> 16), f16tof32({h2}), f16tof32({h2} >> 16))"),
                )
            } else {
                (
                    format!("float3({}, {}, {})", fl(self, k + 4), fl(self, k + 5), fl(self, k + 6)),
                    format!("float3({}, {}, {})", fl(self, k + 7), fl(self, k + 8), fl(self, k + 9)),
                )
            };
            let (d0, d1) = (self.sreg(srsrc), self.sreg(srsrc + 1));
            let t = self.t();
            let act = self.act();
            // descriptor: base address in 256-byte units (word 0), box sorting = word 1 bit 31
            self.line(&format!("uint4 {t} = bvh_intersect({node}, {d0} << 8, {extent}, {origin}, {dir}, {inv}, ({d1} >> 31) != 0u);"));
            let st: Vec<String> = (0..4).map(|c| format!("{} = {t}.{};", self.vreg(vdata + c), ["x", "y", "z", "w"][c as usize])).collect();
            self.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
            return Ok(());
        }
        // body coordinates per DIM: 1D, 2D, 3D, CUBE, 1D_ARRAY, 2D_ARRAY, 2D_MSAA, 2D_MSAA_ARRAY
        let body = [1u64, 2, 3, 3, 2, 3, 3, 4][dim as usize];
        let sampling = base.starts_with("sample") || base.starts_with("gather4");
        let rest = base.trim_start_matches("sample").trim_start_matches("gather4");
        let mods: Vec<&str> = if sampling { rest.split('_').filter(|x| !x.is_empty()).collect() } else { Vec::new() };
        if mods.contains(&"cd") {
            return Err("MIMG cd");
        }
        let (has_cl, has_o) = (mods.contains(&"cl"), mods.contains(&"o"));
        let (has_b, has_c, has_d, has_l, has_lz) =
            (mods.contains(&"b"), mods.contains(&"c"), mods.contains(&"d"), mods.contains(&"l"), mods.contains(&"lz"));
        let grad_n = if has_d { [1u64, 2, 3, 2, 1, 2, 2, 2][dim as usize] } else { 0 };
        // address components: (16-bit?, starts a dword?)
        let h = if a16 { 16 } else { 32 };
        let mut shape: Vec<(u32, bool)> = Vec::new();
        let (mut i_bias, mut i_cmp, mut i_grad, mut i_body, mut i_extra, mut i_clamp) = (None, None, 0usize, 0usize, None, None);
        if sampling {
            // address order: offset, bias, z-compare, derivatives, body, lod / clamp
            if has_o {
                shape.push((32, true));
            }
            if has_b {
                i_bias = Some(shape.len());
                shape.push((h, !a16));
            }
            if has_c {
                i_cmp = Some(shape.len());
                shape.push((32, true));
            }
            i_grad = shape.len();
            for _ in 0..2 * grad_n {
                shape.push((32, true));
            }
            i_body = shape.len();
            for k in 0..body {
                shape.push((h, !a16 || (grad_n != 0 && k == 0)));
            }
            if has_l {
                i_extra = Some(shape.len());
                shape.push((h, !a16));
            }
            if has_cl {
                i_clamp = Some(shape.len());
                shape.push((h, !a16));
            }
        } else if base == "get_resinfo" {
            i_extra = Some(0);
            shape.push((32, true));
        } else {
            i_body = 0;
            for _ in 0..body {
                shape.push((h, !a16));
            }
            if base.ends_with("_mip") {
                i_extra = Some(shape.len());
                shape.push((h, !a16));
            }
        }
        let mut pos: Vec<(u64, Option<u64>)> = Vec::new();
        let mut cursor = 0u64; // in 16-bit units
        for &(w, starts) in &shape {
            if (w == 32 || starts) && cursor % 2 == 1 {
                cursor += 1;
            }
            pos.push((cursor / 2, (w == 16).then_some(cursor % 2)));
            cursor += if w == 32 { 2 } else { 1 };
        }
        let compf = |s: &mut Self, i: usize| -> String {
            let (dw, half) = pos[i];
            let v = s.vreg(addr_reg(dw));
            match half {
                None => format!("asfloat({v})"),
                Some(hh) => format!("f16tof32(({v} >> {}u) & 0xFFFFu)", 16 * hh),
            }
        };
        let compu = |s: &mut Self, i: usize| -> String {
            let (dw, half) = pos[i];
            let v = s.vreg(addr_reg(dw));
            match half {
                None => v,
                Some(hh) => format!("(({v} >> {}u) & 0xFFFFu)", 16 * hh),
            }
        };
        let t = self.t();
        let tw0 = self.sreg(srsrc);
        let tw1 = self.sreg(srsrc + 1);
        let act = self.act();
        let ps = matches!(self.opt.mode, Mode::Pixel(_));
        let comps: Vec<u64> = (0..4).filter(|c| dmask >> c & 1 == 1).collect();
        // write the selected result components (`r` is a uint4 of raw bits); D16 packs halves
        let write_result = |s: &mut Self, r: &str, n: usize, sel: &dyn Fn(usize) -> u64, half: &dyn Fn(&str) -> String| {
            let mut st = Vec::new();
            if d16 {
                for k in (0..n).step_by(2) {
                    let v = s.vreg(vdata + k as u64 / 2);
                    let lo = half(&format!("{r}.{}", ["x", "y", "z", "w"][sel(k) as usize]));
                    let hi = if k + 1 < n { half(&format!("{r}.{}", ["x", "y", "z", "w"][sel(k + 1) as usize])) } else { "0u".into() };
                    st.push(format!("{v} = ({lo} & 0xFFFFu) | ({hi} << 16);"));
                }
            } else {
                for k in 0..n {
                    let v = s.vreg(vdata + k as u64);
                    st.push(format!("{v} = {r}.{};", ["x", "y", "z", "w"][sel(k) as usize]));
                }
            }
            s.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
        };
        let f16 = |e: &str| format!("f32tof16(asfloat({e}))");

        if sampling {
            if matches!(dim, 6 | 7) {
                return Err("MIMG sample MSAA");
            }
            let bias = i_bias.map(|i| compf(self, i));
            let cmp = i_cmp.map(|i| compf(self, i));
            let grads = if has_d {
                let dx: Vec<String> = (0..grad_n as usize).map(|k| compf(self, i_grad + k)).collect();
                let dy: Vec<String> = (0..grad_n as usize).map(|k| compf(self, i_grad + grad_n as usize + k)).collect();
                Some((dx, dy))
            } else {
                None
            };
            let coords: Vec<String> = (0..body as usize).map(|k| compf(self, i_body + k)).collect();
            let lod = i_extra.map(|i| compf(self, i));
            let cl = i_clamp.map(|i| compf(self, i));
            // the clamp overloads take an offset first (none for cubes)
            let zoff = match dim {
                0 | 4 => Some("0"),
                1 | 5 => Some("int2(0, 0)"),
                2 => Some("int3(0, 0, 0)"),
                _ => None,
            };
            let tail = |args: String| -> String {
                match (&cl, zoff) {
                    (Some(c), Some(z)) => format!("{args}, {z}, {c}"),
                    (Some(c), None) => format!("{args}, {c}"),
                    _ => args,
                }
            };
            let s3 = self.sreg(ssamp + 3);
            // texel offsets come from a VGPR (6-bit fields); SM 6.6 takes only immediate offsets, so
            // the coordinates move by offset / size of the sampled mip (exact for lz / l / gather,
            // the rounded implicit LOD otherwise)
            let offset = if has_o {
                if dim == 3 {
                    return Err("MIMG offset cube");
                }
                let o = compu(self, 0);
                Some(o)
            } else {
                None
            };
            let coord0 = match dim {
                0 => coords[0].clone(),
                1 => format!("float2({}, {})", coords[0], coords[1]),
                2 => format!("float3({}, {}, {})", coords[0], coords[1], coords[2]),
                4 => format!("float2({}, round({}))", coords[0], coords[1]),
                5 => format!("float3({}, {}, round({}))", coords[0], coords[1], coords[2]),
                _ => String::new(),
            };
            let coords: Vec<String> = if offset.is_some() {
                coords.iter().enumerate().map(|(k, c)| if (dim == 4 && k == 1) || (dim == 5 && k == 2) { c.clone() } else { format!("({c} + {t}_o[{k}])") }).collect()
            } else {
                coords
            };
            let (ty, coord) = match dim {
                0 => ("Texture1D<float4>", coords[0].clone()),
                1 => ("Texture2D<float4>", format!("float2({}, {})", coords[0], coords[1])),
                2 => ("Texture3D<float4>", format!("float3({}, {}, {})", coords[0], coords[1], coords[2])),
                3 => ("TextureCube<float4>", format!("cube_dir({}, {}, {})", coords[0], coords[1], coords[2])),
                4 => ("Texture1DArray<float4>", format!("float2({}, round({}))", coords[0], coords[1])),
                5 => ("Texture2DArray<float4>", format!("float3({}, {}, round({}))", coords[0], coords[1], coords[2])),
                _ => return Err("MIMG dim"),
            };
            if f("UNORM") != 0 && dim != 3 {
                return Err("MIMG unorm");
            }
            let samp = if has_c { "SamplerComparisonState" } else { "SamplerState" };
            self.line(&format!("{ty} {t}_T = ResourceDescriptorHeap[NonUniformResourceIndex({tw0})];"));
            self.line(&format!("{samp} {t}_S = SamplerDescriptorHeap[NonUniformResourceIndex({s3} & 0xFFFu)];"));
            if let Some(o) = &offset {
                let level = match (&lod, has_lz || base.starts_with("gather4"), ps && grads.is_none() && !has_c) {
                    (_, true, _) => "0u".to_string(),
                    (Some(l), _, _) => format!("(uint)max(round({l}), 0.0f)"),
                    (None, _, true) => format!("(uint)max(round({t}_T.CalculateLevelOfDetail({t}_S, {coord0})), 0.0f)"),
                    _ => "0u".to_string(),
                };
                let dims = match dim {
                    0 => format!("{t}_T.GetDimensions({level}, {t}_w, {t}_n);"),
                    1 => format!("{t}_T.GetDimensions({level}, {t}_w, {t}_h, {t}_n);"),
                    2 => format!("{t}_T.GetDimensions({level}, {t}_w, {t}_h, {t}_d, {t}_n);"),
                    4 => format!("{t}_T.GetDimensions({level}, {t}_w, {t}_d, {t}_n); {t}_d = 1u;"),
                    _ => format!("{t}_T.GetDimensions({level}, {t}_w, {t}_h, {t}_d, {t}_n); {t}_d = 1u;"),
                };
                self.line(&format!("uint {t}_w = 1u, {t}_h = 1u, {t}_d = 1u, {t}_n; {dims}"));
                self.line(&format!(
                    "float3 {t}_o = float3((float)(((int)({o} << 26)) >> 26) / (float){t}_w, (float)(((int)({o} << 18)) >> 26) / (float){t}_h, (float)(((int)({o} << 10)) >> 26) / (float){t}_d);"
                ));
            }
            let gather = base.starts_with("gather4");
            let call = if gather {
                if comps.len() != 1 {
                    return Err("gather4 dmask");
                }
                let ch = ["Red", "Green", "Blue", "Alpha"][comps[0] as usize];
                match &cmp {
                    Some(z) => format!("{t}_T.GatherCmp{ch}({t}_S, {coord}, {z})"),
                    None => format!("{t}_T.Gather{ch}({t}_S, {coord})"),
                }
            } else {
                // outside PS there are no implicit derivatives: LOD 0 (bias = LOD), raised by the clamp
                let floor = |l: String| match &cl {
                    Some(c) => format!("max({l}, {c})"),
                    None => l,
                };
                match (&cmp, &grads, &lod, &bias) {
                    (Some(z), _, _, _) if has_lz || !ps => format!("float4({t}_T.SampleCmpLevelZero({t}_S, {coord}, {z}).xxxx)"),
                    (Some(z), _, _, _) => format!("float4({t}_T.SampleCmp({t}_S, {}).xxxx)", tail(format!("{coord}, {z}"))),
                    (None, Some((dx, dy)), _, _) => {
                        let v = |x: &[String]| if x.len() == 1 { x[0].clone() } else { format!("float{}({})", x.len(), x.join(", ")) };
                        format!("{t}_T.SampleGrad({t}_S, {})", tail(format!("{coord}, {}, {}", v(dx), v(dy))))
                    }
                    (None, None, Some(l), _) => format!("{t}_T.SampleLevel({t}_S, {coord}, {l})"),
                    (None, None, None, Some(b)) if ps => format!("{t}_T.SampleBias({t}_S, {})", tail(format!("{coord}, {b}"))),
                    (None, None, None, Some(b)) => format!("{t}_T.SampleLevel({t}_S, {coord}, {})", floor(b.clone())),
                    _ if has_lz || !ps => format!("{t}_T.SampleLevel({t}_S, {coord}, {})", floor("0.0f".into())),
                    _ => format!("{t}_T.Sample({t}_S, {})", tail(coord.clone())),
                }
            };
            self.line(&format!("uint4 {t}_r = asuint({call});"));
            let n = if gather { 4 } else { comps.len() };
            let comps2 = comps.clone();
            let sel = move |k: usize| if gather { k as u64 } else { comps2[k] };
            write_result(self, &format!("{t}_r"), n, &sel, &f16);
            return Ok(());
        }

        if base == "get_lod" {
            if !ps {
                return Err("image_get_lod poza PS");
            }
            let coords: Vec<String> = (0..body as usize).map(|k| compf(self, i_body + k)).collect();
            let (ty, coord) = match dim {
                0 => ("Texture1D<float4>", coords[0].clone()),
                1 => ("Texture2D<float4>", format!("float2({}, {})", coords[0], coords[1])),
                2 => ("Texture3D<float4>", format!("float3({}, {}, {})", coords[0], coords[1], coords[2])),
                3 => ("TextureCube<float4>", format!("cube_dir({}, {}, {})", coords[0], coords[1], coords[2])),
                4 => ("Texture1DArray<float4>", format!("float2({}, round({}))", coords[0], coords[1])),
                5 => ("Texture2DArray<float4>", format!("float3({}, {}, round({}))", coords[0], coords[1], coords[2])),
                _ => return Err("MIMG dim"),
            };
            let s3 = self.sreg(ssamp + 3);
            self.line(&format!("{ty} {t}_T = ResourceDescriptorHeap[NonUniformResourceIndex({tw0})];"));
            self.line(&format!("SamplerState {t}_S = SamplerDescriptorHeap[NonUniformResourceIndex({s3} & 0xFFFu)];"));
            self.line(&format!(
                "uint4 {t}_r = asuint(float4({t}_T.CalculateLevelOfDetail({t}_S, {coord}), {t}_T.CalculateLevelOfDetailUnclamped({t}_S, {coord}), 0.0f, 0.0f));"
            ));
            let comps2 = comps.clone();
            write_result(self, &format!("{t}_r"), comps.len(), &move |k| comps2[k], &f16);
            return Ok(());
        }

        if base == "get_resinfo" {
            let mipv = compu(self, 0);
            let (ty, call) = match dim {
                0 => ("Texture1D", format!("{t}_T.GetDimensions({mipv}, {t}_w, {t}_n); {t}_h = 1u; {t}_d = 1u;")),
                1 => ("Texture2D", format!("{t}_T.GetDimensions({mipv}, {t}_w, {t}_h, {t}_n); {t}_d = 1u;")),
                2 => ("Texture3D", format!("{t}_T.GetDimensions({mipv}, {t}_w, {t}_h, {t}_d, {t}_n);")),
                3 => ("TextureCube", format!("{t}_T.GetDimensions({mipv}, {t}_w, {t}_h, {t}_n); {t}_d = 1u;")),
                4 => ("Texture1DArray", format!("{t}_T.GetDimensions({mipv}, {t}_w, {t}_d, {t}_n); {t}_h = 1u;")),
                5 => ("Texture2DArray", format!("{t}_T.GetDimensions({mipv}, {t}_w, {t}_h, {t}_d, {t}_n);")),
                _ => return Err("get_resinfo dim"),
            };
            self.line(&format!("{ty}<float4> {t}_T = ResourceDescriptorHeap[NonUniformResourceIndex({tw0})];"));
            self.line(&format!("uint {t}_w, {t}_h, {t}_d, {t}_n; {call}"));
            self.line(&format!("uint4 {t}_r = uint4({t}_w, {t}_h, {t}_d, {t}_n);"));
            let comps2 = comps.clone();
            write_result(self, &format!("{t}_r"), comps.len(), &move |k| comps2[k], &|e: &str| e.to_string());
            return Ok(());
        }

        // integer coordinates for load / store / atomics
        let ic: Vec<String> = (0..body as usize).map(|k| compu(self, i_body + k)).collect();
        let mip = i_extra.map(|i| compu(self, i)).unwrap_or("0u".into());
        let (rty, icoord, load_arg) = match dim {
            0 => ("1D", format!("int({})", ic[0]), format!("int2({}, {mip})", ic[0])),
            1 => ("2D", format!("int2({}, {})", ic[0], ic[1]), format!("int3({}, {}, {mip})", ic[0], ic[1])),
            2 => ("3D", format!("int3({}, {}, {})", ic[0], ic[1], ic[2]), format!("int4({}, {}, {}, {mip})", ic[0], ic[1], ic[2])),
            4 => ("1DArray", format!("int2({}, {})", ic[0], ic[1]), format!("int3({}, {}, {mip})", ic[0], ic[1])),
            5 | 3 => ("2DArray", format!("int3({}, {}, {})", ic[0], ic[1], ic[2]), format!("int4({}, {}, {}, {mip})", ic[0], ic[1], ic[2])),
            6 => ("2DMS", format!("int2({}, {})", ic[0], ic[1]), String::new()),
            _ => return Err("MIMG dim"),
        };
        if dim == 3 && base.starts_with("load") {
            return Err("image_load cube");
        }
        self.line(&format!("uint {t}_cls = img_cls(({tw1} >> 20) & 0x1FFu);"));
        match base {
            "load" | "load_mip" => {
                self.line(&format!("uint4 {t}_r;"));
                let ld = if dim == 6 { format!("{t}_T.Load({icoord}, {})", ic.get(2).cloned().unwrap_or("0u".into())) } else { format!("{t}_T.Load({load_arg})") };
                let tex = |vt: &str| format!("Texture{rty}<{vt}4> {t}_T = ResourceDescriptorHeap[NonUniformResourceIndex({tw0})];");
                self.line(&format!(
                    "if ({t}_cls == 1u) {{ {} {t}_r = {ld}; }} else if ({t}_cls == 2u) {{ {} {t}_r = asuint({ld}); }} else {{ {} {t}_r = asuint({ld}); }}",
                    tex("uint"),
                    tex("int"),
                    tex("float")
                ));
                let cls = format!("{t}_cls");
                let half = move |e: &str| format!("(({cls} == 0u) ? f32tof16(asfloat({e})) : ({e} & 0xFFFFu))");
                let comps2 = comps.clone();
                write_result(self, &format!("{t}_r"), comps.len(), &move |k| comps2[k], &half);
            }
            "store" | "store_mip" => {
                if base == "store_mip" {
                    return Err("image_store_mip");
                }
                let mut vals = Vec::new();
                for c in 0..4u64 {
                    vals.push(match comps.iter().position(|&x| x == c) {
                        Some(k) if d16 => {
                            let v = self.vreg(vdata + k as u64 / 2);
                            let hpart = format!("(({v} >> {}u) & 0xFFFFu)", 16 * (k % 2));
                            format!("(({t}_cls == 0u) ? asuint(f16tof32({hpart})) : {hpart})")
                        }
                        Some(k) => self.vreg(vdata + k as u64),
                        None => "0u".into(),
                    });
                }
                let v4 = format!("uint4({})", vals.join(", "));
                let uav = |vt: &str| format!("RWTexture{rty}<{vt}4> {t}_U = ResourceDescriptorHeap[NonUniformResourceIndex({tw0} + 1u)];");
                self.line(&format!(
                    "if ({act}) {{ if ({t}_cls == 1u) {{ {} {t}_U[{icoord}] = {v4}; }} else if ({t}_cls == 2u) {{ {} {t}_U[{icoord}] = (int4){v4}; }} else {{ {} {t}_U[{icoord}] = asfloat({v4}); }} }}",
                    uav("uint"),
                    uav("int"),
                    uav("float")
                ));
            }
            _ if base.starts_with("atomic_") => {
                let op = match &base[7..] {
                    "add" => "InterlockedAdd",
                    "smin" | "umin" => "InterlockedMin",
                    "smax" | "umax" => "InterlockedMax",
                    "and" => "InterlockedAnd",
                    "or" => "InterlockedOr",
                    "xor" => "InterlockedXor",
                    "swap" => "InterlockedExchange",
                    _ => return Err("image_atomic"),
                };
                let signed = base.ends_with("smin") || base.ends_with("smax");
                let v = self.vreg(vdata);
                let vt = if signed { "int" } else { "uint" };
                let val = if signed { format!("(int){v}") } else { v.clone() };
                self.line(&format!("RWTexture{rty}<{vt}> {t}_U = ResourceDescriptorHeap[NonUniformResourceIndex({tw0} + 1u)];"));
                self.line(&format!("{vt} {t}_old = 0;"));
                self.line(&format!("if ({act}) {{ {op}({t}_U[{icoord}], {val}, {t}_old); }}"));
                if f("GLC") == 1 {
                    self.line(&format!("if ({act}) {v} = (uint){t}_old;"));
                }
            }
            _ => return Err("MIMG"),
        }
        Ok(())
    }

    // ---------------------------------------------------------------- vector ALU

    /// Source/output modifiers of a VALU instruction, wherever the encoding keeps them.
    fn mods(d: &Decoded) -> Mods {
        let fv = |e: &Encoding, n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
        let bit = |e: &Encoding, n: &str| fv(e, n) == 1;
        match d.variant {
            Variant::Sdwa => match ext_encoding(d) {
                Some(e) => Mods {
                    abs: [bit(e, "SRC0_ABS"), bit(e, "SRC1_ABS"), false],
                    neg: [bit(e, "SRC0_NEG"), bit(e, "SRC1_NEG"), false],
                    omod: fv(e, "OMOD"),
                    clamp: bit(e, "CLAMP"),
                    opsel: 0,
                },
                None => Mods::default(),
            },
            Variant::Dpp16 => match ext_encoding(d) {
                Some(e) => Mods {
                    abs: [bit(e, "SRC0_ABS"), bit(e, "SRC1_ABS"), false],
                    neg: [bit(e, "SRC0_NEG"), bit(e, "SRC1_NEG"), false],
                    ..Default::default()
                },
                None => Mods::default(),
            },
            Variant::Dpp8 => Mods::default(),
            Variant::Plain => {
                let e = d.enc;
                let (abs, neg) = (fv(e, "ABS"), fv(e, "NEG"));
                Mods {
                    abs: [abs & 1 == 1, abs >> 1 & 1 == 1, abs >> 2 & 1 == 1],
                    neg: [neg & 1 == 1, neg >> 1 & 1 == 1, neg >> 2 & 1 == 1],
                    omod: fv(e, "OMOD"),
                    clamp: bit(e, "CLAMP"),
                    opsel: fv(e, "OP_SEL"),
                }
            }
        }
    }

    /// Float view of source `i` with abs/neg.
    fn fsrc(m: &Mods, i: usize, u: &str) -> String {
        let mut s = format!("asfloat({u})");
        if m.abs[i] {
            s = format!("abs({s})");
        }
        if m.neg[i] {
            s = format!("(-{s})");
        }
        s
    }

    /// Float result with output modifiers, as u32.
    fn fres(m: &Mods, e: &str) -> String {
        let mut s = match m.omod {
            1 => format!("(({e}) * 2.0f)"),
            2 => format!("(({e}) * 4.0f)"),
            3 => format!("(({e}) * 0.5f)"),
            _ => format!("({e})"),
        };
        if m.clamp {
            s = format!("saturate({s})");
        }
        format!("asuint({s})")
    }

    /// u32 source expressions in operand order; SGPR-pair masks come as "lo|hi".
    /// SDWA selects/extends its two sources, DPP reads src0 from another lane.
    fn sources(&mut self, d: &Decoded, ins: &[&Operand], src_f16: bool) -> Result<Vec<String>, &'static str> {
        let mut u = Vec::new();
        for o in ins {
            if d.variant != Variant::Plain && o.field == Some("SRC0") {
                u.push("0u".into()); // replaced below
                continue;
            }
            match o.ty {
                "OPR_SREG" | "OPR_VCC" if o.size == 64 => {
                    let (l, h) = self.read64(d, o).ok_or("op")?;
                    u.push(format!("{l}|{h}"));
                }
                _ if o.size == 64 => return Err("operand 64-bit"),
                _ => u.push(self.read(d, o, src_f16).ok_or("op")?),
            }
        }
        match d.variant {
            Variant::Plain => {}
            Variant::Sdwa => {
                let e = ext_encoding(d).ok_or("SDWA")?;
                let fv = |n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
                let s0 = if fv("S0") == 1 { self.code32(fv("VSRC0"), None, false).ok_or("op")? } else { self.vreg(fv("VSRC0")) };
                if !u.is_empty() {
                    u[0] = format!("sdwa_sel({s0}, {}u, {})", fv("SRC0_SEL"), fv("SRC0_SEXT") == 1);
                }
                if d.enc.field("VSRC1").is_some() && u.len() > 1 {
                    let v1 = fv("VSRC1");
                    let s1 = if fv("S1") == 1 { self.code32(v1, None, false).ok_or("op")? } else { self.vreg(v1) };
                    u[1] = format!("sdwa_sel({s1}, {}u, {})", fv("SRC1_SEL"), fv("SRC1_SEXT") == 1);
                }
            }
            Variant::Dpp16 | Variant::Dpp8 => {
                let e = ext_encoding(d).ok_or("DPP")?;
                let fv = |n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
                let v = self.vreg(fv("VSRC0"));
                let (el, eh) = (self.sreg(126), self.sreg(127));
                let eh = if self.opt.wave64 { eh } else { "0u".into() };
                let t = self.t();
                if d.variant == Variant::Dpp16 {
                    let ctrl = fv("DPP_CTRL");
                    let (fi, bound) = (fv("FI") == 1, fv("BOUND_CTRL") == 1);
                    let src = dpp16_src(ctrl).ok_or("DPP_CTRL")?;
                    self.line(&format!("int {t}_l = {src};"));
                    self.line(&format!(
                        "bool {t}_ok = {t}_l >= 0 && ({fi} || lanebit_at({el}, {eh}, (uint){t}_l));"
                    ));
                    self.line(&format!("uint {t} = rla({v}, (uint)max({t}_l, 0));"));
                    let (rm, bm) = (fv("ROW_MASK"), fv("BANK_MASK"));
                    // write enable: row/bank masks, and an invalid source either reads 0 (bound_ctrl) or disables
                    let mut en = format!("((({rm}u >> (lane_id >> 4)) & 1u) != 0u && (({bm}u >> ((lane_id >> 2) & 3u)) & 1u) != 0u)");
                    if bound {
                        self.line(&format!("{t} = {t}_ok ? {t} : 0u;"));
                    } else {
                        en = format!("({en} && {t}_ok)");
                    }
                    self.dpp_enable = Some(en);
                } else {
                    let sel: u64 = (0..8).map(|i| fv(&format!("LANE_SEL_{i}")) << (3 * i)).sum();
                    let fi = rdna2::field_value(d.enc, &d.words, "SRC0").unwrap_or(0) == 0xEA;
                    self.line(&format!("uint {t}_l = (lane_id & ~7u) | (({sel}u >> (3u * (lane_id & 7u))) & 7u);"));
                    self.line(&format!("uint {t} = rla({v}, {t}_l);"));
                    if !fi {
                        self.line(&format!("{t} = lanebit_at({el}, {eh}, {t}_l) ? {t} : 0u;"));
                    }
                }
                if !u.is_empty() {
                    u[0] = t;
                }
            }
        }
        Ok(u)
    }

    /// Write a VALU result, honouring SDWA dst_sel/dst_unused and DPP write enables.
    fn vwrite(&mut self, d: &Decoded, dst: &Dst, val: &str) {
        let mut val = val.to_string();
        if d.variant == Variant::Sdwa {
            if let (Some(e), Dst::V(n)) = (ext_encoding(d), dst) {
                let fv = |k: &str| rdna2::field_value(e, &d.words, k).unwrap_or(0);
                if e.field("DST_SEL").is_some() {
                    let cur = self.vreg(*n);
                    val = format!("sdwa_dst({val}, {cur}, {}u, {}u)", fv("DST_SEL"), fv("DST_UNUSED"));
                }
            }
        }
        match (self.dpp_enable.take(), dst) {
            (Some(en), Dst::V(n)) => {
                let v = self.vreg(*n);
                let a = self.act();
                self.line(&format!("if ({a} && {en}) {v} = {val};"));
            }
            _ => self.write32(dst, &val),
        }
    }

    /// VOP3P packed math (two 16-bit halves; op_sel picks the source half for the low result,
    /// op_sel_hi for the high one; neg / neg_hi per half), v_pk_fmac_f16 (VOP2) and the dots.
    fn valu_pk(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        if d.variant != Variant::Plain {
            return Err("VOP3P SDWA/DPP");
        }
        let (outs, ins) = Self::ops(d);
        let vop3p = d.enc.name == "ENC_VOP3P";
        let fv = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let (sel, sel_hi) = if vop3p { (fv("OP_SEL"), fv("OP_SEL_HI") | fv("OP_SEL_HI_2") << 2) } else { (0, 0b111) };
        let (neg, neg_hi) = if vop3p { (fv("NEG"), fv("NEG_HI")) } else { (0, 0) };
        let clamp = vop3p && fv("CLAMP") == 1;
        let u = self.sources(d, &ins, name.ends_with("_f16"))?;
        let dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
        let cur = match &dst {
            Dst::V(n) => self.vreg(*n),
            _ => "0u".into(),
        };
        let src = |i: usize| u.get(i).cloned().unwrap_or_else(|| "0u".into());
        let h = |i: usize, hi: bool| -> String {
            let s = src(i);
            if (if hi { sel_hi } else { sel }) >> i & 1 == 1 {
                format!("(({s}) >> 16)")
            } else {
                format!("(({s}) & 0xFFFFu)")
            }
        };
        let fh = |i: usize, hi: bool| -> String {
            let v = format!("f16tof32({})", h(i, hi));
            if (if hi { neg_hi } else { neg }) >> i & 1 == 1 {
                format!("(-{v})")
            } else {
                v
            }
        };
        let si = |i: usize, hi: bool| format!("(((int)({} << 16)) >> 16)", h(i, hi));
        let fr = |e: String| if clamp { format!("(f32tof16(saturate({e})) & 0xFFFFu)") } else { format!("(f32tof16({e}) & 0xFFFFu)") };
        let t = self.t();
        // dot products: one 32-bit result
        let dot: Option<String> = match name {
            "v_dot2_f32_f16" => {
                let mut c = format!("asfloat({})", src(2));
                if neg >> 2 & 1 == 1 {
                    c = format!("(-{c})");
                }
                let e = format!("mad({}, {}, mad({}, {}, {c}))", fh(0, true), fh(1, true), fh(0, false), fh(1, false));
                Some(if clamp { format!("asuint(saturate({e}))") } else { format!("asuint({e})") })
            }
            "v_dot2c_f32_f16" => Some(format!("asuint(mad({}, {}, mad({}, {}, asfloat({cur}))))", fh(0, true), fh(1, true), fh(0, false), fh(1, false))),
            "v_dot4c_i32_i8" => Some(format!("dot4_i8({}, {}, {cur})", src(0), src(1))),
            "v_dot2_i32_i16" => Some(format!("(uint)({} * {} + {} * {} + (int){})", si(0, false), si(1, false), si(0, true), si(1, true), src(2))),
            "v_dot2_u32_u16" => Some(format!("({} * {} + {} * {} + {})", h(0, false), h(1, false), h(0, true), h(1, true), src(2))),
            "v_dot4_u32_u8" => Some(format!("dot4_u8({}, {}, {})", src(0), src(1), src(2))),
            "v_dot4_i32_i8" => Some(format!("dot4_i8({}, {}, {})", src(0), src(1), src(2))),
            "v_dot8_u32_u4" => Some(format!("dot8_u4({}, {}, {})", src(0), src(1), src(2))),
            "v_dot8_i32_i4" => Some(format!("dot8_i4({}, {}, {})", src(0), src(1), src(2))),
            _ => None,
        };
        if let Some(e) = dot {
            self.line(&format!("uint {t} = {e};"));
            self.write32(&dst, &t);
            return Ok(());
        }
        let half = |hi: bool| -> Option<String> {
            let (a, b, c) = (h(0, hi), h(1, hi), h(2, hi));
            let curh = if hi { format!("({cur} >> 16)") } else { format!("({cur} & 0xFFFFu)") };
            Some(match name {
                "v_pk_add_f16" => fr(format!("{} + {}", fh(0, hi), fh(1, hi))),
                "v_pk_mul_f16" => fr(format!("{} * {}", fh(0, hi), fh(1, hi))),
                "v_pk_fma_f16" => fr(format!("mad({}, {}, {})", fh(0, hi), fh(1, hi), fh(2, hi))),
                "v_pk_fmac_f16" => fr(format!("mad({}, {}, f16tof32({curh}))", fh(0, hi), fh(1, hi))),
                "v_pk_min_f16" => fr(format!("min({}, {})", fh(0, hi), fh(1, hi))),
                "v_pk_max_f16" => fr(format!("max({}, {})", fh(0, hi), fh(1, hi))),
                "v_pk_add_u16" => format!("(({a} + {b}) & 0xFFFFu)"),
                "v_pk_sub_u16" => format!("(({a} - {b}) & 0xFFFFu)"),
                "v_pk_add_i16" => format!("((uint)({} + {}) & 0xFFFFu)", si(0, hi), si(1, hi)),
                "v_pk_sub_i16" => format!("((uint)({} - {}) & 0xFFFFu)", si(0, hi), si(1, hi)),
                "v_pk_mul_lo_u16" => format!("(({a} * {b}) & 0xFFFFu)"),
                "v_pk_mad_u16" => format!("(({a} * {b} + {c}) & 0xFFFFu)"),
                "v_pk_mad_i16" => format!("((uint)({} * {} + {}) & 0xFFFFu)", si(0, hi), si(1, hi), si(2, hi)),
                "v_pk_lshlrev_b16" => format!("(({b} << ({a} & 15u)) & 0xFFFFu)"),
                "v_pk_lshrrev_b16" => format!("({b} >> ({a} & 15u))"),
                "v_pk_ashrrev_i16" => format!("((uint)({} >> ({a} & 15u)) & 0xFFFFu)", si(1, hi)),
                "v_pk_max_u16" => format!("max({a}, {b})"),
                "v_pk_min_u16" => format!("min({a}, {b})"),
                "v_pk_max_i16" => format!("((uint)max({}, {}) & 0xFFFFu)", si(0, hi), si(1, hi)),
                "v_pk_min_i16" => format!("((uint)min({}, {}) & 0xFFFFu)", si(0, hi), si(1, hi)),
                _ => return None,
            })
        };
        let (lo, hi) = (half(false).ok_or("VOP3P")?, half(true).ok_or("VOP3P")?);
        self.line(&format!("uint {t} = {lo} | ({hi} << 16);"));
        self.write32(&dst, &t);
        Ok(())
    }

    /// v_movrel*: VGPR indexing with the (uniform) M0 – a switch over the registers the
    /// program uses from the base upwards.
    fn movrel(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        if d.variant != Variant::Plain {
            return Err("movrel SDWA/DPP");
        }
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let vop3 = d.enc.name == "ENC_VOP3";
        let src_code = f("SRC0");
        let vdst = f("VDST");
        let m0 = self.sreg(124);
        let act = self.act();
        let top = self.max_vgpr.max(vdst).max(src_code.saturating_sub(256));
        let _ = vop3;
        let t = self.t();
        let (rd_idx, wr_idx) = match name {
            "v_movrels_b32" => (Some(m0.clone()), None),
            "v_movreld_b32" => (None, Some(m0.clone())),
            "v_movrelsd_b32" => (Some(m0.clone()), Some(m0.clone())),
            "v_movrelsd_2_b32" => (Some(format!("({m0} & 0x3FFu)")), Some(format!("(({m0} >> 16) & 0x3FFu)"))),
            _ => return Err("movrel"),
        };
        let value = match &rd_idx {
            Some(idx) => {
                if src_code < 256 {
                    return Err("movrels: źródło nie jest VGPR");
                }
                let base = src_code - 256;
                let mut cases = String::new();
                for k in 0..=top.saturating_sub(base) {
                    let v = self.vreg(base + k);
                    cases.push_str(&format!(" case {k}u: {t} = {v}; break;"));
                }
                self.line(&format!("uint {t} = 0u; switch ({idx}) {{{cases} default: break; }}"));
                t.clone()
            }
            None => self.code32(src_code, d.literal, false).ok_or("op")?,
        };
        match &wr_idx {
            Some(idx) => {
                let mut cases = String::new();
                for k in 0..=top.saturating_sub(vdst) {
                    let v = self.vreg(vdst + k);
                    cases.push_str(&format!(" case {k}u: {v} = {value}; break;"));
                }
                self.line(&format!("if ({act}) {{ switch ({idx}) {{{cases} default: break; }} }}"));
            }
            None => {
                let v = self.vreg(vdst);
                self.line(&format!("if ({act}) {v} = {value};"));
            }
        }
        Ok(())
    }

    /// 16-bit VALU (f16 / u16 / i16 / b16): sources are the low halves (VOP3 op_sel picks the
    /// high half per source), results go to the low half (op_sel[3]: high half) and the other
    /// half is preserved – AnyPS5 `write16Bits` (GPL-2.0, ported 2026-11-20).
    fn valu16(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let (outs, ins) = Self::ops(d);
        let m = Self::mods(d);
        let u = self.sources(d, &ins, name.contains("f16"))?;
        let plain = d.variant == Variant::Plain;
        let half = |i: usize| -> String {
            let s = u.get(i).map(String::as_str).unwrap_or("0u");
            if plain && m.opsel >> i & 1 == 1 {
                format!("(({s}) >> 16)")
            } else {
                format!("(({s}) & 0xFFFFu)")
            }
        };
        let fh = |i: usize| -> String {
            let mut s = format!("f16tof32({})", half(i));
            if m.abs[i.min(2)] {
                s = format!("abs({s})");
            }
            if m.neg[i.min(2)] {
                s = format!("(-{s})");
            }
            s
        };
        let si = |i: usize| format!("(((int)({} << 16)) >> 16)", half(i));
        let fr = |e: String| -> String {
            let mut s = match m.omod {
                1 => format!("(({e}) * 2.0f)"),
                2 => format!("(({e}) * 4.0f)"),
                3 => format!("(({e}) * 0.5f)"),
                _ => format!("({e})"),
            };
            if m.clamp {
                s = format!("saturate({s})");
            }
            format!("(f32tof16({s}) & 0xFFFFu)")
        };
        let dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
        let cur = match &dst {
            Dst::V(n) => self.vreg(*n),
            _ => "0u".into(),
        };
        let curh = if plain && m.opsel >> 3 & 1 == 1 { format!("({cur} >> 16)") } else { format!("({cur} & 0xFFFFu)") };
        let (a, b, c) = (half(0), half(1), half(2));
        let r: String = match name {
            "v_add_f16" => fr(format!("{} + {}", fh(0), fh(1))),
            "v_sub_f16" => fr(format!("{} - {}", fh(0), fh(1))),
            "v_subrev_f16" => fr(format!("{} - {}", fh(1), fh(0))),
            "v_mul_f16" => fr(format!("{} * {}", fh(0), fh(1))),
            "v_min_f16" => fr(format!("min({}, {})", fh(0), fh(1))),
            "v_max_f16" => fr(format!("max({}, {})", fh(0), fh(1))),
            "v_fma_f16" => fr(format!("mad({}, {}, {})", fh(0), fh(1), fh(2))),
            "v_med3_f16" => fr(format!("med3f({}, {}, {})", fh(0), fh(1), fh(2))),
            "v_min3_f16" => fr(format!("min(min({}, {}), {})", fh(0), fh(1), fh(2))),
            "v_max3_f16" => fr(format!("max(max({}, {}), {})", fh(0), fh(1), fh(2))),
            "v_med3_u16" => format!("med3u({a}, {b}, {c})"),
            "v_min3_u16" => format!("min(min({a}, {b}), {c})"),
            "v_max3_u16" => format!("max(max({a}, {b}), {c})"),
            "v_med3_i16" => format!("((uint)med3i({}, {}, {}) & 0xFFFFu)", si(0), si(1), si(2)),
            "v_min3_i16" => format!("((uint)min(min({}, {}), {}) & 0xFFFFu)", si(0), si(1), si(2)),
            "v_max3_i16" => format!("((uint)max(max({}, {}), {}) & 0xFFFFu)", si(0), si(1), si(2)),
            "v_frexp_mant_f16" => fr(format!("frexp_mant({})", fh(0))),
            "v_frexp_exp_i16_f16" => format!("((uint)frexp_exp({}) & 0xFFFFu)", fh(0)),
            "v_cndmask_b16" => return Err("VALU 16-bit"),
            "v_mad_f16" => fr(format!("mad_nf({}, {}, {})", fh(0), fh(1), fh(2))),
            "v_fmac_f16" => fr(format!("mad({}, {}, f16tof32({curh}))", fh(0), fh(1))),
            "v_fmamk_f16" => fr(format!("mad({}, f16tof32({} & 0xFFFFu), {})", fh(0), u[1], fh(2))),
            "v_fmaak_f16" => fr(format!("mad({}, {}, f16tof32({} & 0xFFFFu))", fh(0), fh(1), u[2])),
            "v_ldexp_f16" => fr(format!("ldexp({}, (float){})", fh(0), si(1))),
            "v_rcp_f16" => fr(format!("rcp({})", fh(0))),
            "v_rsq_f16" => fr(format!("rsqrt({})", fh(0))),
            "v_sqrt_f16" => fr(format!("sqrt({})", fh(0))),
            "v_log_f16" => fr(format!("log2({})", fh(0))),
            "v_exp_f16" => fr(format!("exp2({})", fh(0))),
            "v_sin_f16" => fr(format!("sin({} * 6.2831853f)", fh(0))),
            "v_cos_f16" => fr(format!("cos({} * 6.2831853f)", fh(0))),
            "v_fract_f16" => fr(format!("fract_f({})", fh(0))),
            "v_floor_f16" => fr(format!("floor({})", fh(0))),
            "v_ceil_f16" => fr(format!("ceil({})", fh(0))),
            "v_trunc_f16" => fr(format!("trunc({})", fh(0))),
            "v_rndne_f16" => fr(format!("round({})", fh(0))),
            "v_cvt_f16_u16" => fr(format!("(float){a}")),
            "v_cvt_f16_i16" => fr(format!("(float){}", si(0))),
            "v_cvt_u16_f16" => format!("(cvt_u32({}) > 0xFFFFu ? 0xFFFFu : cvt_u32({}))", fh(0), fh(0)),
            "v_cvt_i16_f16" => format!("((uint)clamp(cvt_i32({}), -32768, 32767) & 0xFFFFu)", fh(0)),
            "v_add_nc_u16" | "v_add_u16" => format!("(({a} + {b}) & 0xFFFFu)"),
            "v_sub_nc_u16" | "v_sub_u16" => format!("(({a} - {b}) & 0xFFFFu)"),
            "v_subrev_nc_u16" | "v_subrev_u16" => format!("(({b} - {a}) & 0xFFFFu)"),
            "v_add_nc_i16" => format!("((uint)({} + {}) & 0xFFFFu)", si(0), si(1)),
            "v_sub_nc_i16" => format!("((uint)({} - {}) & 0xFFFFu)", si(0), si(1)),
            "v_mul_lo_u16" => format!("(({a} * {b}) & 0xFFFFu)"),
            "v_lshlrev_b16" => format!("(({b} << ({a} & 15u)) & 0xFFFFu)"),
            "v_lshrrev_b16" => format!("({b} >> ({a} & 15u))"),
            "v_ashrrev_i16" => format!("((uint)({} >> ({a} & 15u)) & 0xFFFFu)", si(1)),
            "v_max_u16" => format!("max({a}, {b})"),
            "v_min_u16" => format!("min({a}, {b})"),
            "v_max_i16" => format!("((uint)max({}, {}) & 0xFFFFu)", si(0), si(1)),
            "v_min_i16" => format!("((uint)min({}, {}) & 0xFFFFu)", si(0), si(1)),
            "v_mad_u16" => format!("(({a} * {b} + {c}) & 0xFFFFu)"),
            "v_mad_i16" => format!("((uint)({} * {} + {}) & 0xFFFFu)", si(0), si(1), si(2)),
            _ => return Err("VALU 16-bit"),
        };
        let t = self.t();
        self.line(&format!("uint {t} = {r};"));
        if d.variant == Variant::Sdwa {
            self.vwrite(d, &dst, &t);
        } else {
            let merged = if m.opsel >> 3 & 1 == 1 { format!("({cur} & 0xFFFFu) | ({t} << 16)") } else { format!("({cur} & 0xFFFF0000u) | {t}") };
            self.vwrite(d, &dst, &merged);
        }
        Ok(())
    }

    /// 64-bit VALU: integer compares / shifts / v_mad_u64_u32 and the basic f64 operations.
    fn valu64(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let (outs, ins) = Self::ops(d);
        let m = Self::mods(d);
        if d.variant != Variant::Plain {
            return Err("64-bit SDWA/DPP");
        }
        let mut lo_hi: Vec<(String, String)> = Vec::new();
        for o in &ins {
            if o.size == 64 {
                lo_hi.push(self.read64(d, o).ok_or("op")?);
            } else {
                let v = self.read(d, o, false).ok_or("op")?;
                lo_hi.push((v, "0u".into()));
            }
        }
        let u64v = |i: usize| format!("(((uint64_t){} << 32) | (uint64_t){})", lo_hi[i].1, lo_hi[i].0);
        let f64v = |i: usize| {
            let mut s = format!("asdouble({}, {})", lo_hi[i].0, lo_hi[i].1);
            if m.abs[i.min(2)] {
                s = format!("abs({s})");
            }
            if m.neg[i.min(2)] {
                s = format!("(-{s})");
            }
            s
        };
        let dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
        let t = self.t();
        if let Some(rest) = name.strip_prefix("v_cmpx_").or_else(|| name.strip_prefix("v_cmp_")) {
            let (op, ty) = rest.rsplit_once('_').ok_or("VOPC")?;
            let (a, b) = match ty {
                "u64" => (u64v(0), u64v(1)),
                "i64" => (format!("(int64_t){}", u64v(0)), format!("(int64_t){}", u64v(1))),
                "f64" => (f64v(0), f64v(1)),
                _ => return Err("VOPC 64"),
            };
            let cond = match op {
                "f" => "false".to_string(),
                "lt" => format!("{a} < {b}"),
                "eq" => format!("{a} == {b}"),
                "le" => format!("{a} <= {b}"),
                "gt" => format!("{a} > {b}"),
                "ne" => format!("{a} != {b}"),
                "lg" if ty == "f64" => format!("({a} < {b} || {a} > {b})"),
                "nlg" => format!("!({a} < {b} || {a} > {b})"),
                "lg" => format!("{a} != {b}"),
                "ge" => format!("{a} >= {b}"),
                "neq" => format!("!({a} == {b})"),
                "nlt" => format!("!({a} < {b})"),
                "nge" => format!("!({a} >= {b})"),
                "ngt" => format!("!({a} > {b})"),
                "nle" => format!("!({a} <= {b})"),
                "o" => format!("({a} == {a} && {b} == {b})"),
                "u" => format!("({a} != {a} || {b} != {b})"),
                "t" | "tru" => "true".to_string(),
                _ => return Err("VOPC 64"),
            };
            let dst = if name.starts_with("v_cmpx_") { Dst::S(126) } else { dst };
            self.write_mask(&dst, &cond);
            return Ok(());
        }
        let r64: String = match name {
            "v_lshlrev_b64" => format!("({} << ({} & 63u))", u64v(1), lo_hi[0].0),
            "v_lshrrev_b64" => format!("({} >> ({} & 63u))", u64v(1), lo_hi[0].0),
            "v_ashrrev_i64" => format!("(uint64_t)((int64_t){} >> ({} & 63u))", u64v(1), lo_hi[0].0),
            "v_mad_u64_u32" | "v_mad_i64_i32" => {
                let p = if name == "v_mad_u64_u32" {
                    format!("((uint64_t){} * (uint64_t){})", lo_hi[0].0, lo_hi[1].0)
                } else {
                    format!("(uint64_t)((int64_t)(int){} * (int64_t)(int){})", lo_hi[0].0, lo_hi[1].0)
                };
                self.line(&format!("uint64_t {t}_p = {p}; uint64_t {t}_c = {};", u64v(2)));
                self.line(&format!("uint64_t {t}_w = {t}_p + {t}_c;"));
                let sdst = outs.get(1).map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
                self.write_mask(&sdst, &format!("{t}_w < {t}_c"));
                self.write64(&dst, &format!("(uint){t}_w"), &format!("(uint)({t}_w >> 32)"));
                return Ok(());
            }
            _ => {
                // f64 results
                let e = match name {
                    "v_add_f64" => format!("{} + {}", f64v(0), f64v(1)),
                    "v_mul_f64" => format!("{} * {}", f64v(0), f64v(1)),
                    "v_fma_f64" => format!("fma({}, {}, {})", f64v(0), f64v(1), f64v(2)),
                    "v_min_f64" => format!("min({}, {})", f64v(0), f64v(1)),
                    "v_max_f64" => format!("max({}, {})", f64v(0), f64v(1)),
                    "v_rcp_f64" => format!("(1.0 / {})", f64v(0)),
                    "v_cvt_f64_f32" => format!("(double)asfloat({})", lo_hi[0].0),
                    "v_cvt_f64_i32" => format!("(double)(int){}", lo_hi[0].0),
                    "v_cvt_f64_u32" => format!("(double){}", lo_hi[0].0),
                    "v_floor_f64" => format!("floor_d({})", f64v(0)),
                    "v_trunc_f64" => format!("(double)(int64_t){}", f64v(0)),
                    "v_ldexp_f64" => format!("({} * (double)exp2((float)(int){}))", f64v(0), lo_hi[1].0),
                    "v_cvt_f32_f64" => {
                        self.line(&format!("uint {t} = asuint((float){});", f64v(0)));
                        self.write32(&dst, &t);
                        return Ok(());
                    }
                    "v_cvt_i32_f64" => {
                        self.line(&format!("uint {t} = (uint)(int)clamp({}, -2147483648.0, 2147483647.0);", f64v(0)));
                        self.write32(&dst, &t);
                        return Ok(());
                    }
                    "v_cvt_u32_f64" => {
                        self.line(&format!("uint {t} = (uint)clamp({}, 0.0, 4294967295.0);", f64v(0)));
                        self.write32(&dst, &t);
                        return Ok(());
                    }
                    _ => return Err("VALU 64-bit"),
                };
                let mut e = e;
                if m.clamp {
                    e = format!("saturate({e})");
                }
                self.line(&format!("double {t}_d = {e}; uint {t}_l, {t}_h; asuint({t}_d, {t}_l, {t}_h);"));
                self.write64(&dst, &format!("{t}_l"), &format!("{t}_h"));
                return Ok(());
            }
        };
        self.line(&format!("uint64_t {t}_w = {r64};"));
        self.write64(&dst, &format!("(uint){t}_w"), &format!("(uint)({t}_w >> 32)"));
        Ok(())
    }

    fn valu(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        if name.starts_with("v_interp_") {
            return self.interp(d, name);
        }
        let (outs, ins) = Self::ops(d);
        let f16 = name.contains("f16") || name.contains("_i16") || name.contains("_u16") || name.contains("_b16");
        let cmp = name.starts_with("v_cmp");
        // inline constants are f16 patterns only where the source itself is 16-bit
        let src_f16 = matches!(name, "v_cvt_f32_f16" | "v_pack_b32_f16") || (cmp && name.ends_with("_f16"));
        let mix = name.starts_with("v_fma_mix");
        if name.starts_with("v_pk_") || name.starts_with("v_dot") {
            return self.valu_pk(d, name);
        }
        if name.starts_with("v_movrel") {
            return self.movrel(d, name);
        }
        if d.enc.name == "ENC_VOP3P" && !mix {
            return Err("VOP3P");
        }
        if d.variant != Variant::Plain && ext_encoding(d).is_none() {
            return Err("SDWA/DPP w tym kodowaniu");
        }
        self.dpp_enable = None;
        if name.contains("f64") || name.contains("_i64") || name.contains("_u64") || name.contains("_b64") {
            return self.valu64(d, name);
        }
        if f16 && !mix && !cmp && !matches!(name, "v_cvt_f32_f16" | "v_cvt_f16_f32" | "v_cvt_pkrtz_f16_f32" | "v_pack_b32_f16") {
            return self.valu16(d, name);
        }
        let m = Self::mods(d);
        let u = self.sources(d, &ins, src_f16)?;
        if let Some(rest) = name.strip_prefix("v_cmpx_").or_else(|| name.strip_prefix("v_cmp_")) {
            return self.vopc(d, rest, name.starts_with("v_cmpx_"), &outs, &u, &m);
        }
        let dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
        let cur = match &dst {
            Dst::V(n) => self.vreg(*n),
            _ => "0u".into(),
        };
        let a = u.first().cloned().unwrap_or_default();
        let b = u.get(1).cloned().unwrap_or_default();
        let c = u.get(2).cloned().unwrap_or_default();
        let fs = |i: usize| Self::fsrc(&m, i, u.get(i).map(String::as_str).unwrap_or("0u"));
        let fr = |e: String| Self::fres(&m, &e);
        let wmask = if self.opt.wave64 { 63 } else { 31 };
        let res: String = match name {
            "v_mov_b32" | "v_mov_fed_b32" => a.clone(),
            "v_not_b32" => format!("~{a}"),
            "v_bfrev_b32" => format!("reversebits({a})"),
            "v_ffbh_u32" => format!("(({a}) == 0u ? 0xFFFFFFFFu : 31u - firstbithigh({a}))"),
            "v_ffbl_b32" => format!("(({a}) == 0u ? 0xFFFFFFFFu : firstbitlow({a}))"),
            "v_ffbh_i32" => format!("ffbh_i({a})"),
            "v_cvt_f32_i32" => fr(format!("(float)(int){a}")),
            "v_cvt_f32_u32" => fr(format!("(float){a}")),
            "v_cvt_u32_f32" => format!("cvt_u32({})", fs(0)),
            "v_cvt_i32_f32" => format!("(uint)cvt_i32({})", fs(0)),
            "v_cvt_rpi_i32_f32" => format!("(uint)cvt_i32(floor({} + 0.5f))", fs(0)),
            "v_cvt_flr_i32_f32" => format!("(uint)cvt_i32(floor({}))", fs(0)),
            "v_cvt_f32_ubyte0" => fr(format!("(float)({a} & 0xFFu)")),
            "v_cvt_f32_ubyte1" => fr(format!("(float)(({a} >> 8) & 0xFFu)")),
            "v_cvt_f32_ubyte2" => fr(format!("(float)(({a} >> 16) & 0xFFu)")),
            "v_cvt_f32_ubyte3" => fr(format!("(float)({a} >> 24)")),
            "v_cvt_f32_f16" => {
                let h = if m.opsel & 1 == 1 { format!("({a} >> 16)") } else { format!("({a} & 0xFFFFu)") };
                fr(format!("f16tof32({h})"))
            }
            "v_cvt_f16_f32" => {
                let h = format!("(f32tof16({}) & 0xFFFFu)", fs(0));
                if m.opsel >> 3 & 1 == 1 {
                    format!("({cur} & 0xFFFFu) | ({h} << 16)")
                } else if d.variant == Variant::Sdwa {
                    h
                } else {
                    format!("({cur} & 0xFFFF0000u) | {h}")
                }
            }
            "v_cvt_pkrtz_f16_f32" => format!("f16_rtz({}) | (f16_rtz({}) << 16)", fs(0), fs(1)),
            "v_pack_b32_f16" => {
                let lo = if m.opsel & 1 == 1 { format!("({a} >> 16)") } else { format!("({a} & 0xFFFFu)") };
                let hi = if m.opsel & 2 == 2 { format!("({b} >> 16)") } else { format!("({b} & 0xFFFFu)") };
                format!("{lo} | ({hi} << 16)")
            }
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                let fv = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
                let (sel, hi2) = (fv("OP_SEL"), fv("OP_SEL_HI") | fv("OP_SEL_HI_2") << 2);
                let (neg, abs) = (fv("NEG"), fv("NEG_HI"));
                let src = |i: usize| -> String {
                    let raw = &u[i];
                    let mut s = if hi2 >> i & 1 == 1 {
                        if sel >> i & 1 == 1 {
                            format!("f16tof32({raw} >> 16)")
                        } else {
                            format!("f16tof32({raw} & 0xFFFFu)")
                        }
                    } else {
                        format!("asfloat({raw})")
                    };
                    if abs >> i & 1 == 1 {
                        s = format!("abs({s})");
                    }
                    if neg >> i & 1 == 1 {
                        s = format!("(-{s})");
                    }
                    s
                };
                let mut e = format!("mad({}, {}, {})", src(0), src(1), src(2));
                if fv("CLAMP") == 1 {
                    e = format!("saturate({e})");
                }
                match name {
                    "v_fma_mix_f32" => format!("asuint({e})"),
                    "v_fma_mixlo_f16" => format!("({cur} & 0xFFFF0000u) | (f32tof16({e}) & 0xFFFFu)"),
                    _ => format!("({cur} & 0xFFFFu) | (f32tof16({e}) << 16)"),
                }
            }
            "v_rcp_f32" | "v_rcp_iflag_f32" => fr(format!("rcp({})", fs(0))),
            "v_rsq_f32" => fr(format!("rsqrt({})", fs(0))),
            "v_sqrt_f32" => fr(format!("sqrt({})", fs(0))),
            "v_exp_f32" => fr(format!("exp2({})", fs(0))),
            "v_log_f32" => fr(format!("log2({})", fs(0))),
            "v_sin_f32" => fr(format!("sin({} * 6.2831853f)", fs(0))),
            "v_cos_f32" => fr(format!("cos({} * 6.2831853f)", fs(0))),
            "v_fract_f32" => fr(format!("fract_f({})", fs(0))),
            "v_trunc_f32" => fr(format!("trunc({})", fs(0))),
            "v_floor_f32" => fr(format!("floor({})", fs(0))),
            "v_ceil_f32" => fr(format!("ceil({})", fs(0))),
            "v_rndne_f32" => fr(format!("round({})", fs(0))),
            "v_frexp_mant_f32" => fr(format!("frexp_mant({})", fs(0))),
            "v_frexp_exp_i32_f32" => format!("(uint)frexp_exp({})", fs(0)),
            "v_ldexp_f32" => fr(format!("ldexp({}, (float)(int){b})", fs(0))),
            "v_add_f32" => fr(format!("{} + {}", fs(0), fs(1))),
            "v_sub_f32" => fr(format!("{} - {}", fs(0), fs(1))),
            "v_subrev_f32" => fr(format!("{} - {}", fs(1), fs(0))),
            "v_mul_f32" => fr(format!("{} * {}", fs(0), fs(1))),
            "v_mul_legacy_f32" => fr(format!("mul_legacy({}, {})", fs(0), fs(1))),
            "v_min_f32" => fr(format!("min({}, {})", fs(0), fs(1))),
            "v_max_f32" => fr(format!("max({}, {})", fs(0), fs(1))),
            "v_fmac_f32" => fr(format!("mad({}, {}, asfloat({cur}))", fs(0), fs(1))),
            "v_mac_f32" => fr(format!("mad_nf({}, {}, asfloat({cur}))", fs(0), fs(1))),
            "v_fmac_legacy_f32" => fr(format!("fma_legacy({}, {}, asfloat({cur}))", fs(0), fs(1))),
            "v_fma_f32" => fr(format!("mad({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_mad_f32" => fr(format!("mad_nf({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_fma_legacy_f32" => fr(format!("fma_legacy({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_fmamk_f32" => fr(format!("mad({}, asfloat({b}), {})", fs(0), fs(2))),
            "v_fmaak_f32" => fr(format!("mad({}, {}, asfloat({c}))", fs(0), fs(1))),
            "v_madmk_f32" => fr(format!("mad_nf({}, asfloat({b}), {})", fs(0), fs(2))),
            "v_madak_f32" => fr(format!("mad_nf({}, {}, asfloat({c}))", fs(0), fs(1))),
            "v_min3_f32" => fr(format!("min(min({}, {}), {})", fs(0), fs(1), fs(2))),
            "v_max3_f32" => fr(format!("max(max({}, {}), {})", fs(0), fs(1), fs(2))),
            "v_med3_f32" => fr(format!("med3f({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_cubeid_f32" => fr(format!("cube_id({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_cubesc_f32" => fr(format!("cube_sc({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_cubetc_f32" => fr(format!("cube_tc({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_cubema_f32" => fr(format!("cube_ma({}, {}, {})", fs(0), fs(1), fs(2))),
            "v_add_nc_u32" => format!("{a} + {b}"),
            "v_sub_nc_u32" => format!("{a} - {b}"),
            "v_subrev_nc_u32" => format!("{b} - {a}"),
            "v_mul_lo_u32" => format!("{a} * {b}"),
            "v_mul_hi_u32" => format!("(uint)(((uint64_t){a} * (uint64_t){b}) >> 32)"),
            "v_mul_hi_i32" => format!("(uint)(((int64_t)(int){a} * (int64_t)(int){b}) >> 32)"),
            "v_mul_u32_u24" => format!("({a} & 0xFFFFFFu) * ({b} & 0xFFFFFFu)"),
            "v_mul_hi_u32_u24" => format!("(uint)(((uint64_t)({a} & 0xFFFFFFu) * (uint64_t)({b} & 0xFFFFFFu)) >> 32)"),
            "v_mul_i32_i24" => format!("(uint)(i24({a}) * i24({b}))"),
            "v_mad_u32_u24" => format!("({a} & 0xFFFFFFu) * ({b} & 0xFFFFFFu) + {c}"),
            "v_mad_i32_i24" => format!("(uint)(i24({a}) * i24({b}) + (int){c})"),
            "v_and_b32" => format!("{a} & {b}"),
            "v_or_b32" => format!("{a} | {b}"),
            "v_xor_b32" => format!("{a} ^ {b}"),
            "v_xnor_b32" => format!("~({a} ^ {b})"),
            "v_lshlrev_b32" => format!("{b} << ({a} & 31u)"),
            "v_lshrrev_b32" => format!("{b} >> ({a} & 31u)"),
            "v_ashrrev_i32" => format!("(uint)((int){b} >> ({a} & 31u))"),
            "v_lshl_add_u32" => format!("({a} << ({b} & 31u)) + {c}"),
            "v_add_lshl_u32" => format!("({a} + {b}) << ({c} & 31u)"),
            "v_lshl_or_b32" => format!("({a} << ({b} & 31u)) | {c}"),
            "v_and_or_b32" => format!("({a} & {b}) | {c}"),
            "v_or3_b32" => format!("{a} | {b} | {c}"),
            "v_xor3_b32" => format!("{a} ^ {b} ^ {c}"),
            "v_add3_u32" => format!("{a} + {b} + {c}"),
            "v_xad_u32" => format!("({a} ^ {b}) + {c}"),
            "v_bfe_u32" => format!("bfe_u({a}, {b}, {c})"),
            "v_bfe_i32" => format!("(uint)bfe_i({a}, {b}, {c})"),
            "v_bfi_b32" => format!("({a} & {b}) | (~{a} & {c})"),
            "v_bfm_b32" => format!("((({a} & 31u) == 0u) ? 0u : (0xFFFFFFFFu >> (32u - ({a} & 31u)))) << ({b} & 31u)"),
            "v_bcnt_u32_b32" => format!("countbits({a}) + {b}"),
            "v_perm_b32" => format!("perm_b32({a}, {b}, {c})"),
            "v_alignbit_b32" => format!("alignbit({a}, {b}, {c})"),
            "v_alignbyte_b32" => format!("alignbit({a}, {b}, ({c} & 3u) * 8u)"),
            "v_min_u32" => format!("min({a}, {b})"),
            "v_max_u32" => format!("max({a}, {b})"),
            "v_min_i32" => format!("(uint)min((int){a}, (int){b})"),
            "v_max_i32" => format!("(uint)max((int){a}, (int){b})"),
            "v_min3_u32" => format!("min(min({a}, {b}), {c})"),
            "v_max3_u32" => format!("max(max({a}, {b}), {c})"),
            "v_med3_u32" => format!("med3u({a}, {b}, {c})"),
            "v_min3_i32" => format!("(uint)min(min((int){a}, (int){b}), (int){c})"),
            "v_max3_i32" => format!("(uint)max(max((int){a}, (int){b}), (int){c})"),
            "v_med3_i32" => format!("(uint)med3i((int){a}, (int){b}, (int){c})"),
            "v_sad_u32" => format!("(max({a}, {b}) - min({a}, {b})) + {c}"),
            "v_cvt_pk_u8_f32" => format!(
                "({c} & ~(0xFFu << (8u * ({b} & 3u)))) | (((uint)clamp(round({}), 0.0f, 255.0f)) << (8u * ({b} & 3u)))",
                fs(0)
            ),
            "v_mbcnt_lo_u32_b32" => format!("countbits({a} & lanemask_lt_lo()) + {b}"),
            "v_mbcnt_hi_u32_b32" => format!("countbits({a} & lanemask_lt_hi()) + {b}"),
            "v_cndmask_b32" => {
                let (l, h) = u.get(2).ok_or("maska")?.split_once('|').ok_or("maska")?;
                let bit = self.lane_of(l, h);
                format!("{bit} ? {b} : {a}")
            }
            "v_permlane16_b32" | "v_permlanex16_b32" => {
                // lane i of each row reads lane sel[i] of the same / the other row (32-lane halves)
                let (fi, bound) = (m.opsel & 1 == 1, m.opsel & 2 == 2);
                let t = self.t();
                let other = if name == "v_permlanex16_b32" { "^ 16u" } else { "" };
                self.line(&format!(
                    "uint {t}_n = (lane_id & 15u) < 8u ? (({b} >> (4u * (lane_id & 7u))) & 15u) : (({c} >> (4u * (lane_id & 7u))) & 15u);"
                ));
                self.line(&format!("uint {t}_l = (lane_id & ~31u) | (((lane_id & 16u) {other}) & 16u) | {t}_n;"));
                let (el, eh) = (self.sreg(126), self.sreg(127));
                let eh = if self.opt.wave64 { eh } else { "0u".into() };
                self.line(&format!("bool {t}_ok = {fi} || lanebit_at({el}, {eh}, {t}_l);"));
                self.line(&format!("uint {t} = rla({a}, {t}_l);"));
                if bound {
                    format!("{t}_ok ? {t} : 0u")
                } else {
                    format!("{t}_ok ? {t} : {cur}")
                }
            }
            "v_readfirstlane_b32" => {
                let (el, eh) = (self.sreg(126), self.sreg(127));
                let eh = if self.opt.wave64 { eh } else { "0u".into() };
                let t = self.t();
                self.line(&format!("uint {t} = rla({a}, first_lane({el}, {eh}));"));
                self.write32(&dst, &t);
                return Ok(());
            }
            "v_readlane_b32" => {
                let t = self.t();
                self.line(&format!("uint {t} = rla({a}, {b} & {wmask}u);"));
                self.write32(&dst, &t);
                return Ok(());
            }
            "v_writelane_b32" => {
                // ignores EXEC
                if let Dst::V(n) = &dst {
                    let v = self.vreg(*n);
                    self.line(&format!("if (lane_id == ({b} & {wmask}u)) {v} = {a};"));
                }
                return Ok(());
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" => {
                let (x, y) = if name == "v_subrev_co_u32" { (b.clone(), a.clone()) } else { (a.clone(), b.clone()) };
                let t = self.t();
                let carry = if name == "v_add_co_u32" {
                    self.line(&format!("uint {t} = {x} + {y};"));
                    format!("{t} < {x}")
                } else {
                    self.line(&format!("uint {t} = {x} - {y};"));
                    format!("{y} > {x}")
                };
                let sdst = outs.get(1).map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
                self.write_mask(&sdst, &carry);
                self.vwrite(d, &dst, &t);
                return Ok(());
            }
            "v_add_co_ci_u32" | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => {
                let (l, h) = u.get(2).ok_or("maska")?.split_once('|').ok_or("maska")?;
                let cin = format!("({} ? 1u : 0u)", self.lane_of(l, h));
                let (x, y) = if name == "v_subrev_co_ci_u32" { (b.clone(), a.clone()) } else { (a.clone(), b.clone()) };
                let t = self.t();
                if name == "v_add_co_ci_u32" {
                    self.line(&format!("uint64_t {t}w = (uint64_t){x} + (uint64_t){y} + (uint64_t){cin};"));
                } else {
                    self.line(&format!("uint64_t {t}w = (uint64_t){x} - (uint64_t){y} - (uint64_t){cin};"));
                }
                self.line(&format!("uint {t} = (uint){t}w;"));
                let sdst = outs.get(1).map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
                self.write_mask(&sdst, &format!("({t}w >> 32) != 0ull"));
                self.vwrite(d, &dst, &t);
                return Ok(());
            }
            _ => return Err("VALU"),
        };
        let t = self.t();
        self.line(&format!("uint {t} = {res};"));
        self.vwrite(d, &dst, &t);
        Ok(())
    }

    fn vopc(&mut self, d: &Decoded, rest: &str, cmpx: bool, outs: &[&Operand], u: &[String], m: &Mods) -> Result<(), &'static str> {
        let (op, ty) = rest.rsplit_once('_').ok_or("VOPC")?;
        if u.len() < 2 {
            return Err("VOPC");
        }
        let (mut ua, mut ub) = (u[0].clone(), u[1].clone());
        let half = |i: u64, s: &str| if m.opsel >> i & 1 == 1 { format!("(({s}) >> 16)") } else { format!("(({s}) & 0xFFFFu)") };
        let ty = match ty {
            "f16" => {
                // compare in f32: exact for every f16 value (class keeps the f16 view)
                if op == "class" {
                    let (h, c) = (half(0, &ua), ub.clone());
                    let mut v = format!("f16tof32({h})");
                    if m.abs[0] {
                        v = format!("abs({v})");
                    }
                    if m.neg[0] {
                        v = format!("(-{v})");
                    }
                    let cond = format!("((({c}) >> fclass16(f32tof16({v}))) & 1u) != 0u");
                    return self.vopc_write(d, cmpx, outs, &cond);
                }
                ua = format!("asuint(f16tof32({}))", half(0, &ua));
                ub = format!("asuint(f16tof32({}))", half(1, &ub));
                "f32"
            }
            "u16" => {
                ua = half(0, &ua);
                ub = half(1, &ub);
                "u32"
            }
            "i16" => {
                ua = format!("(uint)(((int)({} << 16)) >> 16)", half(0, &ua));
                ub = format!("(uint)(((int)({} << 16)) >> 16)", half(1, &ub));
                "i32"
            }
            t => t,
        };
        let cond = match ty {
            "f32" => {
                let (a, b) = (Self::fsrc(m, 0, &ua), Self::fsrc(m, 1, &ub));
                match op {
                    "class" => format!("((({ub}) >> fclass({a})) & 1u) != 0u"),
                    "f" => "false".to_string(),
                    "lt" => format!("{a} < {b}"),
                    "eq" => format!("{a} == {b}"),
                    "le" => format!("{a} <= {b}"),
                    "gt" => format!("{a} > {b}"),
                    "lg" => format!("({a} < {b} || {a} > {b})"),
                    "ge" => format!("{a} >= {b}"),
                    "o" => format!("(!isnan({a}) && !isnan({b}))"),
                    "u" => format!("(isnan({a}) || isnan({b}))"),
                    "nge" => format!("!({a} >= {b})"),
                    "nlg" => format!("!({a} < {b} || {a} > {b})"),
                    "ngt" => format!("!({a} > {b})"),
                    "nle" => format!("!({a} <= {b})"),
                    "neq" => format!("!({a} == {b})"),
                    "nlt" => format!("!({a} < {b})"),
                    "tru" => "true".to_string(),
                    _ => return Err("VOPC f32"),
                }
            }
            "i32" | "u32" => {
                let (a, b) = if ty == "i32" { (format!("(int){ua}"), format!("(int){ub}")) } else { (ua.clone(), ub.clone()) };
                match op {
                    "f" => "false".to_string(),
                    "lt" => format!("{a} < {b}"),
                    "eq" => format!("{a} == {b}"),
                    "le" => format!("{a} <= {b}"),
                    "gt" => format!("{a} > {b}"),
                    "ne" | "lg" => format!("{a} != {b}"),
                    "ge" => format!("{a} >= {b}"),
                    "t" | "tru" => "true".to_string(),
                    _ => return Err("VOPC int"),
                }
            }
            _ => return Err("VOPC typ"),
        };
        self.vopc_write(d, cmpx, outs, &cond)
    }

    /// Destination of a compare: VCC / SDST (SDWA SD) / EXEC (cmpx).
    fn vopc_write(&mut self, d: &Decoded, cmpx: bool, outs: &[&Operand], cond: &str) -> Result<(), &'static str> {
        let mut dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
        if d.variant == Variant::Sdwa {
            // SDWA compares write VCC unless SD selects SDST
            if let Some(e) = ext_encoding(d) {
                if rdna2::field_value(e, &d.words, "SD") == Some(1) {
                    let sd = rdna2::field_value(e, &d.words, "SDST").unwrap_or(106);
                    dst = if sd == 125 { Dst::Null } else { Dst::S(sd) };
                }
            }
        }
        if cmpx {
            dst = Dst::S(126);
        }
        self.write_mask(&dst, cond);
        Ok(())
    }

    // ---------------------------------------------------------------- cross-lane (DS)

    /// buffer_atomic_*: interlocked operations on the flat memory (GLC returns the old value).
    fn buffer_atomic(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let op = name.trim_start_matches("buffer_atomic_");
        let func = match op {
            "add" => "InterlockedAdd",
            "smin" | "umin" => "InterlockedMin",
            "smax" | "umax" => "InterlockedMax",
            "and" => "InterlockedAnd",
            "or" => "InterlockedOr",
            "xor" => "InterlockedXor",
            "swap" => "InterlockedExchange",
            "cmpswap" => "InterlockedCompareExchange",
            _ => return Err("buffer_atomic"),
        };
        let (vdata, vaddr, srsrc) = (f("VDATA"), f("VADDR"), Self::field(d, "SRSRC"));
        let soff = self.code32(f("SOFFSET"), None, false).ok_or("op")?;
        let (offen, idxen) = (f("OFFEN") != 0, f("IDXEN") != 0);
        let (d0, d1, d2) = (self.sreg(srsrc), self.sreg(srsrc + 1), self.sreg(srsrc + 2));
        let index = if idxen { self.vreg(vaddr) } else { "0u".into() };
        let voff = if offen { self.vreg(vaddr + idxen as u64) } else { "0u".into() };
        let t = self.t();
        self.line(&format!("uint {t}_stride = ({d1} >> 16) & 0x3FFFu;"));
        self.line(&format!("uint {t}_off = {voff} + {}u;", f("OFFSET")));
        self.line(&format!("uint {t}_a = ({d0} + {soff} + {t}_off + {t}_stride * {index}) & ~3u;"));
        self.line(&format!("bool {t}_ok = ({t}_stride == 0u) ? ({t}_off + 4u <= {d2}) : ({index} < {d2});"));
        let v = self.vreg(vdata);
        let act = self.act();
        let signed = op == "smin" || op == "smax";
        let ty = if signed { "int" } else { "uint" };
        self.line(&format!("{ty} {t}_old = 0;"));
        let call = if op == "cmpswap" {
            let cmp = self.vreg(vdata + 1);
            format!("mem.InterlockedCompareExchange({t}_a, {cmp}, {v}, {t}_old);")
        } else if signed {
            format!("mem.{func}({t}_a, (int){v}, {t}_old);")
        } else {
            format!("mem.{func}({t}_a, {v}, {t}_old);")
        };
        self.line(&format!("if ({act} && {t}_ok) {{ {call} }}"));
        if f("GLC") == 1 {
            self.line(&format!("if ({act}) {v} = {t}_ok ? (uint){t}_old : 0u;"));
        }
        Ok(())
    }

    /// FLAT / global: the flat memory model (address = low 32 bits).
    fn flat(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let op = name.trim_start_matches("global_").trim_start_matches("flat_");
        if let Some(a) = op.strip_prefix("atomic_") {
            return self.flat_atomic(d, a);
        }
        let (store, count) = match op {
            "load_dword" => (false, 1),
            "load_dwordx2" => (false, 2),
            "load_dwordx3" => (false, 3),
            "load_dwordx4" => (false, 4),
            "store_dword" => (true, 1),
            "store_dwordx2" => (true, 2),
            "store_dwordx3" => (true, 3),
            "store_dwordx4" => (true, 4),
            _ => return Err("FLAT"),
        };
        let global = d.enc.name == "ENC_FLAT_GLBL";
        let addr = self.vreg(f("ADDR"));
        let saddr = f("SADDR");
        let base = if global && saddr != 0x7D && saddr != 0x7F {
            let s = self.sreg(saddr);
            format!("{s} + {addr}")
        } else {
            addr
        };
        let raw = f("OFFSET") as u32;
        let off = if global { (((raw << 20) as i32) >> 20) as u32 } else { raw };
        let t = self.t();
        self.line(&format!("uint {t}_a = {base} + {};", hex(off)));
        let act = self.act();
        let mut st = Vec::new();
        for i in 0..count {
            if store {
                let v = self.vreg(f("DATA") + i);
                st.push(format!("mem.Store(({t}_a + {}u) & ~3u, {v});", 4 * i));
            } else {
                let v = self.vreg(f("VDST") + i);
                st.push(format!("{v} = mem.Load(({t}_a + {}u) & ~3u);", 4 * i));
            }
        }
        self.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
        Ok(())
    }

    /// FLAT / global atomics on the flat memory (dword ops; float min / max by a CAS loop).
    fn flat_atomic(&mut self, d: &Decoded, op: &str) -> Result<(), &'static str> {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let global = d.enc.name == "ENC_FLAT_GLBL";
        let addr = self.vreg(f("ADDR"));
        let saddr = f("SADDR");
        let base = if global && saddr != 0x7D && saddr != 0x7F { format!("{} + {addr}", self.sreg(saddr)) } else { addr };
        let raw = f("OFFSET") as u32;
        let off = if global { (((raw << 20) as i32) >> 20) as u32 } else { raw };
        let t = self.t();
        self.line(&format!("uint {t}_a = ({base} + {}) & ~3u;", hex(off)));
        let v = self.vreg(f("DATA"));
        let act = self.act();
        self.line(&format!("uint {t}_old = 0u;"));
        let body = match op {
            "add" => format!("mem.InterlockedAdd({t}_a, {v}, {t}_old);"),
            "sub" => format!("mem.InterlockedAdd({t}_a, 0u - {v}, {t}_old);"),
            "and" => format!("mem.InterlockedAnd({t}_a, {v}, {t}_old);"),
            "or" => format!("mem.InterlockedOr({t}_a, {v}, {t}_old);"),
            "xor" => format!("mem.InterlockedXor({t}_a, {v}, {t}_old);"),
            "swap" => format!("mem.InterlockedExchange({t}_a, {v}, {t}_old);"),
            "umin" => format!("mem.InterlockedMin({t}_a, {v}, {t}_old);"),
            "umax" => format!("mem.InterlockedMax({t}_a, {v}, {t}_old);"),
            "smin" => format!("int {t}_s; mem.InterlockedMin({t}_a, (int){v}, {t}_s); {t}_old = (uint){t}_s;"),
            "smax" => format!("int {t}_s; mem.InterlockedMax({t}_a, (int){v}, {t}_s); {t}_old = (uint){t}_s;"),
            "cmpswap" => {
                let cmp = self.vreg(f("DATA") + 1);
                format!("mem.InterlockedCompareExchange({t}_a, {cmp}, {v}, {t}_old);")
            }
            // float min / max as one integer atomic: non-negative floats order like signed
            // ints, negative ones like unsigned ints in reverse (no CAS loop – a program with
            // 168 such loops was too much for the driver's compiler)
            "fmin" | "fmax" => {
                let (pos, neg) = if op == "fmin" { ("InterlockedMin", "InterlockedMax") } else { ("InterlockedMax", "InterlockedMin") };
                format!(
                    "if (asfloat({v}) >= 0.0f) {{ int {t}_s; mem.{pos}({t}_a, asint({v}), {t}_s); {t}_old = asuint({t}_s); }} \
else {{ mem.{neg}({t}_a, {v}, {t}_old); }}"
                )
            }
            _ => return Err("FLAT atomic"),
        };
        self.line(&format!("if ({act}) {{ {body} }}"));
        if f("GLC") == 1 {
            let dst = self.vreg(f("VDST"));
            self.line(&format!("if ({act}) {dst} = {t}_old;"));
        }
        Ok(())
    }

    /// LDS (`groupshared`, compute only) and the DS cross-lane permutes.
    fn ds(&mut self, d: &Decoded, name: &str) -> Result<(), &'static str> {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let act = self.act();
        let t = self.t();
        let (o0, o1) = (f("OFFSET0"), f("OFFSET1"));
        let off16 = o0 | o1 << 8;
        match name {
            "ds_swizzle_b32" => {
                let pat = off16;
                let a = self.vreg(f("ADDR"));
                let src = if pat & 0x8000 != 0 {
                    format!("(lane_id & ~3u) | (({pat}u >> (2u * (lane_id & 3u))) & 3u)")
                } else if pat >> 14 == 0 {
                    let (and, or, xor) = (pat & 0x1F, (pat >> 5) & 0x1F, (pat >> 10) & 0x1F);
                    format!("(lane_id & 0x20u) | ((((lane_id & 0x1Fu) & {and}u) | {or}u) ^ {xor}u)")
                } else {
                    return Err("ds_swizzle: tryb FFT/rotacji");
                };
                self.line(&format!("uint {t} = rla({a}, {src});"));
                let dst = Dst::V(f("VDST"));
                self.write32(&dst, &t);
                return Ok(());
            }
            "ds_bpermute_b32" => {
                let (a, v) = (self.vreg(f("ADDR")), self.vreg(f("DATA0")));
                let m = if self.opt.wave64 { 63 } else { 31 };
                self.line(&format!("uint {t} = rla({v}, (({a} + {off16}u) >> 2) & {m}u);"));
                let dst = Dst::V(f("VDST"));
                self.write32(&dst, &t);
                return Ok(());
            }
            _ => {}
        }
        if !matches!(self.opt.mode, Mode::Kernel | Mode::Mesh(_) | Mode::Geometry(_) | Mode::Compute(_)) {
            return Err("LDS poza compute");
        }
        // geometry mode: where each GS lane puts its output vertices (lowest LDS address written)
        let gs_phase = match &self.opt.mode {
            Mode::Geometry(g) => g.gs_start.is_some_and(|s| d.pc >= s),
            _ => false,
        };
        self.uses_lds = true;
        // the address is read before any result is written (ds_read2_b32 v[0:1], v0 …)
        let addr = self.vreg(f("ADDR"));
        let a = format!("{t}_ad");
        self.line(&format!("uint {a} = {addr};"));
        let lds = |at: String| format!("xcl_lds[(({at}) >> 2) & 8191u]");
        let mut st = Vec::new();
        let rw = |n: &str| -> Option<(bool, u64, u64, bool)> {
            // (write?, dwords per element, stride of offsets (bytes), two elements?)
            Some(match n {
                "ds_read_b32" => (false, 1, 1, false),
                "ds_read_b64" => (false, 2, 1, false),
                "ds_read_b96" => (false, 3, 1, false),
                "ds_read_b128" => (false, 4, 1, false),
                "ds_write_b32" => (true, 1, 1, false),
                "ds_write_b64" => (true, 2, 1, false),
                "ds_write_b96" => (true, 3, 1, false),
                "ds_write_b128" => (true, 4, 1, false),
                "ds_read2_b32" => (false, 1, 4, true),
                "ds_read2_b64" => (false, 2, 8, true),
                "ds_read2st64_b32" => (false, 1, 256, true),
                "ds_read2st64_b64" => (false, 2, 512, true),
                "ds_write2_b32" => (true, 1, 4, true),
                "ds_write2_b64" => (true, 2, 8, true),
                "ds_write2st64_b32" => (true, 1, 256, true),
                "ds_write2st64_b64" => (true, 2, 512, true),
                _ => return None,
            })
        };
        // 8 / 16-bit LDS accesses: atomic and / or keep the rest of the dword intact
        let small = match name {
            "ds_write_b8" => Some((8u32, false, true)),
            "ds_write_b16" => Some((16, false, true)),
            "ds_read_u8" => Some((8, false, false)),
            "ds_read_i8" => Some((8, true, false)),
            "ds_read_u16" => Some((16, false, false)),
            "ds_read_i16" => Some((16, true, false)),
            _ => None,
        };
        if let Some((bits, signed, write)) = small {
            let at = format!("{a} + {off16}u");
            let slot = lds(at.clone());
            let sh = format!("((({at}) & 3u) * 8u)");
            let mask = (1u64 << bits) - 1;
            if write {
                let v = self.vreg(f("DATA0"));
                self.line(&format!(
                    "if ({act}) {{ InterlockedAnd({slot}, ~({mask}u << {sh})); InterlockedOr({slot}, ({v} & {mask}u) << {sh}); }}"
                ));
            } else {
                let v = self.vreg(f("VDST"));
                let val = if signed {
                    format!("(uint)(((int)((({slot} >> {sh}) & {mask}u) << {}u)) >> {}u)", 32 - bits, 32 - bits)
                } else {
                    format!("(({slot} >> {sh}) & {mask}u)")
                };
                self.line(&format!("if ({act}) {v} = {val};"));
            }
            return Ok(());
        }
        if let Some((write, n, stride, two)) = rw(name) {
            let addrs: Vec<String> = if two {
                vec![format!("{a} + {}u", o0 * stride), format!("{a} + {}u", o1 * stride)]
            } else {
                vec![format!("{a} + {off16}u")]
            };
            let (vd, d0, d1) = (f("VDST"), f("DATA0"), f("DATA1"));
            for (e, at) in addrs.iter().enumerate() {
                for k in 0..n {
                    let slot = lds(format!("{at} + {}u", 4 * k));
                    if write {
                        let src = self.vreg(if e == 0 { d0 } else { d1 } + k);
                        st.push(format!("{slot} = {src};"));
                    } else {
                        let v = self.vreg(vd + e as u64 * n + k);
                        st.push(format!("{v} = {slot};"));
                    }
                }
            }
            self.line(&format!("if ({act}) {{ {} }}", st.join(" ")));
            if write && gs_phase {
                let lowest = addrs.iter().map(|x| format!("({x})")).reduce(|p, q| format!("min({p}, {q})")).unwrap_or_default();
                self.line(&format!("if ({act}) xcl_gs_base = min(xcl_gs_base, {lowest});"));
            }
            return Ok(());
        }
        let (op, rtn) = match name.strip_prefix("ds_") {
            Some(r) if r.contains("_rtn_") => (r.replacen("_rtn", "", 1), true),
            Some(r) => (r.to_string(), false),
            None => return Err("DS"),
        };
        let func = match op.as_str() {
            "add_u32" => "InterlockedAdd",
            "min_u32" | "min_i32" => "InterlockedMin",
            "max_u32" | "max_i32" => "InterlockedMax",
            "and_b32" => "InterlockedAnd",
            "or_b32" => "InterlockedOr",
            "xor_b32" => "InterlockedXor",
            "wrxchg_b32" => "InterlockedExchange",
            "sub_u32" => "InterlockedAdd",
            _ => return Err("DS"),
        };
        if op.ends_with("_i32") {
            return Err("DS min/max i32");
        }
        let v = self.vreg(f("DATA0"));
        let val = if op == "sub_u32" { format!("(0u - {v})") } else { v };
        let slot = lds(format!("{a} + {off16}u"));
        self.line(&format!("uint {t}_o = 0u; if ({act}) {{ {func}({slot}, {val}, {t}_o); }}"));
        if rtn {
            let r = self.vreg(f("VDST"));
            self.line(&format!("if ({act}) {r} = {t}_o;"));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- whole shader

    pub fn finish(&self, body: &str) -> String {
        let mut h = String::new();
        h.push_str("// generated by XCL (RDNA2 -> HLSL), do not edit\n");
        h.push_str("RWByteAddressBuffer io : register(u0);\nRWByteAddressBuffer res : register(u1);\n");
        // compute work across workgroups (bottom-up BVH builds: atomics, then reading what
        // other groups wrote) relies on GLC / cache invalidation on the console: keep the flat
        // memory coherent across the GPU there; graphics keeps the cached (faster) view
        if matches!(self.opt.mode, Mode::Compute(_)) {
            h.push_str("globallycoherent RWByteAddressBuffer mem : register(u2);\n");
        } else {
            h.push_str("RWByteAddressBuffer mem : register(u2);\n");
        }
        h.push_str(PRELUDE);
        let infos: Vec<String> = (0..BUF_FORMATS.len()).map(|i| format!("0x{:08X}u", buf_format_info(i))).collect();
        let _ = writeln!(h, "static const uint FMT_INFO[78] = {{ {} }};", infos.join(", "));
        h.push_str(PRELUDE_MEM);
        let mut sg: BTreeSet<u64> = self.sgprs.clone();
        sg.extend([106, 107, 124, 126, 127]);
        let load_sgprs = |h: &mut String| {
            for &c in &sg {
                let _ = writeln!(h, "    uint {} = io.Load({}u);", sname(c).unwrap(), c * 4);
            }
        };
        match &self.opt.mode {
            Mode::Kernel => {
                // hw_wave 64 with a wave32 program: the wave-size independent form (two halves)
                let split = self.opt.hw_wave == 64 && !self.opt.wave64;
                let lanes = if self.opt.wave64 || split { 64 } else { 32 };
                if self.uses_lds {
                    h.push_str("groupshared uint xcl_lds[8192]; // D3D12 limit: 32 KB (the console allows 64 KB)\n");
                }
                let _ = writeln!(h, "[WaveSize({lanes})]\n[numthreads({lanes}, 1, 1)]\nvoid main(uint gi : SV_GroupIndex)\n{{");
                if split {
                    h.push_str("    uint xcl_hw_lane = WaveGetLaneIndex();\n    xcl_w32 = WaveGetLaneCount() > 32u;\n    xcl_half = xcl_w32 ? xcl_hw_lane >> 5 : 0u;\n    lane_id = xcl_hw_lane & 31u;\n");
                } else {
                    h.push_str("    uint xcl_hw_lane = WaveGetLaneIndex();\n    lane_id = xcl_hw_lane;\n");
                }
                load_sgprs(&mut h);
                let _ = writeln!(h, "    uint scc = io.Load({}u);", KERNEL_SCC_SLOT * 4);
                for &v in &self.vgprs {
                    let _ = writeln!(h, "    uint v{v} = io.Load(({}u + xcl_hw_lane * {}u + {v}u) * 4u);", KERNEL_SGPRS, KERNEL_VGPRS);
                }
                h.push_str(body);
                h.push_str("    if (xcl_hw_lane == 0u) {\n");
                for &c in &sg {
                    let _ = writeln!(h, "        res.Store({}u, {});", c * 4, sname(c).unwrap());
                }
                let _ = writeln!(h, "        res.Store({}u, scc);", KERNEL_SCC_SLOT * 4);
                h.push_str("    }\n");
                for &v in &self.vgprs {
                    let _ = writeln!(h, "    res.Store(({}u + xcl_hw_lane * {}u + {v}u) * 4u, v{v});", KERNEL_SGPRS, KERNEL_VGPRS);
                }
                h.push_str("}\n");
            }
            Mode::Vertex(vs) => {
                h.push_str("struct XclIn {\n");
                let inputs: Vec<&SigElement> = vs.inputs.iter().filter(|e| e.system_value == 0 && !e.name.starts_with("SV_")).collect();
                for e in &inputs {
                    let _ = writeln!(h, "    {}4 a{} : {};", hlsl_type(e), e.register, e.semantic());
                }
                h.push_str("    uint xcl_vid : SV_VertexID;\n    uint xcl_iid : SV_InstanceID;\n};\n");
                out_struct(&mut h, "XclOut", &vs.outputs);
                h.push_str("XclOut main(XclIn xi)\n{\n    XclOut xo = (XclOut)0;\n");
                h.push_str(if self.opt.wave64 { "    lane_id = WaveGetLaneIndex();\n" } else { "    uint xcl_hw_lane = WaveGetLaneIndex();\n    xcl_w32 = WaveGetLaneCount() > 32u;\n    xcl_half = xcl_w32 ? xcl_hw_lane >> 5 : 0u;\n    lane_id = xcl_hw_lane & 31u;\n" });
                load_sgprs(&mut h);
                h.push_str("    uint scc = 0u;\n    { uint2 e0 = ballot(true); exec_lo = e0.x; exec_hi = e0.y; }\n");
                for &v in &self.vgprs {
                    // SV_VertexID / SV_InstanceID listed in ISG1 come through the fetch shader like an
                    // attribute: v[base + 4r] (retail VInstanced* read the instance id in v8 = r1)
                    let sv_at = vs.inputs.iter().find(|e| matches!(e.system_value, 6 | 8) && v == vs.attr_base + 4 * e.register as u64);
                    let init = if v == 0 {
                        "xi.xcl_vid".to_string()
                    } else if v == 3 {
                        "xi.xcl_iid".to_string()
                    } else if let Some(e) = sv_at {
                        if e.system_value == 6 { "xi.xcl_vid" } else { "xi.xcl_iid" }.to_string()
                    } else {
                        inputs
                            .iter()
                            .find(|e| v >= vs.attr_base + 4 * e.register as u64 && v < vs.attr_base + 4 * e.register as u64 + 4)
                            .map(|e| format!("asuint(xi.a{}[{}])", e.register, v - vs.attr_base - 4 * e.register as u64))
                            .unwrap_or_else(|| "0u".into())
                    };
                    let _ = writeln!(h, "    uint v{v} = {init};");
                }
                h.push_str(body);
                h.push_str("    return xo;\n}\n");
            }
            Mode::Compute(cs) => {
                let lanes = if self.opt.wave64 { 64 } else { 32 };
                if self.uses_lds {
                    h.push_str("groupshared uint xcl_lds[8192]; // D3D12 limit: 32 KB (the console allows 64 KB)\n");
                }
                let [x, y, z] = cs.threads;
                let _ = writeln!(
                    h,
                    "[WaveSize({lanes})]\n[numthreads({x}, {y}, {z})]\nvoid main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID, uint gi : SV_GroupIndex)\n{{"
                );
                h.push_str("    lane_id = WaveGetLaneIndex();\n");
                load_sgprs(&mut h);
                // system SGPRs after the user data: enabled group ids, then TG_SIZE
                let mut next = cs.user_sgprs as u64;
                for (k, c) in ["x", "y", "z"].iter().enumerate() {
                    if cs.tgid[k] {
                        if sg.contains(&next) {
                            let _ = writeln!(h, "    {} = gid.{c};", sname(next).unwrap());
                        }
                        next += 1;
                    }
                }
                if cs.tg_size && sg.contains(&next) {
                    let _ = writeln!(h, "    {} = ({}u / {lanes}u) | ((gi / {lanes}u) << 6);", sname(next).unwrap(), x * y * z);
                }
                h.push_str("    uint scc = 0u;\n    { uint2 e0 = ballot(true); exec_lo = e0.x; exec_hi = e0.y; }\n");
                for &v in &self.vgprs {
                    let init = match v {
                        0 => "gtid.x",
                        1 if cs.tid_components >= 2 => "gtid.y",
                        2 if cs.tid_components >= 3 => "gtid.z",
                        _ => "0u",
                    };
                    let _ = writeln!(h, "    uint v{v} = {init};");
                }
                h.push_str(body);
                h.push_str("}\n");
            }
            Mode::Mesh(ms) => {
                let lanes = if self.opt.wave64 { 64 } else { 32 };
                if self.uses_lds {
                    h.push_str("groupshared uint xcl_lds[8192]; // D3D12 limit: 32 KB (the console allows 64 KB)\n");
                }
                let t = ms.threads;
                let _ = writeln!(h, "[WaveSize({lanes})]\n[numthreads({t}, 1, 1)]\nvoid main(uint gi : SV_GroupIndex, uint3 gid : SV_GroupID)\n{{");
                h.push_str("    lane_id = WaveGetLaneIndex();\n");
                // thread index in the group as the console numbers it: wave · lanes + lane
                let _ = writeln!(h, "    uint xcl_wave = gi / {lanes}u;\n    uint xcl_slot = xcl_wave * {lanes}u + lane_id;");
                let _ = writeln!(h, "    uint xcl_rec = gid.x * {}u;", ms.record_dwords());
                let _ = writeln!(h, "    uint xcl_vtx = xcl_rec + {MESH_HEADER}u + xcl_slot * {}u;", ms.vert_dwords());
                let _ = writeln!(h, "    uint xcl_prm = xcl_rec + {}u + xcl_slot;", MESH_HEADER + ms.max_vertices as usize * ms.vert_dwords());
                load_sgprs(&mut h);
                if sg.contains(&3) {
                    h.push_str("    s3 = (s3 & 0xF0FFFFFFu) | (xcl_wave << 24);\n");
                }
                h.push_str("    uint scc = 0u;\n    { uint2 e0 = ballot(true); exec_lo = e0.x; exec_hi = e0.y; }\n");
                for &v in &self.vgprs {
                    let init = if v == 5 { format!("gid.x * {t}u + xcl_slot") } else { "0u".into() };
                    let _ = writeln!(h, "    uint v{v} = {init};");
                }
                h.push_str(body);
                h.push_str("}\n");
            }
            Mode::Geometry(g) => {
                let pg = super::GS_PRIMS_PER_GROUP;
                let rec = g.record();
                let (vd, vb) = (rec.vert_dwords(), g.vertex_bytes());
                let (maxv, maxp) = (rec.max_vertices, rec.max_primitives);
                let maxt = g.max_vertex_count.saturating_sub(2).max(1);
                let nreg = g.input_registers();
                h.push_str("groupshared uint xcl_lds[8192]; // D3D12 limit: 32 KB (the console allows 64 KB)\n");
                h.push_str("[WaveSize(64)]\n[numthreads(64, 1, 1)]\nvoid main(uint gi : SV_GroupIndex, uint3 gid : SV_GroupID)\n{\n");
                h.push_str("    lane_id = WaveGetLaneIndex();\n");
                let _ = writeln!(h, "    uint xcl_total = io.Load({}u);\n    uint xcl_vin = io.Load({}u);", super::IO_GS_PRIMS * 4, super::IO_GS_VERTICES * 4);
                let _ = writeln!(h, "    uint xcl_p0 = gid.x * {pg}u;\n    uint xcl_np = xcl_total > xcl_p0 ? min({pg}u, xcl_total - xcl_p0) : 0u;\n    uint xcl_nv = 3u * xcl_np;");
                let _ = writeln!(h, "    uint xcl_rec = gid.x * {}u;", rec.record_dwords());
                let _ = writeln!(h, "    uint xcl_emitted = 0u, xcl_strip = 0u, xcl_ntri = 0u, xcl_gs_base = 0xFFFFFFFFu;\n    uint xcl_tri[{maxt}];");
                load_sgprs(&mut h);
                if sg.contains(&2) {
                    let _ = writeln!(h, "    s2 = {}u; // GS output area in LDS (after the ES items)", g.es_ring_bytes());
                }
                let item = g.es_item_bytes() / 4; // dwords per ES vertex
                if sg.contains(&3) {
                    h.push_str("    s3 = xcl_nv | (xcl_np << 8); // merged wave info: ES vertices, GS primitives, wave 0\n");
                }
                h.push_str("    uint scc = 0u;\n    { uint2 e0 = ballot(true); exec_lo = e0.x; exec_hi = e0.y; }\n");
                for &v in &self.vgprs {
                    let init = match v {
                        // LDS dword offsets of the triangle's vertices (ES vertex i at i · item)
                        0 => format!("(lane_id < xcl_np) ? ((lane_id * 3u * {item}u) | ((lane_id * 3u + 1u) * {item}u << 16)) : 0u"),
                        1 => format!("(lane_id < xcl_np) ? ((lane_id * 3u + 2u) * {item}u) : 0u"),
                        2 => "xcl_p0 + lane_id".to_string(),
                        5 => "xcl_p0 * 3u + lane_id".to_string(),
                        v if v >= 9 && v < 9 + 4 * nreg as u64 => {
                            let (r, c) = ((v - 9) / 4, (v - 9) % 4);
                            format!("(lane_id < xcl_nv) ? mem.Load(xcl_vin + ((xcl_p0 * 3u + lane_id) * {nreg}u + {r}u) * 16u + {}u) : 0u", c * 4)
                        }
                        _ => "0u".to_string(),
                    };
                    let _ = writeln!(h, "    uint v{v} = {init};");
                }
                h.push_str(body);
                // emitted vertices (LDS, register r at +16·r) and strip triangles → the record
                let pos_reg = g.outputs.iter().find(|e| e.system_value == 1).map(|e| e.register).unwrap_or(0);
                let regs: Vec<u32> = std::iter::once(pos_reg).chain(ranked(&g.outputs, true).iter().map(|e| e.register)).collect();
                h.push_str("    uint xcl_vb = WavePrefixSum(xcl_emitted);\n    uint xcl_tb = WavePrefixSum(xcl_ntri);\n");
                // wave sums with every lane active (not inside the lane-0 branch)
                h.push_str("    uint xcl_tv = WaveActiveSum(xcl_emitted);\n    uint xcl_tt = WaveActiveSum(xcl_ntri);\n");
                let _ = writeln!(h, "    if (lane_id == 0u) {{ res.Store(xcl_rec * 4u, min(xcl_tv, {maxv}u)); res.Store(xcl_rec * 4u + 4u, min(xcl_tt, {maxp}u)); }}");
                let _ = writeln!(h, "    for (uint k = 0u; k < xcl_emitted && xcl_gs_base != 0xFFFFFFFFu; k++) {{\n        uint vi = xcl_vb + k;\n        if (vi >= {maxv}u) break;");
                let _ = writeln!(h, "        uint src = xcl_gs_base + k * {vb}u;\n        uint dst = xcl_rec + {MESH_HEADER}u + vi * {vd}u;");
                for (slot, r) in regs.iter().enumerate() {
                    for c in 0..4 {
                        let _ = writeln!(h, "        res.Store((dst + {}u) * 4u, xcl_lds[((src + {}u) >> 2) & 8191u]);", 4 * slot + c, 16 * r + 4 * c as u32);
                    }
                }
                h.push_str("    }\n");
                let _ = writeln!(h, "    for (uint j = 0u; j < xcl_ntri; j++) {{\n        uint t = xcl_tri[j];\n        if (xcl_tb + j >= {maxp}u) break;");
                let _ = writeln!(
                    h,
                    "        res.Store((xcl_rec + {}u + xcl_tb + j) * 4u, ((t & 0x3FFu) + xcl_vb) | ((((t >> 10) & 0x3FFu) + xcl_vb) << 10) | ((((t >> 20) & 0x3FFu) + xcl_vb) << 20));\n    }}",
                    MESH_HEADER + maxv as usize * vd
                );
                h.push_str("}\n");
            }
            Mode::Pixel(ps) => {
                h.push_str("struct XclIn {\n    float4 xcl_pos : SV_Position;\n");
                for (k, e) in ranked(&ps.inputs, false).iter().enumerate() {
                    let mut m = self.ps_modes.get(&(k as u32)).cloned().unwrap_or_default();
                    if e.component_type != 3 {
                        m = "nointerpolation".into();
                    }
                    let _ = writeln!(h, "    {m} {}4 a{k} : {};", hlsl_type(e), e.semantic());
                }
                if ps.input_ena >> 12 & 1 == 1 {
                    h.push_str("    bool xcl_ff : SV_IsFrontFace;\n");
                }
                if ps.input_ena >> 14 & 1 == 1 {
                    h.push_str("    uint xcl_cov : SV_Coverage;\n");
                }
                h.push_str("};\nstruct XclOut {\n");
                for e in &ps.outputs {
                    match e.system_value {
                        64 => {
                            let _ = writeln!(h, "    {}4 t{} : SV_Target{};", hlsl_type(e), e.index, e.index);
                        }
                        65 => h.push_str("    float xcl_depth : SV_Depth;\n"),
                        67 => h.push_str("    float xcl_depth : SV_DepthGreaterEqual;\n"),
                        68 => h.push_str("    float xcl_depth : SV_DepthLessEqual;\n"),
                        _ => {}
                    }
                }
                if !ps.outputs.iter().any(|e| matches!(e.system_value, 64 | 65 | 67 | 68)) {
                    h.push_str("    float4 xcl_unused : SV_Target7;\n");
                }
                h.push_str("};\nXclOut main(XclIn xi)\n{\n    XclOut xo = (XclOut)0;\n    bool xcl_live = false;\n");
                h.push_str(if self.opt.wave64 { "    lane_id = WaveGetLaneIndex();\n" } else { "    uint xcl_hw_lane = WaveGetLaneIndex();\n    xcl_w32 = WaveGetLaneCount() > 32u;\n    xcl_half = xcl_w32 ? xcl_hw_lane >> 5 : 0u;\n    lane_id = xcl_hw_lane & 31u;\n" });
                load_sgprs(&mut h);
                h.push_str("    uint scc = 0u;\n    { uint2 e0 = ballot(true); exec_lo = e0.x; exec_hi = e0.y; }\n");
                let sys = |bit: u32| ps_input_vgpr(ps.input_ena, bit);
                for &v in &self.vgprs {
                    let init = if Some(v) == sys(8) {
                        "asuint(xi.xcl_pos.x)"
                    } else if Some(v) == sys(9) {
                        "asuint(xi.xcl_pos.y)"
                    } else if Some(v) == sys(10) {
                        "asuint(xi.xcl_pos.z)"
                    } else if Some(v) == sys(11) {
                        "asuint(xi.xcl_pos.w)"
                    } else if Some(v) == sys(12) {
                        "asuint(xi.xcl_ff ? 1.0f : 0.0f)"
                    } else if Some(v) == sys(14) {
                        "xi.xcl_cov"
                    } else {
                        "0u"
                    };
                    let _ = writeln!(h, "    uint v{v} = {init};");
                }
                h.push_str(body);
                if self.ps_done {
                    h.push_str("    if (!xcl_live) discard;\n");
                }
                h.push_str("    return xo;\n}\n");
            }
        }
        h
    }
}

/// Vertex output struct shared by the VS mode and the mesh pass-through: SV_Position, then
/// the params in `exp param` order (non-system OSG1 elements, then clip/cull distances).
fn out_struct(h: &mut String, name: &str, outputs: &[SigElement]) {
    let _ = writeln!(h, "struct {name} {{\n    float4 pos : SV_Position;");
    for (k, e) in ranked(outputs, true).iter().enumerate() {
        let n = out_comps(e);
        let ty = if n == 1 { hlsl_type(e).to_string() } else { format!("{}{n}", hlsl_type(e)) };
        let _ = writeln!(h, "    {ty} p{k} : {};", e.semantic());
    }
    h.push_str("};\n");
}

/// Second pass of the mesh mode: a mesh shader that turns the records written by the
/// translated program (one per group, see `MsInterface::record_dwords`) into vertices and
/// triangles. Null primitives (bit 31) and out-of-range indices are culled.
pub fn mesh_passthrough(ms: &MsInterface) -> String {
    let mut h = String::new();
    h.push_str("// generated by XCL: mesh pass-through for a translated NGG program\n");
    h.push_str("RWByteAddressBuffer io : register(u0);\nRWByteAddressBuffer res : register(u1);\n");
    out_struct(&mut h, "XclVert", &ms.outputs);
    h.push_str("struct XclPrim { bool cull : SV_CullPrimitive; };\n");
    let (mv, mp, vd, rec) = (ms.max_vertices, ms.max_primitives, ms.vert_dwords(), ms.record_dwords());
    let t = ms.threads.clamp(1, 128);
    let _ = writeln!(
        h,
        "[outputtopology(\"triangle\")]\n[numthreads({t}, 1, 1)]\nvoid main(uint gi : SV_GroupIndex, uint3 gid : SV_GroupID, out vertices XclVert verts[{mv}], out indices uint3 tris[{mp}], out primitives XclPrim prims[{mp}])\n{{"
    );
    let _ = writeln!(h, "    uint rec = gid.x * {rec}u;");
    let _ = writeln!(h, "    uint nv = min(res.Load(rec * 4u), {mv}u);\n    uint np = min(res.Load(rec * 4u + 4u), {mp}u);");
    h.push_str("    SetMeshOutputCounts(nv, np);\n");
    let _ = writeln!(h, "    for (uint i = gi; i < nv; i += {t}u) {{");
    let _ = writeln!(h, "        uint b = (rec + {MESH_HEADER}u + i * {vd}u) * 4u;");
    h.push_str("        XclVert o;\n        o.pos = asfloat(res.Load4(b));\n");
    for (k, e) in ranked(&ms.outputs, true).iter().enumerate() {
        let n = out_comps(e);
        let sw = ["x", "y", "z", "w"][..n].concat();
        let raw = format!("res.Load4(b + {}u).{sw}", 16 * (k + 1));
        let v = match e.component_type {
            1 => raw,
            2 => format!("asint({raw})"),
            _ => format!("asfloat({raw})"),
        };
        let _ = writeln!(h, "        o.p{k} = {v};");
    }
    h.push_str("        verts[i] = o;\n    }\n");
    let _ = writeln!(h, "    for (uint j = gi; j < np; j += {t}u) {{");
    let _ = writeln!(h, "        uint p = res.Load((rec + {}u + j) * 4u);", MESH_HEADER + mv as usize * vd);
    h.push_str("        uint3 x = uint3(p & 0x3FFu, (p >> 10) & 0x3FFu, (p >> 20) & 0x3FFu);\n");
    h.push_str("        bool bad = (p >> 31) != 0u || any(x >= nv);\n");
    h.push_str("        tris[j] = bad ? uint3(0u, 0u, 0u) : x;\n        XclPrim q; q.cull = bad; prims[j] = q;\n    }\n}\n");
    h
}

fn hlsl_type(e: &SigElement) -> &'static str {
    match e.component_type {
        1 => "uint",
        2 => "int",
        _ => "float",
    }
}

/// Components of an output element: clip/cull distances by their mask, the rest as 4-vectors.
fn out_comps(e: &SigElement) -> usize {
    if matches!(e.system_value, 2 | 3) {
        (8 - e.mask.leading_zeros()) as usize
    } else {
        4
    }
}

/// Interpolation modifier of every PS attribute from the I/J pair its v_interp uses.
pub fn ps_attr_modes(insts: &[Decoded], ena: u32) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for d in insts {
        let name = d.inst.name.to_ascii_lowercase();
        if !name.starts_with("v_interp_") {
            continue;
        }
        let (_, ij, attr, _) = Em::interp_fields(d);
        let mode = if name == "v_interp_mov_f32" {
            "nointerpolation"
        } else {
            let pair = [(0u32, "sample"), (1, ""), (2, "centroid"), (4, "noperspective sample"), (5, "noperspective"), (6, "noperspective centroid")]
                .into_iter()
                .find(|&(bit, _)| ps_input_vgpr(ena, bit).is_some_and(|b| ij == b || ij == b + 1));
            match pair {
                Some((_, m)) => m,
                None => continue,
            }
        };
        out.entry(attr as u32).or_insert_with(|| mode.to_string());
    }
    out
}

/// `lt` → ("<", signed) for s_cmp_* / s_cmpk_* suffixes like "eq_i32".
fn cmp_op(rest: &str) -> Option<(&'static str, bool)> {
    let (op, ty) = rest.rsplit_once('_')?;
    let signed = ty == "i32";
    Some((
        match op {
            "eq" => "==",
            "lg" => "!=",
            "gt" => ">",
            "ge" => ">=",
            "lt" => "<",
            "le" => "<=",
            _ => return None,
        },
        signed,
    ))
}

const PRELUDE_MEM: &str = r#"
uint4 ld_raw(uint a) {
    uint b = a & ~3u; uint s = (a & 3u) * 8u;
    uint w0 = mem.Load(b), w1 = mem.Load(b + 4u), w2 = mem.Load(b + 8u), w3 = mem.Load(b + 12u), w4 = mem.Load(b + 16u);
    if (s == 0u) return uint4(w0, w1, w2, w3);
    return uint4((w0 >> s) | (w1 << (32u - s)), (w1 >> s) | (w2 << (32u - s)), (w2 >> s) | (w3 << (32u - s)), (w3 >> s) | (w4 << (32u - s)));
}
uint bits_at(uint4 r, uint off, uint w) {
    uint i = off >> 5; uint s = off & 31u;
    uint lo = r[i]; uint hi = (i < 3u) ? r[i + 1u] : 0u;
    uint v = (s == 0u) ? lo : ((lo >> s) | (hi << (32u - s)));
    return w >= 32u ? v : (v & ((1u << w) - 1u));
}
float uf_small(uint v, uint mb) {
    uint e = v >> mb; uint m = v & ((1u << mb) - 1u); float fm = (float)m / (float)(1u << mb);
    if (e == 31u) return m != 0u ? asfloat(0x7FC00000u) : asfloat(0x7F800000u);
    if (e == 0u) return fm * exp2(-14.0f);
    return exp2((float)e - 15.0f) * (1.0f + fm);
}
uint fmt_bytes(uint info) { uint n = 0u; [unroll] for (uint k = 0u; k < 4u; k++) n += (info >> (8u + 6u * k)) & 63u; return n / 8u; }
uint fmt_comp(uint4 r, uint info, uint c) {
    uint n = info & 7u; uint ty = (info >> 3) & 7u;
    bool isint = ty == 4u || ty == 5u;
    if (c >= n) return c == 3u ? (isint ? 1u : 0x3F800000u) : 0u;
    uint off = 0u; [unroll] for (uint k = 0u; k < 3u; k++) if (k < c) off += (info >> (8u + 6u * k)) & 63u;
    uint w = (info >> (8u + 6u * c)) & 63u;
    uint v = bits_at(r, off, w);
    int sv = w >= 32u ? (int)v : (((int)(v << (32u - w))) >> (32u - w));
    float maxu = w >= 32u ? 4294967295.0f : (float)((1u << w) - 1u);
    float maxs = w >= 32u ? 2147483647.0f : (float)((1u << (w - 1u)) - 1u);
    // exact endpoints (the hardware conversion is exact; DXC divides approximately)
    if (ty == 0u) return asuint(v == 0u ? 0.0f : ((float)v >= maxu ? 1.0f : (float)v / maxu));
    if (ty == 1u) return asuint(sv == 0 ? 0.0f : ((float)sv >= maxs ? 1.0f : max((float)sv / maxs, -1.0f)));
    if (ty == 2u) return asuint((float)v);
    if (ty == 3u) return asuint((float)sv);
    if (ty == 4u) return v;
    if (ty == 5u) return (uint)sv;
    return w == 32u ? v : (w == 16u ? asuint(f16tof32(v)) : asuint(uf_small(v, w == 11u ? 6u : 5u)));
}
// image_bvh(64)_intersect_ray: node = id (low 3 bits: type), at base + (id & ~7) * 8.
// box32 (5) / box16 (4): 4 child ids + boxes → hit children (closest first when sorting),
// misses 0xFFFFFFFF; triangle (0..3): (t numerator, denominator, i, j numerators), miss t = inf;
// instance / AABB nodes are left to the shader (all 0xFFFFFFFF).
uint4 bvh_intersect(uint node, uint base, float tmax, float3 o, float3 d, float3 inv, bool sort) {
    uint type = node & 7u;
    uint at = base + ((node & ~7u) << 3);
    if (type == 4u || type == 5u) {
        uint4 child = mem.Load4(at);
        float tn[4]; uint id[4];
        [unroll] for (uint k = 0u; k < 4u; k++) {
            float3 mn, mx;
            if (type == 5u) { mn = asfloat(mem.Load3(at + 16u + 24u * k)); mx = asfloat(mem.Load3(at + 28u + 24u * k)); }
            else { uint3 h = mem.Load3(at + 16u + 12u * k); mn = float3(f16tof32(h.x), f16tof32(h.x >> 16), f16tof32(h.y)); mx = float3(f16tof32(h.y >> 16), f16tof32(h.z), f16tof32(h.z >> 16)); }
            float3 t1 = (mn - o) * inv, t2 = (mx - o) * inv;
            float a = max(max(min(t1.x, t2.x), min(t1.y, t2.y)), min(t1.z, t2.z));
            float b = min(min(max(t1.x, t2.x), max(t1.y, t2.y)), max(t1.z, t2.z));
            bool hit = child[k] != 0xFFFFFFFFu && a <= b && b >= 0.0f && a <= tmax;
            tn[k] = hit ? a : asfloat(0x7F800000u);
            id[k] = hit ? child[k] : 0xFFFFFFFFu;
        }
        if (sort) {
            // sorting network for 4: (0,1) (2,3) (0,2) (1,3) (1,2)
            uint2 pairs[5] = { uint2(0, 1), uint2(2, 3), uint2(0, 2), uint2(1, 3), uint2(1, 2) };
            [unroll] for (uint p = 0u; p < 5u; p++) {
                uint i = pairs[p].x, j = pairs[p].y;
                if (tn[j] < tn[i]) { float ft = tn[i]; tn[i] = tn[j]; tn[j] = ft; uint ui = id[i]; id[i] = id[j]; id[j] = ui; }
            }
        }
        return uint4(id[0], id[1], id[2], id[3]);
    }
    if (type <= 3u) {
        // GPURT gfx10 node: a pair of triangles, type 0 = (v0, v1, v2), type 1 = (v1, v3, v2)
        bool t1 = type == 1u;
        float3 v0 = asfloat(mem.Load3(at + (t1 ? 12u : 0u))), v1 = asfloat(mem.Load3(at + (t1 ? 36u : 12u))), v2 = asfloat(mem.Load3(at + 24u));
        float3 e1 = v1 - v0, e2 = v2 - v0, p = cross(d, e2);
        float det = dot(e1, p);
        float3 sv = o - v0, q = cross(sv, e1);
        float i = dot(sv, p), j = dot(d, q), t = dot(e2, q);
        // inside when i, j, 1 - i - j >= 0 relative to det (either winding)
        bool hit = det != 0.0f && i * det >= 0.0f && j * det >= 0.0f && (i + j) * det <= det * det;
        // barycentrics back to the original vertex order: triangle id @60, per triangle I / J source
        float bc[4] = { det - i - j, i, j, 0.0f };
        uint tid = mem.Load(at + 60u) >> (8u * type);
        return hit ? uint4(asuint(t), asuint(det), asuint(bc[tid & 3u]), asuint(bc[(tid >> 2) & 3u])) : uint4(0x7F800000u, asuint(1.0f), 0u, 0u);
    }
    return uint4(0xFFFFFFFFu, 0xFFFFFFFFu, 0xFFFFFFFFu, 0xFFFFFFFFu);
}
uint img_cls(uint fmt) { uint ty = fmt <= 77u ? ((FMT_INFO[fmt] >> 3) & 7u) : 6u; return ty == 4u ? 1u : (ty == 5u ? 2u : 0u); }
void put_bits(inout uint4 r, uint off, uint w, uint v) {
    uint m = w >= 32u ? 0xFFFFFFFFu : ((1u << w) - 1u); v &= m;
    uint i = off >> 5; uint s = off & 31u;
    r[i] |= v << s;
    if (s != 0u && s + w > 32u && i < 3u) r[i + 1u] |= v >> (32u - s);
}
uint fmt_encode(uint x, uint info, uint c) {
    uint ty = (info >> 3) & 7u; uint w = (info >> (8u + 6u * c)) & 63u; float f = asfloat(x);
    float maxu = w >= 32u ? 4294967295.0f : (float)((1u << w) - 1u);
    float maxs = w >= 32u ? 2147483647.0f : (float)((1u << (w - 1u)) - 1u);
    if (ty == 0u) return (uint)round(saturate(f) * maxu);
    if (ty == 1u) return (uint)(int)round(clamp(f, -1.0f, 1.0f) * maxs);
    if (ty == 2u) return (uint)f;
    if (ty == 3u) return (uint)(int)f;
    if (ty == 4u || ty == 5u || w == 32u) return x;
    uint h = f32tof16(f);
    return w == 16u ? h : (w == 11u ? ((h >> 4) & 0x7FFu) : ((h >> 5) & 0x3FFu));
}
uint4 fmt_pack(uint4 v, uint info) {
    uint4 r = uint4(0u, 0u, 0u, 0u); uint n = info & 7u; uint off = 0u;
    [unroll] for (uint c = 0u; c < 4u; c++) { if (c < n) { uint w = (info >> (8u + 6u * c)) & 63u; put_bits(r, off, w, fmt_encode(v[c], info, c)); off += w; } }
    return r;
}
void st_part(uint b, uint m, uint v) { mem.InterlockedAnd(b, ~m); mem.InterlockedOr(b, v & m); }
void st_bytes(uint a, uint4 r, uint n) {
    uint s = (a & 3u) * 8u; uint b = a & ~3u;
    if (s == 0u && (n & 3u) == 0u) { [unroll] for (uint k = 0u; k < 4u; k++) if (k * 4u < n) mem.Store(b + 4u * k, r[k]); return; }
    uint m = n >= 4u ? 0xFFFFFFFFu : ((1u << (8u * n)) - 1u);
    st_part(b, m << s, r.x << s);
    if (s != 0u && s + 8u * n > 32u) st_part(b + 4u, m >> (32u - s), r.x >> (32u - s));
}
uint dst_sel(uint4 c, uint sel, bool isint) { return sel == 0u ? 0u : (sel == 1u ? (isint ? 1u : 0x3F800000u) : c[(sel - 4u) & 3u]); }
"#;

const PRELUDE: &str = r#"
static uint lane_id;
// wave32 program in a 64-lane hardware wave: each half is its own wave (xcl_half = 0 / 1)
static bool xcl_w32 = false;
static uint xcl_half = 0u;
bool lanebit(uint lo, uint hi) { return lane_id < 32u ? ((lo >> lane_id) & 1u) != 0u : ((hi >> (lane_id - 32u)) & 1u) != 0u; }
uint2 ballot(bool b) { uint4 m = WaveActiveBallot(b); return xcl_w32 ? uint2(xcl_half != 0u ? m.y : m.x, 0u) : m.xy; }
uint rla(uint v, uint l) { return WaveReadLaneAt(v, l + 32u * xcl_half); }
bool lanebit_at(uint lo, uint hi, uint l) { return l < 32u ? ((lo >> l) & 1u) != 0u : ((hi >> (l - 32u)) & 1u) != 0u; }
uint sdwa_sel(uint x, uint sel, bool sext) {
    if (sel >= 6u) return x;
    uint sh = sel < 4u ? 8u * sel : 16u * (sel - 4u); uint w = sel < 4u ? 8u : 16u;
    uint v = (x >> sh) & ((1u << w) - 1u);
    return sext ? (uint)(((int)(v << (32u - w))) >> (32u - w)) : v;
}
uint sdwa_dst(uint r, uint old, uint sel, uint unused) {
    if (sel >= 6u) return r;
    uint sh = sel < 4u ? 8u * sel : 16u * (sel - 4u); uint w = sel < 4u ? 8u : 16u;
    uint m = ((1u << w) - 1u) << sh; uint v = (r << sh) & m;
    if (unused == 2u) return (old & ~m) | v;
    if (unused == 1u) { uint top = sh + w; bool s = ((r >> (w - 1u)) & 1u) != 0u; return v | ((s && top < 32u) ? (0xFFFFFFFFu << top) : 0u); }
    return v;
}
float cube_id(float x, float y, float z) { if (abs(z) >= abs(x) && abs(z) >= abs(y)) return z < 0.0f ? 5.0f : 4.0f; if (abs(y) >= abs(x)) return y < 0.0f ? 3.0f : 2.0f; return x < 0.0f ? 1.0f : 0.0f; }
float cube_sc(float x, float y, float z) { if (abs(z) >= abs(x) && abs(z) >= abs(y)) return z < 0.0f ? -x : x; if (abs(y) >= abs(x)) return x; return x < 0.0f ? z : -z; }
float cube_tc(float x, float y, float z) { if (abs(z) >= abs(x) && abs(z) >= abs(y)) return -y; if (abs(y) >= abs(x)) return y < 0.0f ? -z : z; return -y; }
float cube_ma(float x, float y, float z) { if (abs(z) >= abs(x) && abs(z) >= abs(y)) return 2.0f * z; if (abs(y) >= abs(x)) return 2.0f * y; return 2.0f * x; }
// AMD cube coordinates (sc/|2ma| + 1.5, tc/|2ma| + 1.5, face) back to a direction
float3 cube_dir(float s, float t, float face) {
    float u = (s - 1.5f) * 2.0f; float v = (t - 1.5f) * 2.0f; uint f = ((uint)round(face)) % 8u;
    if (f == 0u) return float3(1.0f, -v, -u);
    if (f == 1u) return float3(-1.0f, -v, u);
    if (f == 2u) return float3(u, 1.0f, v);
    if (f == 3u) return float3(u, -1.0f, -v);
    if (f == 4u) return float3(u, -v, 1.0f);
    return float3(-u, -v, -1.0f);
}
uint first_lane(uint lo, uint hi) { return lo != 0u ? firstbitlow(lo) : (hi != 0u ? 32u + firstbitlow(hi) : 0u); }
uint lanemask_lt_lo() { return lane_id >= 32u ? 0xFFFFFFFFu : (lane_id == 0u ? 0u : (0xFFFFFFFFu >> (32u - lane_id))); }
uint lanemask_lt_hi() { return lane_id <= 32u ? 0u : (0xFFFFFFFFu >> (64u - lane_id)); }
uint wqm(uint m) { uint q = m | (m >> 1); q = q | (q >> 2); q &= 0x11111111u; return q * 15u; }
float mul_legacy(float a, float b) { return (a == 0.0f || b == 0.0f) ? 0.0f : a * b; }
float fma_legacy(float a, float b, float c) { return (a == 0.0f || b == 0.0f) ? c : mad(a, b, c); }
float mad_nf(float a, float b, float c) { precise float p = a * b; precise float r = p + c; return r; }
float med3f(float a, float b, float c) { return max(min(a, b), min(max(a, b), c)); }
uint med3u(uint a, uint b, uint c) { return max(min(a, b), min(max(a, b), c)); }
int med3i(int a, int b, int c) { return max(min(a, b), min(max(a, b), c)); }
float fract_f(float x) { return min(x - floor(x), asfloat(0x3F7FFFFFu)); }
uint cvt_u32(float f) { return (isnan(f) || f <= 0.0f) ? 0u : (f >= 4294967296.0f ? 0xFFFFFFFFu : (uint)f); }
int cvt_i32(float f) { return isnan(f) ? 0 : (f <= -2147483648.0f ? (int)0x80000000u : (f >= 2147483648.0f ? 0x7FFFFFFF : (int)f)); }
int i24(uint v) { return ((int)(v << 8)) >> 8; }
uint bfe_u(uint v, uint o, uint w) { o &= 31u; w &= 31u; return w == 0u ? 0u : ((v >> o) & (0xFFFFFFFFu >> (32u - w))); }
int bfe_i(uint v, uint o, uint w) { o &= 31u; w &= 31u; if (w == 0u) return 0; uint s = 32u - w; return ((int)((v >> o) << s)) >> s; }
uint sbfe_u(uint v, uint c) { uint o = c & 31u; uint w = (c >> 16) & 0x7Fu; return w == 0u ? 0u : (w >= 32u ? (v >> o) : ((v >> o) & (0xFFFFFFFFu >> (32u - w)))); }
int sbfe_i(uint v, uint c) { uint o = c & 31u; uint w = (c >> 16) & 0x7Fu; if (w == 0u) return 0; if (w >= 32u) return ((int)v) >> o; uint s = 32u - w; return ((int)((v >> o) << s)) >> s; }
uint ffbh_i(uint v) { uint x = ((int)v < 0) ? ~v : v; return x == 0u ? 0xFFFFFFFFu : 31u - firstbithigh(x); }
uint alignbit(uint s0, uint s1, uint s) { s &= 31u; return s == 0u ? s1 : ((s1 >> s) | (s0 << (32u - s))); }
uint perm_b32(uint s0, uint s1, uint sel) {
    uint r = 0u;
    [unroll] for (uint i = 0u; i < 4u; i++) {
        uint k = (sel >> (8u * i)) & 0xFFu; uint b;
        if (k >= 13u) b = 0xFFu; else if (k == 12u) b = 0u;
        else if (k >= 8u) { uint s = (k == 8u) ? (s1 >> 15) : ((k == 9u) ? (s1 >> 31) : ((k == 10u) ? (s0 >> 15) : (s0 >> 31))); b = (s & 1u) != 0u ? 0xFFu : 0u; }
        else b = ((k < 4u) ? (s1 >> (8u * k)) : (s0 >> (8u * (k - 4u)))) & 0xFFu;
        r |= b << (8u * i);
    }
    return r;
}
uint f16_rtz(float f) { uint h = f32tof16(f) & 0xFFFFu; if (!isnan(f) && (h & 0x7FFFu) != 0u && abs(f16tof32(h)) > abs(f)) h -= 1u; return h; }
float frexp_mant(float x) { float e; return (isinf(x) || isnan(x)) ? x : frexp(x, e); }
int frexp_exp(float x) { float e; frexp(x, e); return (isinf(x) || isnan(x)) ? 0 : (int)e; }
double floor_d(double x) { double t = (double)(int64_t)x; return t > x ? t - 1.0 : t; }
uint dot4_u8(uint a, uint b, uint c) { uint r = c; [unroll] for (uint i = 0u; i < 32u; i += 8u) r += ((a >> i) & 0xFFu) * ((b >> i) & 0xFFu); return r; }
uint dot4_i8(uint a, uint b, uint c) { int r = (int)c; [unroll] for (uint i = 0u; i < 32u; i += 8u) r += (((int)(a << (24u - i))) >> 24) * (((int)(b << (24u - i))) >> 24); return (uint)r; }
uint dot8_u4(uint a, uint b, uint c) { uint r = c; [unroll] for (uint i = 0u; i < 32u; i += 4u) r += ((a >> i) & 0xFu) * ((b >> i) & 0xFu); return r; }
uint dot8_i4(uint a, uint b, uint c) { int r = (int)c; [unroll] for (uint i = 0u; i < 32u; i += 4u) r += (((int)(a << (28u - i))) >> 28) * (((int)(b << (28u - i))) >> 28); return (uint)r; }
uint fclass16(uint h) {
    uint e = (h >> 10) & 0x1Fu; uint m = h & 0x3FFu; bool n = (h & 0x8000u) != 0u;
    if (e == 0x1Fu) return m != 0u ? (((m & 0x200u) != 0u) ? 1u : 0u) : (n ? 2u : 9u);
    if (e == 0u) return m == 0u ? (n ? 5u : 6u) : (n ? 4u : 7u);
    return n ? 3u : 8u;
}
uint fclass(float f) {
    uint u = asuint(f); uint e = (u >> 23) & 0xFFu; uint m = u & 0x7FFFFFu; bool n = (u >> 31) != 0u;
    if (e == 0xFFu) return m != 0u ? (((m & 0x400000u) != 0u) ? 1u : 0u) : (n ? 2u : 9u);
    if (e == 0u) return m == 0u ? (n ? 5u : 6u) : (n ? 4u : 7u);
    return n ? 3u : 8u;
}
"#;
