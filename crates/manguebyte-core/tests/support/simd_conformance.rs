//! Every operation of `manguebyte_core::simd`, run on one backend and
//! compared bit for bit with the same operation written in plain Rust.
//!
//! Shared by `tests/simd.rs`, which runs it on every backend of the machine,
//! and by the `simd_register_wasm` example, which runs it in a WebAssembly
//! module built with `simd128`. It uses the public API only.

use manguebyte_core::simd::{Aligned, Backend, Isa, Kernel, Mask, Register, SimdSample};

/// The bits of a sample, as they are.
pub trait Bits: SimdSample {
    fn bits(self) -> u64;
    /// A sample made of the low bits of `bits`.
    fn of_bits(bits: u64) -> Self;
    fn is_nan(self) -> bool;
    /// A zero or a subnormal, with the sign bit it has: what a processor
    /// flushing subnormals takes for the same number.
    fn tiny_sign(self) -> Option<bool>;
    /// Zeros, infinities, NaNs with payloads, subnormals and extremes.
    fn specials() -> Vec<Self>;
}

macro_rules! bits {
    ($t:ty, $u:ty, $payload:expr) => {
        impl Bits for $t {
            fn bits(self) -> u64 {
                u64::from(self.to_bits())
            }
            fn of_bits(bits: u64) -> Self {
                <$t>::from_bits(bits as $u)
            }
            fn is_nan(self) -> bool {
                <$t>::is_nan(self)
            }
            fn tiny_sign(self) -> Option<bool> {
                // By the bits: a comparison would itself be flushed.
                let magnitude = self.to_bits() << 1 >> 1;
                (magnitude < <$t>::MIN_POSITIVE.to_bits()).then(|| self.is_sign_negative())
            }
            fn specials() -> Vec<Self> {
                vec![
                    0.0,
                    -0.0,
                    1.0,
                    -1.0,
                    0.5,
                    -2.0,
                    <$t>::INFINITY,
                    <$t>::NEG_INFINITY,
                    <$t>::NAN,
                    <$t>::from_bits($payload),
                    <$t>::MIN_POSITIVE,
                    -<$t>::MIN_POSITIVE,
                    <$t>::from_bits(1),
                    -<$t>::from_bits(3),
                    <$t>::MAX,
                    <$t>::MIN,
                    <$t>::EPSILON,
                ]
            }
        }
    };
}

bits!(f32, u32, 0xFFC0_1234);
bits!(f64, u64, 0xFFF8_0000_0000_1234);

/// A small generator of test values (SplitMix64).
pub struct Random(pub u64);

impl Random {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value from -4 to 4 with all the digits of the sample type.
    pub fn moderate<S: Bits>(&mut self) -> S {
        S::from_f64((self.next() >> 11) as f64 * (8.0 / (1u64 << 53) as f64) - 4.0)
    }
}

/// Three streams of operands: every pair of special values, then random bit
/// patterns, then moderate numbers.
fn operands<S: Bits>(random_blocks: usize) -> [Vec<S>; 3] {
    let specials = S::specials();
    let n = specials.len();
    let mut random = Random(0x4D61_6E67_7565);
    let mut streams = [Vec::new(), Vec::new(), Vec::new()];
    for i in 0..n * n {
        streams[0].push(specials[i / n]);
        streams[1].push(specials[i % n]);
        streams[2].push(specials[(i / n + 3 * i) % n]);
    }
    while streams[0].len() % S::LANES != 0 {
        for stream in &mut streams {
            stream.push(S::ONE);
        }
    }
    for block in 0..2 * random_blocks {
        for _ in 0..S::LANES {
            for stream in &mut streams {
                let value = if block < random_blocks {
                    S::of_bits(random.next())
                } else {
                    random.moderate()
                };
                stream.push(value);
            }
        }
    }
    streams
}

/// Whether an operation moves bits as they are, or computes, in which case
/// any NaN stands for any other.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Moved,
    Computed,
}

