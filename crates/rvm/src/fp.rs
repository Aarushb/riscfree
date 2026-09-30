//! IEEE-754 semantics for the F/D extensions: NaN boxing, the five rounding
//! modes, and the fflags exceptions.
//!
//! Strategy: the host always computes in round-to-nearest-even, so every
//! non-RNE mode is derived from an *exact* picture of the result.
//!
//! * 32-bit ops bridge through f64, where the sum/difference/product of two
//!   f32 values is exact; the f64 value is then rounded onto the f32 grid
//!   with the requested mode. Division and square root keep their f64
//!   quotient, which is at most half an f64 ulp from the true result — a
//!   pathological operand pair can in principle double-round at the f32
//!   boundary (documented residual risk; directed tests avoid such cases).
//! * 64-bit ops use exact decompositions: Knuth two-sum for add/sub, an
//!   FMA-based two-product for mul, and an FMA residual for div/sqrt. The
//!   mode step is then a single nextafter decision, exact for RTZ/RDN/RUP.
//!   RMM on 64-bit div/sqrt falls back to RNE because tie detection would
//!   need the residual magnitude to full FMA precision.

/// fflags bits (RISC-V ordering).
pub(crate) const NX: u8 = 1 << 0; // inexact
pub(crate) const UF: u8 = 1 << 1; // underflow
pub(crate) const OF: u8 = 1 << 2; // overflow
pub(crate) const DZ: u8 = 1 << 3; // divide by zero
pub(crate) const NV: u8 = 1 << 4; // invalid

/// Rounding mode field values, per the spec encoding.
pub(crate) const RNE: u8 = 0;
pub(crate) const RTZ: u8 = 1;
pub(crate) const RDN: u8 = 2;
pub(crate) const RUP: u8 = 3;
pub(crate) const RMM: u8 = 4;
pub(crate) const DYN: u8 = 7;

pub(crate) const F32_NAN: u32 = 0x7fc0_0000;
pub(crate) const F64_NAN: u64 = 0x7ff8_0000_0000_0000;
const F32_SIGN: u32 = 0x8000_0000;
/// NaN boxing: a 32-bit FP value lives in a 64-bit register with every
/// upper bit set.
const F32_BOX: u64 = 0xffff_ffff_0000_0000;

/// Read a 32-bit FP result out of a register word. If the upper 32 bits are
/// not all ones the register was never written by a 32-bit op, and the spec
/// says to treat the value as a canonical NaN.
pub(crate) fn single_bits(freg: u64) -> u32 {
    if (freg >> 32) == 0xffff_ffff {
        freg as u32
    } else {
        F32_NAN
    }
}

/// Store a 32-bit FP result into a register word (NaN-boxed).
pub(crate) fn box_single(bits: u32) -> u64 {
    F32_BOX | bits as u64
}

/// Resolve an encoded rm field: 7 (DYN) reads the frm CSR; 5/6 are reserved.
pub(crate) fn resolve_rm(rm_field: u32, frm: u64) -> Option<u8> {
    let rm = if rm_field == DYN as u32 {
        (frm & 0x7) as u8
    } else {
        rm_field as u8
    };
    match rm {
        RNE | RTZ | RDN | RUP | RMM => Some(rm),
        _ => None,
    }
}

fn f32_is_nan(b: u32) -> bool {
    b & 0x7f80_0000 == 0x7f80_0000 && b & 0x007f_ffff != 0
}

/// A signaling NaN has the quiet bit clear.
pub(crate) fn f32_is_snan(b: u32) -> bool {
    f32_is_nan(b) && b & 0x0040_0000 == 0
}

fn f64_is_nan(b: u64) -> bool {
    b & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && b & 0x000f_ffff_ffff_ffff != 0
}

pub(crate) fn f64_is_snan(b: u64) -> bool {
    f64_is_nan(b) && b & 0x0008_0000_0000_0000 == 0
}

fn subnormal32(bits: u32) -> bool {
    bits & 0x7f80_0000 == 0 && bits & 0x007f_ffff != 0
}

fn subnormal64(bits: u64) -> bool {
    bits & 0x7ff0_0000_0000_0000 == 0 && bits & 0x000f_ffff_ffff_ffff != 0
}

/// Raise underflow alongside inexactness when the rounded result is tiny
/// (RISC-V detects tininess after rounding).
fn tiny_flags(subnormal: bool, flags: u8) -> u8 {
    if subnormal && flags & NX != 0 {
        flags | UF
    } else {
        flags
    }
}

/// Round the exact value `p + e` — the native RNE result `p` plus its exact
/// residual `e`, |e| <= half an ulp of p — to f64 under `rm`, returning the
/// bit pattern plus NX/OF.
pub(crate) fn round_exact_to_f64(p: f64, e: f64, rm: u8) -> (u64, u8) {
    debug_assert!(!p.is_nan());
    if p.is_infinite() {
        // Native arithmetic overflowed: the exact result is beyond the
        // largest finite, so overflow together with inexactness.
        return (p.to_bits(), NX | OF);
    }
    let mut flags = 0u8;
    if e != 0.0 {
        flags |= NX;
    }
    // Exact result beyond the largest finite in magnitude: native RNE would
    // already have returned infinity, so reaching here means the mode keeps
    // a finite value (RTZ/RDN) or rounds to infinity (RUP) — softfloat
    // raises OF|NX in all of those.
    if (p == f64::MAX && e > 0.0) || (p == f64::MIN && e < 0.0) {
        flags |= NX | OF;
    }
    let r = match rm {
        RNE => p,
        // Toward zero: step off the RNE result only when the exact value is
        // on the zero side of it.
        RTZ => {
            if e != 0.0 && e.is_sign_negative() != p.is_sign_negative() {
                step_zero64(p)
            } else {
                p
            }
        }
        RDN => {
            if e < 0.0 {
                p.next_down()
            } else {
                p
            }
        }
        RUP => {
            if e > 0.0 {
                p.next_up()
            } else {
                p
            }
        }
        RMM => {
            if e == 0.0 {
                p
            } else {
                let half = (p.next_up() - p) / 2.0;
                // At an exact midpoint the away-from-zero neighbor is on the
                // side e points to — but only when that side is away from
                // zero; otherwise p itself is the away candidate.
                if e.abs() == half && e.is_sign_negative() == p.is_sign_negative() {
                    if p < 0.0 {
                        p.next_down()
                    } else {
                        p.next_up()
                    }
                } else {
                    p
                }
            }
        }
        _ => p,
    };
    let bits = r.to_bits();
    (bits, tiny_flags(subnormal64(bits), flags))
}

fn step_zero64(p: f64) -> f64 {
    if p < 0.0 {
        p.next_up()
    } else {
        p.next_down()
    }
}

pub(crate) fn inf32(neg: bool) -> u32 {
    if neg {
        0xff80_0000
    } else {
        0x7f80_0000
    }
}

fn zero32(neg: bool) -> u32 {
    if neg {
        F32_SIGN
    } else {
        0
    }
}

fn inf64(neg: bool) -> u64 {
    if neg {
        0xfff0_0000_0000_0000
    } else {
        0x7ff0_0000_0000_0000
    }
}

fn zero64(neg: bool) -> u64 {
    if neg {
        0x8000_0000_0000_0000
    } else {
        0
    }
}

/// Knuth two-sum: `s + err` reconstructs `a + b` exactly.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bp = s - a;
    let ap = s - bp;
    (s, (a - ap) + (b - bp))
}

/// Binary FP operations. Inputs/outputs are bit patterns; the caller does
/// the NaN-boxed register reads and writes.
#[derive(Clone, Copy)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// Fused multiply-add variants per the four opcodes. `Fnmadd` computes
/// −(a·b) − c and `Fnmsub` computes −(a·b) + c.
#[derive(Clone, Copy)]
pub(crate) enum FmaKind {
    Fmadd,
    Fmsub,
    Fnmadd,
    Fnmsub,
}

impl FmaKind {
    /// Sign applied to the addend inside the fused expression, and whether
    /// the whole result is negated afterwards.
    fn adjust(self) -> (bool, bool) {
        match self {
            FmaKind::Fmadd => (false, false),
            FmaKind::Fmsub => (true, false),
            // −(ab − c) and −(ab + c): flip the addend, flip the result.
            FmaKind::Fnmsub => (true, true),
            FmaKind::Fnmadd => (false, true),
        }
    }
}

/// Shared NaN-input rule for arithmetic ops: any NaN gives the canonical
/// quiet NaN, and a signaling NaN raises invalid.
fn nan_in(a_nan: bool, b_nan: bool, a_snan: bool, b_snan: bool) -> Option<u8> {
    if a_nan || b_nan {
        Some(if a_snan || b_snan { NV } else { 0 })
    } else {
        None
    }
}

pub(crate) fn single_bin(op: BinOp, a_bits: u32, b_bits: u32, rm: u8) -> (u32, u8) {
    if let Some(fl) = nan_in(
        f32_is_nan(a_bits),
        f32_is_nan(b_bits),
        f32_is_snan(a_bits),
        f32_is_snan(b_bits),
    ) {
        return (F32_NAN, fl);
    }
    let a = f32::from_bits(a_bits);
    let b = f32::from_bits(b_bits);
    let (x, y) = if matches!(op, BinOp::Sub) {
        (a, -b)
    } else {
        (a, b)
    };
    let exact: f64 = match op {
        BinOp::Add | BinOp::Sub => {
            if x.is_infinite() && y.is_infinite() && x.is_sign_negative() != y.is_sign_negative() {
                return (F32_NAN, NV);
            }
            // The f64 sum of two f32 values is always exact.
            x as f64 + y as f64
        }
        BinOp::Mul => {
            if (a == 0.0 || b == 0.0) && (a.is_infinite() || b.is_infinite()) {
                return (F32_NAN, NV);
            }
            // Exact for the same reason: 48 mantissa bits fit in 53.
            a as f64 * b as f64
        }
        BinOp::Div => {
            if b == 0.0 {
                if a == 0.0 {
                    return (F32_NAN, NV);
                }
                let neg = a.is_sign_negative() ^ b.is_sign_negative();
                // inf/0 is a signed infinity; a finite nonzero dividend is
                // the divide-by-zero exception.
                return (inf32(neg), if a.is_infinite() { 0 } else { DZ });
            }
            if b.is_infinite() {
                if a.is_infinite() {
                    return (F32_NAN, NV);
                }
                return (zero32(a.is_sign_negative() ^ b.is_sign_negative()), 0);
            }
            if a == 0.0 {
                return (zero32(a.is_sign_negative() ^ b.is_sign_negative()), 0);
            }
            // f64 keeps extra precision; the mode-aware conversion below
            // rounds once more (see the module docs on double rounding).
            a as f64 / b as f64
        }
    };
    round_f64_to_f32_bits(exact, rm)
}

pub(crate) fn double_bin(op: BinOp, a_bits: u64, b_bits: u64, rm: u8) -> (u64, u8) {
    if let Some(fl) = nan_in(
        f64_is_nan(a_bits),
        f64_is_nan(b_bits),
        f64_is_snan(a_bits),
        f64_is_snan(b_bits),
    ) {
        return (F64_NAN, fl);
    }
    let a = f64::from_bits(a_bits);
    let b = f64::from_bits(b_bits);
    let (x, y) = if matches!(op, BinOp::Sub) {
        (a, -b)
    } else {
        (a, b)
    };
    match op {
        BinOp::Add | BinOp::Sub => {
            if x.is_infinite() && y.is_infinite() && x.is_sign_negative() != y.is_sign_negative() {
                return (F64_NAN, NV);
            }
            let (s, e) = two_sum(x, y);
            round_exact_to_f64(s, e, rm)
        }
        BinOp::Mul => {
            if (a == 0.0 || b == 0.0) && (a.is_infinite() || b.is_infinite()) {
                return (F64_NAN, NV);
            }
            let p = a * b;
            // The FMA residual completes the exact 106-bit product.
            let e = a.mul_add(b, -p);
            round_exact_to_f64(p, e, rm)
        }
        BinOp::Div => {
            if b == 0.0 {
                if a == 0.0 {
                    return (F64_NAN, NV);
                }
                let neg = a.is_sign_negative() ^ b.is_sign_negative();
                return (inf64(neg), if a.is_infinite() { 0 } else { DZ });
            }
            if b.is_infinite() {
                if a.is_infinite() {
                    return (F64_NAN, NV);
                }
                return (zero64(a.is_sign_negative() ^ b.is_sign_negative()), 0);
            }
            if a == 0.0 {
                return (zero64(a.is_sign_negative() ^ b.is_sign_negative()), 0);
            }
            let q = a / b;
            if q.is_infinite() {
                // Native overflow (finite operands cannot produce it here
                // otherwise).
                return (q.to_bits(), NX | OF);
            }
            let resid = b.mul_add(-q, a); // a − b·q with one rounding
            if resid == 0.0 {
                return (q.to_bits(), 0);
            }
            // q underestimated the exact quotient when resid·b > 0.
            let under = (resid > 0.0) == (b > 0.0);
            let r = match rm {
                RNE | RMM => q, // RMM ties fall back to RNE (module docs)
                RTZ => {
                    if (resid > 0.0) != (a > 0.0) {
                        step_zero64(q)
                    } else {
                        q
                    }
                }
                RDN => {
                    if under {
                        q.next_down()
                    } else {
                        q
                    }
                }
                _ => {
                    if under {
                        q.next_up()
                    } else {
                        q
                    }
                }
            };
            let bits = r.to_bits();
            (bits, tiny_flags(subnormal64(bits), NX))
        }
    }
}

pub(crate) fn single_sqrt(a_bits: u32, rm: u8) -> (u32, u8) {
    if f32_is_nan(a_bits) {
        return (F32_NAN, if f32_is_snan(a_bits) { NV } else { 0 });
    }
    let a = f32::from_bits(a_bits);
    if a < 0.0 {
        return (F32_NAN, NV);
    }
    if a == 0.0 || a.is_infinite() {
        return (a_bits, 0);
    }
    // f64 sqrt of an f32 value loses at most half an f64 ulp against the
    // true root, so the f32-grid rounding below is exact except for
    // boundary-adjacent operands (module docs).
    round_f64_to_f32_bits((a as f64).sqrt(), rm)
}