/// What each operation gave, in the order of the operands.
struct Outputs<S> {
    values: Vec<(&'static str, Kind, Vec<S>)>,
    masks: Vec<(&'static str, Vec<u32>)>,
}

impl<S: SimdSample> Outputs<S> {
    fn value(&mut self, name: &'static str, kind: Kind, lanes: &[S]) {
        match self.values.iter_mut().find(|(n, _, _)| *n == name) {
            Some((_, _, all)) => all.extend_from_slice(lanes),
            None => self.values.push((name, kind, lanes.to_vec())),
        }
    }

    fn mask(&mut self, name: &'static str, bits: u32) {
        match self.masks.iter_mut().find(|(n, _)| *n == name) {
            Some((_, all)) => all.push(bits),
            None => self.masks.push((name, vec![bits])),
        }
    }
}

/// Runs every operation on each register's worth of the operands.
struct EveryOperation<'a, S> {
    a: &'a [S],
    b: &'a [S],
    c: &'a [S],
}

impl<S: Bits> Kernel for EveryOperation<'_, S> {
    type Output = (Backend, Outputs<S>);

    #[inline(always)]
    fn run<I: Isa>(self, isa: I) -> Self::Output {
        use Kind::{Computed, Moved};
        let lanes = S::LANES;
        let mut out = Outputs {
            values: Vec::new(),
            masks: Vec::new(),
        };
        macro_rules! value {
            ($name:literal, $kind:expr, $register:expr) => {
                out.value($name, $kind, $register.to_array().as_ref())
            };
        }
        for (block, ((a, b), c)) in self
            .a
            .chunks_exact(lanes)
            .zip(self.b.chunks_exact(lanes))
            .zip(self.c.chunks_exact(lanes))
            .enumerate()
        {
            let (x, y, z) = (isa.load(a), isa.load(b), isa.load(c));
            value!("add", Computed, x + y);
            value!("sub", Computed, x - y);
            value!("mul", Computed, x * y);
            value!("div", Computed, x / y);
            value!("neg", Moved, -x);
            value!("abs", Moved, x.abs());
            value!("sqrt", Computed, x.sqrt());
            value!("add_product", Computed, x.add_product(y, z));
            value!("min", Moved, x.min(y));
            value!("max", Moved, x.max(y));
            value!("clamp", Moved, x.clamp(y, z));
            let mut assigned = x;
            assigned += y;
            assigned -= z;
            assigned *= y;
            assigned /= z;
            value!("assigned", Computed, assigned);

            let eq = x.simd_eq(y);
            let ne = x.simd_ne(y);
            let lt = x.simd_lt(y);
            let le = x.simd_le(y);
            let gt = x.simd_gt(z);
            let ge = x.simd_ge(z);
            for (name, mask) in [
                ("eq", eq),
                ("ne", ne),
                ("lt", lt),
                ("le", le),
                ("gt", gt),
                ("ge", ge),
                ("and", lt & gt),
                ("or", lt | gt),
                ("xor", le ^ ge),
                ("not", !lt),
            ] {
                out.mask(name, mask.bits());
                out.mask("any", u32::from(mask.any()));
                out.mask("all", u32::from(mask.all()));
            }
            value!("select", Moved, lt.select(y, z));
            value!("select combined", Moved, (!eq & ge).select(x, y));

            out.value("reduce_sum", Computed, &[x.reduce_sum()]);
            out.value("reduce_min", Moved, &[x.reduce_min()]);
            out.value("reduce_max", Moved, &[x.reduce_max()]);

            // Loads and stores, the operands as they are.
            value!("splat", Moved, isa.splat(a[block % lanes]));
            value!("zero", Moved, <S::Register<I>>::zero(isa));
            let mut array = S::Lanes::default();
            array.as_mut().copy_from_slice(a);
            value!(
                "from_array",
                Moved,
                <S::Register<I>>::from_array(isa, array)
            );
            // A store into the middle of a longer slice, at an odd offset.
            let mut wide = [S::ONE; 24];
            let offset = 1 + block % 7;
            x.store(&mut wide[offset..]);
            out.value("store", Moved, &wide);
            value!("load unaligned", Moved, isa.load(&wide[offset..]));
            let aligned = Aligned(array);
            let loaded = <S::Register<I>>::load_aligned(isa, &aligned);
            value!("load_aligned", Moved, loaded);
            let mut stored = Aligned(S::Lanes::default());
            y.store_aligned(&mut stored);
            out.value("store_aligned", Moved, stored.0.as_ref());
            let length = block % (lanes + 2);
            let mut longer = [S::ZERO; 10];
            longer[..lanes].copy_from_slice(a);
            longer[lanes] = S::ONE;
            longer[lanes + 1] = S::ONE;
            value!(
                "load_partial",
                Moved,
                isa.load_partial(&longer[..length], c[0])
            );
            let mut part = [S::ONE; 10];
            y.store_partial(&mut part[..length]);
            out.value("store_partial", Moved, &part);
        }
        (isa.backend(), out)
    }
}

/// The definition of every operation, one lane at a time.
fn definition<S: Bits>(a: &[S], b: &[S], c: &[S]) -> Outputs<S> {
    use Kind::{Computed, Moved};
    let lanes = S::LANES;
    let mut out = Outputs {
        values: Vec::new(),
        masks: Vec::new(),
    };
    let lesser = |a: S, b: S| if a < b { a } else { b };
    let greater = |a: S, b: S| if a > b { a } else { b };
    let in_pairs = |v: &[S], combine: &dyn Fn(S, S) -> S| {
        if v.len() == 8 {
            let low = combine(combine(v[0], v[4]), combine(v[2], v[6]));
            let high = combine(combine(v[1], v[5]), combine(v[3], v[7]));
            combine(low, high)
        } else {
            combine(combine(v[0], v[2]), combine(v[1], v[3]))
        }
    };
    for (block, ((a, b), c)) in a
        .chunks_exact(lanes)
        .zip(b.chunks_exact(lanes))
        .zip(c.chunks_exact(lanes))
        .enumerate()
    {
        macro_rules! value {
            ($name:literal, $kind:expr, |$i:ident| $lane:expr) => {{
                let result: Vec<S> = (0..lanes).map(|$i| $lane).collect();
                out.value($name, $kind, &result);
            }};
        }
        value!("add", Computed, |i| a[i] + b[i]);
        value!("sub", Computed, |i| a[i] - b[i]);
        value!("mul", Computed, |i| a[i] * b[i]);
        value!("div", Computed, |i| a[i] / b[i]);
        value!("neg", Moved, |i| -a[i]);
        value!("abs", Moved, |i| a[i].abs());
        value!("sqrt", Computed, |i| a[i].sqrt());
        value!("add_product", Computed, |i| a[i] + b[i] * c[i]);
        value!("min", Moved, |i| lesser(a[i], b[i]));
        value!("max", Moved, |i| greater(a[i], b[i]));
        value!("clamp", Moved, |i| lesser(greater(a[i], b[i]), c[i]));
        value!("assigned", Computed, |i| (a[i] + b[i] - c[i]) * b[i] / c[i]);

        let bits = |test: &dyn Fn(usize) -> bool| {
            (0..lanes).fold(0u32, |bits, i| bits | u32::from(test(i)) << i)
        };
        let masks = [
            ("eq", bits(&|i| a[i] == b[i])),
            ("ne", bits(&|i| a[i] != b[i])),
            ("lt", bits(&|i| a[i] < b[i])),
            ("le", bits(&|i| a[i] <= b[i])),
            ("gt", bits(&|i| a[i] > c[i])),
            ("ge", bits(&|i| a[i] >= c[i])),
            ("and", bits(&|i| a[i] < b[i] && a[i] > c[i])),
            ("or", bits(&|i| a[i] < b[i] || a[i] > c[i])),
            ("xor", bits(&|i| (a[i] <= b[i]) != (a[i] >= c[i]))),
            (
                "not",
                bits(&|i| !matches!(a[i].partial_cmp(&b[i]), Some(core::cmp::Ordering::Less))),
            ),
        ];
        for (name, mask) in masks {
            out.mask(name, mask);
            out.mask("any", u32::from(mask != 0));
            out.mask("all", u32::from(mask == (1 << lanes) - 1));
        }
        value!("select", Moved, |i| if a[i] < b[i] { b[i] } else { c[i] });
        value!("select combined", Moved, |i| {
            if a[i] != b[i] && a[i] >= c[i] {
                a[i]
            } else {
                b[i]
            }
        });

        out.value("reduce_sum", Computed, &[in_pairs(a, &|x, y| x + y)]);
        out.value("reduce_min", Moved, &[in_pairs(a, &lesser)]);
        out.value("reduce_max", Moved, &[in_pairs(a, &greater)]);

        value!("splat", Moved, |_i| a[block % lanes]);
        value!("zero", Moved, |_i| S::ZERO);
        value!("from_array", Moved, |i| a[i]);
        let mut wide = [S::ONE; 24];
        let offset = 1 + block % 7;
        wide[offset..offset + lanes].copy_from_slice(a);
        out.value("store", Moved, &wide);
        value!("load unaligned", Moved, |i| a[i]);
        value!("load_aligned", Moved, |i| a[i]);
        out.value("store_aligned", Moved, b);
        let length = block % (lanes + 2);
        value!("load_partial", Moved, |i| if i < length {
            a[i]
        } else {
            c[0]
        });
        let mut part = [S::ONE; 10];
        let written = length.min(lanes);
        part[..written].copy_from_slice(&b[..written]);
        out.value("store_partial", Moved, &part);
    }
    out
}