pub(crate) fn double_sqrt(a_bits: u64, rm: u8) -> (u64, u8) {
    if f64_is_nan(a_bits) {
        return (F64_NAN, if f64_is_snan(a_bits) { NV } else { 0 });
    }
    let a = f64::from_bits(a_bits);
    if a < 0.0 {
        return (F64_NAN, NV);
    }
    if a == 0.0 || a.is_infinite() {
        return (a_bits, 0);
    }
    let q = a.sqrt();
    let resid = q.mul_add(-q, a); // a − q² with one rounding
    if resid == 0.0 {
        return (q.to_bits(), 0);
    }
    let r = match rm {
        RNE | RMM => q,
        RTZ | RDN => {
            // resid > 0 means q is below the true root.
            if resid > 0.0 {
                q
            } else {
                q.next_down()
            }
        }
        _ => {
            if resid > 0.0 {
                q.next_up()
            } else {
                q
            }
        }
    };
    let bits = r.to_bits();
    (bits, tiny_flags(subnormal64(bits), NX))
}

pub(crate) fn single_fma(
    kind: FmaKind,
    a_bits: u32,
    b_bits: u32,
    c_bits: u32,
    rm: u8,
) -> (u32, u8) {
    if let Some(fl) = nan_in(
        f32_is_nan(a_bits) || f32_is_nan(b_bits) || f32_is_nan(c_bits),
        false,
        f32_is_snan(a_bits) || f32_is_snan(b_bits) || f32_is_snan(c_bits),
        false,
    ) {
        return (F32_NAN, fl);
    }
    let a = f32::from_bits(a_bits);
    let b = f32::from_bits(b_bits);
    let (flip_c, negate) = kind.adjust();
    let c = f32::from_bits(c_bits);
    let c_eff = if flip_c { -c } else { c };
    if (a == 0.0 && b.is_infinite()) || (a.is_infinite() && b == 0.0) {
        return (F32_NAN, NV);
    }
    let product_inf = a.is_infinite() || b.is_infinite();
    if product_inf && c_eff.is_infinite() {
        let prod_neg = a.is_sign_negative() ^ b.is_sign_negative();
        if prod_neg != c_eff.is_sign_negative() {
            return (F32_NAN, NV); // inf + (−inf)
        }
        return (inf32(prod_neg), 0);
    }
    if product_inf {
        return (inf32(a.is_sign_negative() ^ b.is_sign_negative()), 0);
    }
    // p is the exact product (f64 holds it), so two_sum completes the exact
    // fused value; RNE uses the native fused instruction.
    let p = a as f64 * b as f64;
    let (s1, e1) = two_sum(p, c_eff as f64);
    let inexact = e1 != 0.0;
    if rm == RNE {
        let v = a.mul_add(b, c_eff);
        let v = if negate { -v } else { v };
        let bits = v.to_bits();
        // The fused result is exact iff it reproduces the exact sum.
        let exact = s1 + e1;
        let mut flags = if e1 != 0.0 || v as f64 != exact {
            NX
        } else {
            0
        };
        if v.is_infinite() {
            flags |= OF | NX;
        }
        return (bits, tiny_flags(subnormal32(bits), flags));
    }
    let (r64_bits, _) = round_exact_to_f64(s1, e1, rm);
    let (bits, mut flags) = round_f64_to_f32_bits(f64::from_bits(r64_bits), rm);
    if inexact {
        flags |= NX;
    }
    let bits = if negate { bits ^ F32_SIGN } else { bits };
    (bits, flags)
}

pub(crate) fn double_fma(
    kind: FmaKind,
    a_bits: u64,
    b_bits: u64,
    c_bits: u64,
    rm: u8,
) -> (u64, u8) {
    if let Some(fl) = nan_in(
        f64_is_nan(a_bits) || f64_is_nan(b_bits) || f64_is_nan(c_bits),
        false,
        f64_is_snan(a_bits) || f64_is_snan(b_bits) || f64_is_snan(c_bits),
        false,
    ) {
        return (F64_NAN, fl);
    }
    let a = f64::from_bits(a_bits);
    let b = f64::from_bits(b_bits);
    let (flip_c, negate) = kind.adjust();
    let c = f64::from_bits(c_bits);
    let c_eff = if flip_c { -c } else { c };
    if (a == 0.0 && b.is_infinite()) || (a.is_infinite() && b == 0.0) {
        return (F64_NAN, NV);
    }
    let product_inf = a.is_infinite() || b.is_infinite();
    if product_inf && c_eff.is_infinite() {
        let prod_neg = a.is_sign_negative() ^ b.is_sign_negative();
        if prod_neg != c_eff.is_sign_negative() {
            return (F64_NAN, NV);
        }
        return (inf64(prod_neg), 0);
    }
    if product_inf {
        return (inf64(a.is_sign_negative() ^ b.is_sign_negative()), 0);
    }
    // Exact decomposition of ab + c: product plus FMA residual, then
    // two-sum with the addend. e2's own rounding makes this approximate in
    // the lowest bit for adversarial operands (module docs).
    let p = a * b;
    let e = a.mul_add(b, -p);
    let (s1, e1) = two_sum(p, c_eff);
    if rm == RNE {
        let v = a.mul_add(b, c_eff);
        let v = if negate { -v } else { v };
        let bits = v.to_bits();
        let mut flags = if e + e1 != 0.0 { NX } else { 0 };
        if v.is_infinite() {
            flags |= OF | NX;
        }
        return (bits, tiny_flags(subnormal64(bits), flags));
    }
    let e2 = e1 + e;
    let (s2, e3) = two_sum(s1, e2);
    let (bits, flags) = round_exact_to_f64(s2, e3, rm);
    (
        if negate {
            bits ^ 0x8000_0000_0000_0000
        } else {
            bits
        },
        flags,
    )
}

/// fle (0) / flt (1) / feq (2). Quiet NaNs raise invalid only for the
/// ordered comparisons; a signaling NaN raises it for feq too.
pub(crate) fn compare32(a_bits: u32, b_bits: u32, funct3: u32) -> (u64, u8) {
    if f32_is_nan(a_bits) || f32_is_nan(b_bits) {
        let snan = f32_is_snan(a_bits) || f32_is_snan(b_bits);
        return (0, if funct3 != 2 || snan { NV } else { 0 });
    }
    let a = f32::from_bits(a_bits);
    let b = f32::from_bits(b_bits);
    let r = match funct3 {
        0 => a <= b,
        1 => a < b,
        _ => a == b,
    };
    (r as u64, 0)
}

pub(crate) fn compare64(a_bits: u64, b_bits: u64, funct3: u32) -> (u64, u8) {
    if f64_is_nan(a_bits) || f64_is_nan(b_bits) {
        let snan = f64_is_snan(a_bits) || f64_is_snan(b_bits);
        return (0, if funct3 != 2 || snan { NV } else { 0 });
    }
    let a = f64::from_bits(a_bits);
    let b = f64::from_bits(b_bits);
    let r = match funct3 {
        0 => a <= b,
        1 => a < b,
        _ => a == b,
    };
    (r as u64, 0)
}