/// The operations that pick one of their operands by comparing them.
const PICKS: [&str; 5] = ["min", "max", "clamp", "reduce_min", "reduce_max"];

/// Runs every operation on `backend` and compares it with the definition.
/// Gives the backend that ran, or what first differed.
///
/// Say `flushed` if the thread is flushing subnormals: the [`PICKS`] may
/// then give a subnormal operand back as the zero of its sign.
pub fn every_operation<S: Bits>(
    backend: Backend,
    random_blocks: usize,
    flushed: bool,
) -> Result<Backend, String> {
    let [a, b, c] = operands::<S>(random_blocks);
    let (ran, got) = backend.dispatch(EveryOperation {
        a: &a,
        b: &b,
        c: &c,
    });
    let expected = definition(&a, &b, &c);
    if got.values.len() != expected.values.len() || got.masks.len() != expected.masks.len() {
        return Err("a different set of operations".to_string());
    }
    for ((name, kind, got), (expected_name, _, expected)) in got.values.iter().zip(&expected.values)
    {
        if name != expected_name || got.len() != expected.len() {
            return Err(format!("{name} is out of step with {expected_name}"));
        }
        for (i, (g, e)) in got.iter().zip(expected).enumerate() {
            let same = g.bits() == e.bits()
                || (*kind == Kind::Computed && g.is_nan() && e.is_nan())
                || (flushed
                    && PICKS.contains(name)
                    && g.tiny_sign().is_some()
                    && g.tiny_sign() == e.tiny_sign());
            if !same {
                return Err(format!(
                    "{name} on {}: element {i} is {g:?} ({:#x}), not {e:?} ({:#x})",
                    ran.name(),
                    g.bits(),
                    e.bits()
                ));
            }
        }
    }
    for ((name, got), (expected_name, expected)) in got.masks.iter().zip(&expected.masks) {
        if name != expected_name || got != expected {
            return Err(format!("mask {name} differs on {}", ran.name()));
        }
    }
    Ok(ran)
}

/// A loop as a processor would write one: a register of resonators fed by
/// one signal, each output limited, the sum of their energies kept in a
/// register, and a tail that does not fill one.
pub struct Resonators<'a, S> {
    pub input: &'a [S],
    pub output: &'a mut [S],
}

/// The coefficient of resonator `lane`, between 0 and 1.
fn coefficient<S: SimdSample>(lane: usize) -> S {
    S::from_f64(0.05 + 0.1 * lane as f64 / 3.0)
}

impl<S: SimdSample> Kernel for Resonators<'_, S> {
    type Output = S;

    #[inline(always)]
    fn run<I: Isa>(self, isa: I) -> S {
        let mut coefficients = S::Lanes::default();
        for (lane, c) in coefficients.as_mut().iter_mut().enumerate() {
            *c = coefficient(lane);
        }
        let f = <S::Register<I>>::from_array(isa, coefficients);
        let damping = isa.splat(S::from_f64(0.01));
        let limit = isa.splat(S::from_f64(0.75));
        let mut low = <S::Register<I>>::zero(isa);
        let mut band = <S::Register<I>>::zero(isa);
        let mut energy = <S::Register<I>>::zero(isa);
        for (x, y) in self.input.iter().zip(self.output.iter_mut()) {
            low = low.add_product(f, band);
            let high = isa.splat(*x) - low - damping * band;
            band = band.add_product(f, high);
            let held = band.clamp(-limit, limit);
            energy = energy.add_product(held, held);
            *y = held.reduce_sum();
        }
        // The output again, as a block under a gain, with its tail.
        let gain = isa.splat(S::from_f64(0.125));
        let mut chunks = self.output.chunks_exact_mut(S::LANES);
        for chunk in &mut chunks {
            (isa.load(chunk) * gain).store(chunk);
        }
        let rest = chunks.into_remainder();
        (isa.load_partial(rest, S::ZERO) * gain).store_partial(rest);
        energy.reduce_sum()
    }
}

/// [`Resonators`] in plain Rust, each operation written out.
#[allow(clippy::assign_op_pattern)]
fn resonators_by_definition<S: SimdSample>(input: &[S], output: &mut [S]) -> S {
    let limit = S::from_f64(0.75);
    let damping = S::from_f64(0.01);
    let mut low = [S::ZERO; 8];
    let mut band = [S::ZERO; 8];
    let mut energy = [S::ZERO; 8];
    let lanes = S::LANES;
    let reduce = |mut v: [S; 8]| {
        let mut half = lanes / 2;
        while half > 0 {
            for j in 0..half {
                v[j] = v[j] + v[j + half];
            }
            half /= 2;
        }
        v[0]
    };
    for (x, y) in input.iter().zip(output.iter_mut()) {
        let mut held = [S::ZERO; 8];
        for j in 0..lanes {
            let f = coefficient::<S>(j);
            low[j] = low[j] + f * band[j];
            let high = *x - low[j] - damping * band[j];
            band[j] = band[j] + f * high;
            let above = if band[j] > -limit { band[j] } else { -limit };
            held[j] = if above < limit { above } else { limit };
            energy[j] = energy[j] + held[j] * held[j];
        }
        *y = reduce(held);
    }
    for y in output {
        *y = *y * S::from_f64(0.125);
    }
    reduce(energy)
}

/// Runs [`Resonators`] on `backend` and compares it with plain Rust.
pub fn resonators<S: Bits>(backend: Backend, frames: usize) -> Result<(), String> {
    let mut random = Random(7);
    let input: Vec<S> = (0..frames).map(|_| random.moderate()).collect();
    let mut output = vec![S::ZERO; frames];
    let energy = backend.dispatch(Resonators {
        input: &input,
        output: &mut output,
    });
    let mut expected = vec![S::ZERO; frames];
    let expected_energy = resonators_by_definition(&input, &mut expected);
    if energy.bits() != expected_energy.bits() {
        return Err(format!(
            "energy on {}: {energy:?}, not {expected_energy:?}",
            backend.name()
        ));
    }
    match output
        .iter()
        .zip(&expected)
        .position(|(g, e)| g.bits() != e.bits())
    {
        Some(i) => Err(format!("frame {i} differs on {}", backend.name())),
        None => Ok(()),
    }
}