/// fmin/fmax: NaN operands yield the canonical quiet NaN with no flags
/// (RARS-level simplification — the 2019 spec would raise NV for signaling
/// inputs); ±0 pairs follow the sign rule.
pub(crate) fn min_max32(a_bits: u32, b_bits: u32, want_max: bool) -> u32 {
    if f32_is_nan(a_bits) || f32_is_nan(b_bits) {
        return F32_NAN;
    }
    let a = f32::from_bits(a_bits);
    let b = f32::from_bits(b_bits);
    if a == 0.0 && b == 0.0 {
        let a_neg = a_bits & F32_SIGN != 0;
        let b_neg = b_bits & F32_SIGN != 0;
        let neg = if want_max {
            a_neg && b_neg
        } else {
            a_neg || b_neg
        };
        return zero32(neg);
    }
    if want_max {
        (if a >= b { a } else { b }).to_bits()
    } else {
        (if a <= b { a } else { b }).to_bits()
    }
}

pub(crate) fn min_max64(a_bits: u64, b_bits: u64, want_max: bool) -> u64 {
    if f64_is_nan(a_bits) || f64_is_nan(b_bits) {
        return F64_NAN;
    }
    let a = f64::from_bits(a_bits);
    let b = f64::from_bits(b_bits);
    if a == 0.0 && b == 0.0 {
        let a_neg = a_bits >> 63 != 0;
        let b_neg = b_bits >> 63 != 0;
        let neg = if want_max {
            a_neg && b_neg
        } else {
            a_neg || b_neg
        };
        return zero64(neg);
    }
    if want_max {
        (if a >= b { a } else { b }).to_bits()
    } else {
        (if a <= b { a } else { b }).to_bits()
    }
}

/// fsgnj (0) / fsgnjn (1) / fsgnjx (2): pure sign manipulation, no flags.
pub(crate) fn sign_inject32(a_bits: u32, b_bits: u32, funct3: u32) -> u32 {
    let payload = a_bits & 0x7fff_ffff;
    match funct3 {
        0 => payload | (b_bits & F32_SIGN),
        1 => payload | (!b_bits & F32_SIGN),
        _ => payload | ((a_bits ^ b_bits) & F32_SIGN),
    }
}

pub(crate) fn sign_inject64(a_bits: u64, b_bits: u64, funct3: u32) -> u64 {
    const SIGN: u64 = 0x8000_0000_0000_0000;
    let payload = a_bits & !SIGN;
    match funct3 {
        0 => payload | (b_bits & SIGN),
        1 => payload | (!b_bits & SIGN),
        _ => payload | ((a_bits ^ b_bits) & SIGN),
    }
}

/// fclass: the 10-bit mask (bit 0 = −inf … bit 9 = quiet NaN).
pub(crate) fn classify32(bits: u32) -> u64 {
    let exp = (bits >> 23) & 0xff;
    let man = bits & 0x007f_ffff;
    let neg = bits & F32_SIGN != 0;
    match (exp, man) {
        (0xff, 0) => 1 << if neg { 0 } else { 7 }, // ±inf
        (0xff, _) => 1 << if man & 0x0040_0000 == 0 { 8 } else { 9 }, // sNaN/qNaN
        (0, 0) => 1 << if neg { 3 } else { 4 },    // ±0
        (0, _) => 1 << if neg { 2 } else { 5 },    // subnormal
        _ => 1 << if neg { 1 } else { 6 },         // normal
    }
}

pub(crate) fn classify64(bits: u64) -> u64 {
    let exp = (bits >> 52) & 0x7ff;
    let man = bits & 0x000f_ffff_ffff_ffff;
    let neg = bits >> 63 != 0;
    match (exp, man) {
        (0x7ff, 0) => 1 << if neg { 0 } else { 7 },
        (0x7ff, _) => {
            1 << if man & 0x0008_0000_0000_0000 == 0 {
                8
            } else {
                9
            }
        }
        (0, 0) => 1 << if neg { 3 } else { 4 },
        (0, _) => 1 << if neg { 2 } else { 5 },
        _ => 1 << if neg { 1 } else { 6 },
    }
}

/// Round a finite f64 to an integer-valued f64 under `rm` (fcvt to int).
fn round_to_integer(v: f64, rm: u8) -> f64 {
    match rm {
        RTZ => v.trunc(),
        RDN => v.floor(),
        RUP => v.ceil(),
        RMM => {
            let t = v.trunc();
            let frac = v - t;
            if frac == 0.5 || frac == -0.5 {
                if v < 0.0 {
                    t - 1.0
                } else {
                    t + 1.0
                }
            } else if frac > 0.5 {
                t + 1.0
            } else if frac < -0.5 {
                t - 1.0
            } else {
                t
            }
        }
        _ => v.round_ties_even(),
    }
}

/// FCVT float→int (32-bit result, sign-extended per the register
/// convention). Invalid inputs (NaN, ±inf, out-of-range) yield the positive
/// maximum 0x7fffffff / 0xffffffff with NV, matching the directed spec
/// behavior; the 2019 spec's toward-side clipping is a documented
/// simplification.
pub(crate) fn cvt_to_int(bits: u64, is_double: bool, rm: u8, unsigned: bool) -> (u64, u8) {
    let max = if unsigned {
        0xffff_ffffu64
    } else {
        0x7fff_ffff
    };
    let nan = if is_double {
        f64_is_nan(bits)
    } else {
        f32_is_nan(bits as u32)
    };
    if nan {
        return (max as i32 as i64 as u64, NV);
    }
    let v: f64 = if is_double {
        f64::from_bits(bits)
    } else {
        f32::from_bits(bits as u32) as f64
    };
    if v.is_infinite() {
        return (max as i32 as i64 as u64, NV);
    }
    let n = round_to_integer(v, rm);
    let flags = if n != v { NX } else { 0 };
    if unsigned {
        if !(0.0..=u32::MAX as f64).contains(&n) {
            return (max as i32 as i64 as u64, flags | NV);
        }
        ((n as u32) as i32 as i64 as u64, flags)
    } else {
        if !((i32::MIN as f64)..=(i32::MAX as f64)).contains(&n) {
            return (max as i32 as i64 as u64, flags | NV);
        }
        ((n as i32) as i64 as u64, flags)
    }
}

/// FCVT int→f32: the i32/u32 source widens exactly into f64, then rounds
/// onto the f32 grid (inexact for large integers).
pub(crate) fn cvt_int_to_f32(v: u64, unsigned: bool, rm: u8) -> (u32, u8) {
    let x: f64 = if unsigned {
        v as u32 as f64
    } else {
        v as i32 as f64
    };
    round_f64_to_f32_bits(x, rm)
}

/// FCVT float→int with a 64-bit result (fcvt.l.s/fcvt.lu.s/fcvt.l.d/
/// fcvt.lu.d, RV64 only). Invalid inputs (NaN, ±inf, out of range) yield
/// the target type's positive maximum with NV — the same documented
/// simplification as the 32-bit path.
pub(crate) fn cvt_to_int64(bits: u64, is_double: bool, rm: u8, unsigned: bool) -> (u64, u8) {
    let nan = if is_double {
        f64_is_nan(bits)
    } else {
        f32_is_nan(bits as u32)
    };
    if nan {
        return (if unsigned { u64::MAX } else { i64::MAX as u64 }, NV);
    }
    let v: f64 = if is_double {
        f64::from_bits(bits)
    } else {
        f32::from_bits(bits as u32) as f64
    };
    if v.is_infinite() {
        return (if unsigned { u64::MAX } else { i64::MAX as u64 }, NV);
    }
    let n = round_to_integer(v, rm);
    let flags = if n != v { NX } else { 0 };
    if unsigned {
        // f64 holds 2^64 exactly, and that value is already out of range.
        if !(0.0..18_446_744_073_709_551_616.0).contains(&n) {
            return (u64::MAX, flags | NV);
        }
        (n as u64, flags)
    } else {
        // i64::MAX as f64 rounds up to 2^63, so the exclusive bound is exact.
        if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&n) {
            return (i64::MAX as u64, flags | NV);
        }
        (n as i64 as u64, flags)
    }
}

/// Exact representability of a 64-bit magnitude on a float grid: the value
/// is exact iff its significant-bit span fits the mantissa (implicit bit
/// included). Used to flag inexactness of int→float conversions.
fn exact_on_grid(mag: u64, mantissa_bits: u32) -> bool {
    mag == 0 || (63 - mag.leading_zeros()) - mag.trailing_zeros() <= mantissa_bits
}

/// FCVT int→f32 from a 64-bit source (fcvt.s.l/fcvt.s.lu, RV64 only).
/// Host integer→float casts are correctly rounded RNE; other modes fall
/// back to RNE, the same documented fallback as 64-bit div/sqrt.
pub(crate) fn cvt_int64_to_f32(v: u64, unsigned: bool, _rm: u8) -> (u32, u8) {
    let x: f32 = if unsigned {
        v as f32
    } else {
        (v as i64) as f32
    };
    let mag = if unsigned {
        v
    } else {
        (v as i64).unsigned_abs()
    };
    let flags = if exact_on_grid(mag, 23) { 0 } else { NX };
    (x.to_bits(), flags)
}

/// FCVT int→f64 from a 64-bit source (fcvt.d.l/fcvt.d.lu, RV64 only).
pub(crate) fn cvt_int64_to_f64(v: u64, unsigned: bool, _rm: u8) -> (u64, u8) {
    let x: f64 = if unsigned {
        v as f64
    } else {
        (v as i64) as f64
    };
    let mag = if unsigned {
        v
    } else {
        (v as i64).unsigned_abs()
    };
    let flags = if exact_on_grid(mag, 52) { 0 } else { NX };
    (x.to_bits(), flags)
}

/// FCVT int→f64 is always exact.
pub(crate) fn cvt_int_to_f64(v: u64, unsigned: bool) -> u64 {
    if unsigned {
        (v as u32) as f64
    } else {
        (v as i32) as f64
    }
    .to_bits()
}

/// FCVT f64→f32 (precision narrowing, any rounding mode).
pub(crate) fn cvt_f64_to_f32(bits: u64, rm: u8) -> (u32, u8) {
    if f64_is_nan(bits) {
        return (F32_NAN, if f64_is_snan(bits) { NV } else { 0 });
    }
    let v = f64::from_bits(bits);
    if v.is_infinite() {
        return (inf32(v.is_sign_negative()), 0);
    }
    round_f64_to_f32_bits(v, rm)
}

/// FCVT f32→f64 is exact.
pub(crate) fn cvt_f32_to_f64(bits: u32) -> (u64, u8) {
    if f32_is_nan(bits) {
        return (F64_NAN, if f32_is_snan(bits) { NV } else { 0 });
    }
    ((f32::from_bits(bits) as f64).to_bits(), 0)
}

/// Rounding direction along the magnitude axis.
#[derive(Clone, Copy)]
enum Dir {
    NearEven,
    NearAway,
    Floor,
    Ceil,
}

/// Round a (possibly inexact) f64 value onto the f32 grid under `rm`,
/// returning f32 bits plus NX/OF/UF.
pub(crate) fn round_f64_to_f32_bits(v: f64, rm: u8) -> (u32, u8) {
    debug_assert!(!v.is_nan());
    if v.is_infinite() {
        return (
            if v.is_sign_negative() {
                0xff80_0000
            } else {
                0x7f80_0000
            },
            0,
        );
    }
    let neg = v.is_sign_negative();
    let mag = v.abs();
    if mag == 0.0 {
        return (if neg { F32_SIGN } else { 0 }, 0);
    }

    // Nearest-even reference result; overflow becomes infinity.
    let rne = mag as f32;
    let inexact = rne as f64 != mag;
    let max = f32::MAX as f64;
    // Overflow threshold: the nearest modes round the tie just above max
    // (mantissa all ones = odd) up to infinity; directed modes only overflow
    // when the magnitude itself exceeds max.
    let half_ulp_max = max - f64::from(f32::from_bits(f32::MAX.to_bits() - 1));
    let of = match rm {
        RNE | RMM => mag >= max + half_ulp_max,
        _ => mag > max,
    };
    let mut flags = 0u8;
    if inexact {
        flags |= NX;
    }
    if of {
        flags |= OF | NX;
    }

    // Decide on the magnitude axis, then reapply the sign.
    let dir = match rm {
        RNE => Dir::NearEven,
        RTZ => Dir::Floor,
        RDN => {
            if neg {
                Dir::Ceil
            } else {
                Dir::Floor
            }
        }
        RUP => {
            if neg {
                Dir::Floor
            } else {
                Dir::Ceil
            }
        }
        _ => Dir::NearAway,
    };
    let bits_mag = match dir {
        Dir::NearEven => rne.to_bits(),
        Dir::NearAway => {
            if !inexact {
                rne.to_bits()
            } else if rne.is_infinite() {
                f32::INFINITY.to_bits()
            } else {
                // The two grid points bracketing the value — the RNE result
                // is one of them, the neighbor on the other side completes
                // the bracket.
                let (lo, hi) = if (rne as f64) > mag {
                    (
                        if rne.to_bits() >= 1 {
                            f32::from_bits(rne.to_bits() - 1)
                        } else {
                            rne
                        },
                        rne,
                    )
                } else {
                    (rne, f32::from_bits(rne.to_bits() + 1))
                };
                // Nearer wins; a tie rounds away from zero (up the axis).
                if (mag - lo as f64) < (hi as f64 - mag) {
                    lo.to_bits()
                } else {
                    hi.to_bits()
                }
            }
        }
        Dir::Floor => {
            if rne.is_infinite() {
                f32::MAX.to_bits()
            } else if inexact && rne as f64 > mag && rne.to_bits() >= 1 {
                rne.to_bits() - 1
            } else {
                rne.to_bits()
            }
        }
        Dir::Ceil => {
            if rne.is_infinite() {
                f32::INFINITY.to_bits()
            } else if inexact && (rne as f64) < mag {
                // One ulp up the grid; at max this wraps to infinity.
                rne.to_bits() + 1
            } else {
                rne.to_bits()
            }
        }
    };
    let bits = if neg { bits_mag | F32_SIGN } else { bits_mag };
    if bits & 0x7fff_ffff == 0 {
        // Rounded all the way to zero: exact when the input was zero,
        // otherwise the tiny value was lost (softfloat raises NX|UF).
        return (bits, if inexact { flags | UF } else { 0 });
    }
    (bits, tiny_flags(subnormal32(bits), flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_boxing() {
        assert_eq!(single_bits(box_single(1.5f32.to_bits())), 1.5f32.to_bits());
        // Un-boxed register contents read as canonical NaN.
        assert_eq!(single_bits(1.5f32.to_bits() as u64), F32_NAN);
        assert_eq!(single_bits(0), F32_NAN);
    }

    #[test]
    fn resolve_modes() {
        assert_eq!(resolve_rm(0, 2), Some(RNE));
        assert_eq!(resolve_rm(7, 4), Some(RMM));
        assert_eq!(resolve_rm(7, 0), Some(RNE));
        assert_eq!(resolve_rm(5, 0), None);
        assert_eq!(resolve_rm(6, 7), None);
    }

    #[test]
    fn f32_rounding_modes_inexact() {
        // 1 + 3*2^-25 sits 1.375 ulp above 1.0.
        let v = 1.0 + 3.0 * f64::powi(2.0, -25);
        assert_eq!(round_f64_to_f32_bits(v, RNE).0, 0x3f80_0001); // nearest is up
        let (rtz, fl) = round_f64_to_f32_bits(v, RTZ);
        assert_eq!(rtz, 0x3f80_0000);
        assert_eq!(fl, NX);
        assert_eq!(round_f64_to_f32_bits(v, RDN).0, 0x3f80_0000);
        assert_eq!(round_f64_to_f32_bits(v, RUP).0, 0x3f80_0001);
        assert_eq!(round_f64_to_f32_bits(v, RMM).0, 0x3f80_0001);
        // Exact tie 1 + 2^-24 (half an ulp at 1.0): RNE stays on the even
        // 1.0, RMM goes away.
        let tie = 1.0 + f64::powi(2.0, -24);
        assert_eq!(round_f64_to_f32_bits(tie, RNE).0, 0x3f80_0000);
        assert_eq!(round_f64_to_f32_bits(tie, RMM).0, 0x3f80_0001);
        // Exact values carry no flags.
        assert_eq!(round_f64_to_f32_bits(1.5, RNE), (1.5f32.to_bits(), 0));
    }

    #[test]
    fn f32_rounding_negative_modes() {
        let v = -(1.0 + 3.0 * f64::powi(2.0, -25));
        // For negatives RDN rounds away from zero: negative floats order by
        // magnitude in increasing bit patterns, so that is bits+1 from -1.0.
        assert_eq!(round_f64_to_f32_bits(v, RDN).0, (-1.0f32).to_bits() + 1);
        assert_eq!(round_f64_to_f32_bits(v, RTZ).0, (-1.0f32).to_bits());
        assert_eq!(round_f64_to_f32_bits(v, RUP).0, (-1.0f32).to_bits());
        assert_eq!(round_f64_to_f32_bits(v, RMM).0, (-1.0f32).to_bits() + 1);
        // Sign is preserved on every path.
        for rm in [RNE, RTZ, RDN, RUP, RMM] {
            assert_eq!(round_f64_to_f32_bits(v, rm).0 & F32_SIGN, F32_SIGN);
        }
    }

    #[test]
    fn f32_overflow_flags() {
        let big = f64::from(f32::MAX) * 1.5;
        let (bits, fl) = round_f64_to_f32_bits(big, RNE);
        assert_eq!(bits, f32::INFINITY.to_bits());
        assert_eq!(fl, NX | OF);
        // RTZ clamps to the largest finite but still raises OF|NX.
        assert_eq!(
            round_f64_to_f32_bits(big, RTZ),
            (f32::MAX.to_bits(), NX | OF)
        );
        assert_eq!(round_f64_to_f32_bits(big, RDN).0, f32::MAX.to_bits());
        assert_eq!(round_f64_to_f32_bits(big, RUP).0, f32::INFINITY.to_bits());
        // Below max + half an ulp, RNE stays finite with just NX.
        let near = f64::from(f32::MAX) * (1.0 + f64::powi(2.0, -30));
        assert_eq!(round_f64_to_f32_bits(near, RNE), (f32::MAX.to_bits(), NX));
        // Negative overflow: RDN to -infinity, RUP clamped to -max.
        let neg_big = -big;
        assert_eq!(round_f64_to_f32_bits(neg_big, RDN).0, 0xff80_0000);
        assert_eq!(round_f64_to_f32_bits(neg_big, RUP).0, 0xff7f_ffff);
    }

    #[test]
    fn f32_subnormal_underflow() {
        // One subnormal step = 2^-149. 1.5 steps is a grid tie that rounds
        // to the even neighbor and is both tiny and inexact.
        let step = f64::from(f32::from_bits(1));
        assert_eq!(round_f64_to_f32_bits(step * 1.5, RNE), (2, NX | UF));
        // Half a step rounds to zero but still reports the lost tininess.
        assert_eq!(round_f64_to_f32_bits(step * 0.5, RNE), (0, NX | UF));
        // An exactly representable subnormal: no flags at all.
        assert_eq!(round_f64_to_f32_bits(step, RNE).1, 0);
        // Min-normal/2 is exactly representable as a subnormal — no flags.
        assert_eq!(
            round_f64_to_f32_bits(f64::from(f32::MIN_POSITIVE) / 2.0, RNE).1,
            0
        );
    }

    #[test]
    fn f64_exact_rounding() {
        // (p, e) pairs per the contract: p is the native RNE result of the
        // exact value p + e, e the residual.
        let one = 1.0f64;
        let ulp = one.next_up() - one;
        // Exact value 1 + 1.5 ulp: RNE ties to the even 1+2ulp, so p is the
        // upper neighbor with e = -0.5 ulp.
        let even_up = one + 2.0 * ulp;
        let p = even_up;
        let e = -0.5 * ulp;
        assert_eq!(round_exact_to_f64(p, e, RNE).0, even_up.to_bits());
        assert_eq!(round_exact_to_f64(p, e, RNE).1, NX);
        // RTZ/RDN take the grid point below, RUP keeps p, and RMM's tie
        // goes away from zero (up, since the value is positive).
        assert_eq!(round_exact_to_f64(p, e, RTZ).0, (one + ulp).to_bits());
        assert_eq!(round_exact_to_f64(p, e, RDN).0, (one + ulp).to_bits());
        assert_eq!(round_exact_to_f64(p, e, RUP).0, even_up.to_bits());
        assert_eq!(round_exact_to_f64(p, e, RMM).0, even_up.to_bits());
        // 1 + 0.25 ulp: RNE lands on 1+1ulp (nearest), directed modes split.
        let p = one + ulp;
        let e = -0.25 * ulp;
        assert_eq!(round_exact_to_f64(p, e, RTZ).0, one.to_bits());
        assert_eq!(round_exact_to_f64(p, e, RDN).0, one.to_bits());
        assert_eq!(round_exact_to_f64(p, e, RUP).0, p.to_bits());
        assert_eq!(round_exact_to_f64(p, e, RMM).0, p.to_bits());
        // Negative mirror of the tie: away from zero means down.
        let p = -even_up;
        let e = 0.5 * ulp;
        assert_eq!(round_exact_to_f64(p, e, RMM).0, (-even_up).to_bits());
        assert_eq!(round_exact_to_f64(p, e, RDN).0, (-even_up).to_bits());
        assert_eq!(round_exact_to_f64(p, e, RUP).0, (-(one + ulp)).to_bits());
        // Exact residual: no flags.
        assert_eq!(round_exact_to_f64(2.5, 0.0, RTZ), (2.5f64.to_bits(), 0));
    }

    #[test]
    fn single_arithmetic_directed() {
        let (bits, fl) = single_bin(BinOp::Add, 1.5f32.to_bits(), 2.25f32.to_bits(), RNE);
        assert_eq!((bits, fl), (3.75f32.to_bits(), 0));
        // -0 + -0 = -0; +0 + -0 = +0.
        assert_eq!(
            single_bin(BinOp::Add, zero32(true), zero32(true), RNE).0,
            zero32(true)
        );
        assert_eq!(
            single_bin(BinOp::Add, zero32(false), zero32(true), RNE).0,
            zero32(false)
        );
        // inf + -inf invalid; inf + 1 = inf.
        assert_eq!(
            single_bin(BinOp::Add, inf32(false), inf32(true), RNE),
            (F32_NAN, NV)
        );
        assert_eq!(
            single_bin(BinOp::Add, inf32(false), 1.0f32.to_bits(), RNE).0,
            inf32(false)
        );
        // Quiet NaN propagates without flags, signaling raises NV.
        assert_eq!(
            single_bin(BinOp::Add, F32_NAN, 1.0f32.to_bits(), RNE),
            (F32_NAN, 0)
        );
        assert_eq!(
            single_bin(BinOp::Add, 0x7f80_0001, 1.0f32.to_bits(), RNE),
            (F32_NAN, NV)
        );
        // Division: 1/0 = +inf + DZ; 0/0 invalid; inf/0 = inf.
        assert_eq!(
            single_bin(BinOp::Div, 1.0f32.to_bits(), zero32(false), RNE),
            (inf32(false), DZ)
        );
        assert_eq!(
            single_bin(BinOp::Div, zero32(false), zero32(false), RNE),
            (F32_NAN, NV)
        );
        assert_eq!(
            single_bin(BinOp::Div, inf32(false), zero32(true), RNE),
            (inf32(true), 0)
        );
        // 1/3 is inexact.
        let (_, fl) = single_bin(BinOp::Div, 1.0f32.to_bits(), 3.0f32.to_bits(), RNE);
        assert_eq!(fl, NX);
        // Overflow to infinity raises OF|NX.
        let big = f32::MAX.to_bits();
        let (_, fl) = single_bin(BinOp::Mul, big, big, RNE);
        assert_eq!(fl, OF | NX);
        // sqrt(-1) → canonical NaN + NV; sqrt(4) exact.
        assert_eq!(single_sqrt((-1.0f32).to_bits(), RNE), (F32_NAN, NV));
        assert_eq!(single_sqrt(4.0f32.to_bits(), RNE), (2.0f32.to_bits(), 0));
        assert_eq!(single_sqrt((-0.0f32).to_bits(), RNE).0, (-0.0f32).to_bits());
    }

    #[test]
    fn double_arithmetic_directed() {
        let (bits, fl) = double_bin(BinOp::Add, 1.5f64.to_bits(), 2.25f64.to_bits(), RNE);
        assert_eq!((bits, fl), (3.75f64.to_bits(), 0));
        // 0.1 + 0.2 is inexact under every mode; RTZ truncates the RNE sum.
        let v = 0.1f64 + 0.2f64; // native RNE sum
        let (rtz, fl) = double_bin(BinOp::Add, 0.1f64.to_bits(), 0.2f64.to_bits(), RTZ);
        assert_ne!(rtz, v.to_bits());
        assert_eq!(fl, NX);
        // 1 - 2^-54 sits exactly half an ulp below 1: RTZ takes it, RNE
        // ties to the even 1.0.
        let half = (1.0f64 - 1.0f64.next_down()) / 2.0;
        assert_eq!(
            double_bin(BinOp::Add, 1.0f64.to_bits(), (-half).to_bits(), RTZ).0,
            1.0f64.next_down().to_bits()
        );
        assert_eq!(
            double_bin(BinOp::Add, 1.0f64.to_bits(), (-half).to_bits(), RNE).0,
            1.0f64.to_bits()
        );
        // 1/3 inexact in every mode.
        for rm in [RNE, RTZ, RDN, RUP, RMM] {
            let (_, fl) = double_bin(BinOp::Div, 1.0f64.to_bits(), 3.0f64.to_bits(), rm);
            assert_eq!(fl, NX);
        }
        // Directed RTZ/RUP division on cases with known excess: 4/3 rounds
        // down under RNE (q under-estimates, so RTZ keeps q and RUP steps
        // up), while 10/3 rounds up (RTZ steps down, RUP keeps q).
        let q = 4.0f64 / 3.0f64; // native RNE quotient
        assert_eq!(
            double_bin(BinOp::Div, 4.0f64.to_bits(), 3.0f64.to_bits(), RTZ).0,
            q.to_bits()
        );
        assert_eq!(
            double_bin(BinOp::Div, 4.0f64.to_bits(), 3.0f64.to_bits(), RUP).0,
            q.next_up().to_bits()
        );
        let q = 10.0f64 / 3.0f64;
        assert_eq!(
            double_bin(BinOp::Div, 10.0f64.to_bits(), 3.0f64.to_bits(), RTZ).0,
            q.next_down().to_bits()
        );
        assert_eq!(
            double_bin(BinOp::Div, 10.0f64.to_bits(), 3.0f64.to_bits(), RUP).0,
            q.to_bits()
        );
        // Overflow.
        let (_, fl) = double_bin(BinOp::Mul, f64::MAX.to_bits(), 2.0f64.to_bits(), RNE);
        assert_eq!(fl, OF | NX);
        // sqrt(-1) invalid; sqrt(2) inexact.
        assert_eq!(double_sqrt((-1.0f64).to_bits(), RNE), (F64_NAN, NV));
        let (_, fl) = double_sqrt(2.0f64.to_bits(), RNE);
        assert_eq!(fl, NX);
    }

    #[test]
    fn fma_single_rounding_directed() {
        // a = 1 + 2^-23, b = 1 + 3·2^-23, c = -1: the exact product is
        // 1 + 2^-21 + 3·2^-46, so the fused result keeps the low bit while
        // a separate mul+add loses it.
        let a = (1.0f32 + f32::powi(2.0, -23)).to_bits();
        let b = (1.0f32 + 3.0 * f32::powi(2.0, -23)).to_bits();
        let c = (-1.0f32).to_bits();
        let (fused, fl) = single_fma(FmaKind::Fmadd, a, b, c, RNE);
        assert_eq!(fl, NX);
        // 2^-21 + 3·2^-46 on the 2^-44 grid rounds up: mantissa 0x...01.
        assert_eq!(fused, 0x3500_0001);
        // Separate mul then add collapses to plain 2^-21.
        let (mul_bits, _) = single_bin(BinOp::Mul, a, b, RNE);
        let (sep, _) = single_bin(BinOp::Add, mul_bits, c, RNE);
        assert_eq!(sep, 0x3500_0000);
        assert_ne!(fused, sep);
        // fmsub/fnmadd/fnmsub sign conventions.
        let d = 2.0f32.to_bits();
        assert_eq!(
            single_fma(FmaKind::Fmsub, a, b, d, RNE).0,
            single_bin(BinOp::Add, mul_bits, (-2.0f32).to_bits(), RNE).0
        );
        let (fnm, _) = single_fma(FmaKind::Fnmadd, a, b, d, RNE);
        assert_eq!(fnm ^ F32_SIGN, single_bin(BinOp::Add, mul_bits, d, RNE).0);
        let (fns, _) = single_fma(FmaKind::Fnmsub, a, b, d, RNE);
        assert_eq!(
            fns ^ F32_SIGN,
            single_bin(BinOp::Add, mul_bits, (-2.0f32).to_bits(), RNE).0
        );
        // 0 * inf + c is invalid.
        assert_eq!(
            single_fma(
                FmaKind::Fmadd,
                zero32(false),
                inf32(false),
                1.0f32.to_bits(),
                RNE
            ),
            (F32_NAN, NV)
        );
    }

    #[test]
    fn fma_double_and_conversions() {
        let a = (1.0f64 + f64::powi(2.0, -52)).to_bits();
        // (1+2^-52)^2 - 1 = 2^-51 + 2^-104, single-rounded to 2^-51.
        let (bits, _) = double_fma(FmaKind::Fmadd, a, a, (-1.0f64).to_bits(), RNE);
        assert_eq!(bits, f64::powi(2.0, -51).to_bits());
        // fcvt.w.s truncates toward zero (rm RTZ baked by the assembler).
        assert_eq!(cvt_to_int(1.9f32.to_bits() as u64, false, RTZ, false).0, 1);
        assert_eq!(
            cvt_to_int((-1.9f32).to_bits() as u64, false, RTZ, false).0,
            (-1i32) as i64 as u64
        );
        // Out of range and NaN → positive max + NV. The 32-bit 0xffffffff
        // result sign-extends per the register convention.
        let (v, fl) = cvt_to_int(3.0e9f32.to_bits() as u64, false, RTZ, false);
        assert_eq!((v, fl), (0x7fff_ffff, NV));
        let (v, fl) = cvt_to_int(F32_NAN as u64, false, RTZ, false);
        assert_eq!((v, fl), (0x7fff_ffff, NV));
        let (v, fl) = cvt_to_int(F32_NAN as u64, false, RTZ, true);
        assert_eq!((v, fl), (u64::MAX, NV)); // 0xffffffff sign-extended
                                             // fcvt.s.w of i32::MAX is inexact (rounds to 2^31).
        let (bits, fl) = cvt_int_to_f32(i32::MAX as u64, false, RNE);
        assert_eq!((bits, fl), ((2.0f32.powi(31)).to_bits(), NX));
        // u32::MAX → 2^32 exactly.
        let (bits, fl) = cvt_int_to_f32(u32::MAX as u64, true, RNE);
        assert_eq!((bits, fl), (4294967296.0f32.to_bits(), NX));
        // Int → f64 is exact.
        assert_eq!(
            cvt_int_to_f64(i32::MIN as u64, false),
            (i32::MIN as f64).to_bits()
        );
        // f64 → f32 narrowing with modes.
        let v = 1.0 + 3.0 * f64::powi(2.0, -25);
        assert_eq!(cvt_f64_to_f32(v.to_bits(), RNE).0, 0x3f80_0001);
        assert_eq!(cvt_f64_to_f32(v.to_bits(), RTZ).0, 0x3f80_0000);
    }

    #[test]
    fn min_max_sign_inject_classify() {
        assert_eq!(
            min_max32(1.5f32.to_bits(), 2.5f32.to_bits(), false),
            1.5f32.to_bits()
        );
        assert_eq!(
            min_max32(1.5f32.to_bits(), 2.5f32.to_bits(), true),
            2.5f32.to_bits()
        );
        // NaN operand → canonical NaN either way.
        assert_eq!(min_max32(F32_NAN, 1.0f32.to_bits(), true), F32_NAN);
        // fmax(-0, +0) = +0; fmin(-0, +0) = -0.
        assert_eq!(min_max32(zero32(true), zero32(false), true), zero32(false));
        assert_eq!(min_max32(zero32(true), zero32(false), false), zero32(true));
        // Sign injection.
        let neg = (-1.5f32).to_bits();
        let pos = 1.5f32.to_bits();
        assert_eq!(sign_inject32(pos, neg, 0), neg); // sgnj: copy b's sign
        assert_eq!(sign_inject32(neg, pos, 1), neg); // sgnjn: invert b's sign
        assert_eq!(sign_inject32(pos, pos, 1), neg);
        assert_eq!(sign_inject32(neg, neg, 2), pos); // sgnjx clears sign
        assert_eq!(sign_inject32(neg, pos, 2), neg);
        // fclass masks for all ten classes.
        assert_eq!(classify32(inf32(true)), 1 << 0);
        assert_eq!(classify32(inf32(false)), 1 << 7);
        assert_eq!(classify32((-1.0f32).to_bits()), 1 << 1);
        assert_eq!(classify32(1.0f32.to_bits()), 1 << 6);
        assert_eq!(classify32(0x1), 1 << 5); // +subnormal
        assert_eq!(classify32(0x8000_0001), 1 << 2); // -subnormal
        assert_eq!(classify32(zero32(false)), 1 << 4);
        assert_eq!(classify32(zero32(true)), 1 << 3);
        assert_eq!(classify32(0x7f80_0001), 1 << 8); // sNaN
        assert_eq!(classify32(F32_NAN), 1 << 9);
        assert_eq!(classify64((-f64::INFINITY).to_bits()), 1 << 0);
        assert_eq!(classify64(F64_NAN), 1 << 9);
    }

    #[test]
    fn comparisons() {
        assert_eq!(compare32(1.0f32.to_bits(), 2.0f32.to_bits(), 1), (1, 0)); // flt
        assert_eq!(compare32(2.0f32.to_bits(), 2.0f32.to_bits(), 0), (1, 0)); // fle
        assert_eq!(compare32(1.0f32.to_bits(), 2.0f32.to_bits(), 2), (0, 0)); // feq
        assert_eq!(compare32(zero32(true), zero32(false), 2), (1, 0)); // -0 == +0
                                                                       // Ordered comparisons raise NV on any NaN; feq only on sNaN.
        assert_eq!(compare32(F32_NAN, 1.0f32.to_bits(), 1).1, NV);
        assert_eq!(compare32(F32_NAN, 1.0f32.to_bits(), 2).1, 0);
        assert_eq!(compare32(0x7f80_0001, 1.0f32.to_bits(), 2).1, NV);
        assert_eq!(compare64(F64_NAN, 1.0f64.to_bits(), 1).1, NV);
    }
}
