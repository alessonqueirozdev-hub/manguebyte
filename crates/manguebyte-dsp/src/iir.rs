//! Designing classic recursive (IIR) filters.
//!
//! A [`Design`] describes a Butterworth, Chebyshev type I, Chebyshev type II
//! or elliptic filter: low-pass, high-pass, band-pass or band-stop, of
//! order 1 to 16.
//! [`Design::sections`] turns it into second-order sections for one sample
//! rate, ready for a [`BiquadCascade`](crate::BiquadCascade).
//! [`IirFilter`](crate::IirFilter) is the processor that does both.
//!
//! ```
//! use manguebyte_dsp::iir::{Band, Design};
//!
//! // An eighth-order Butterworth low-pass at 1 kHz: four sections.
//! let design = Design::butterworth(Band::Lowpass { cutoff_hz: 1_000.0 }, 8);
//! let sections = design.sections(48_000.0);
//! assert_eq!(sections.len(), 4);
//!
//! // 3.01 dB down at the cutoff, and about 48 dB down an octave above it.
//! let db = |hz: f64| 20.0 * sections.magnitude_at(hz, 48_000.0).log10();
//! assert!((db(1_000.0) + 3.0103).abs() < 1e-4);
//! assert!((db(2_000.0) + 48.0).abs() < 1.0);
//! ```
//!
//! # Method
//!
//! The textbook one, as in T. W. Parks and C. S. Burrus, "Digital Filter
//! Design" (Wiley, 1987), chapter 7, and A. V. Oppenheim and R. W. Schafer,
//! "Discrete-Time Signal Processing", section 7.1:
//!
//! 1. The poles and zeros of an analogue low-pass prototype with its edge at
//!    1 rad/s. Butterworth poles lie on the unit circle. Chebyshev type I
//!    poles lie on an ellipse whose shape follows from the ripple. Chebyshev
//!    type II ("inverse Chebyshev") poles are the reciprocals of type I poles
//!    designed for the stop band, and its zeros lie on the imaginary axis.
//!    Elliptic poles and zeros are values of the Jacobi elliptic function
//!    `cd`, as in S. J. Orfanidis, "Lecture Notes on Elliptic Filter Design"
//!    (Rutgers University, 2006): the zeros at `±j / (k·cd(u_i·K, k))`, the
//!    poles at `j·cd((u_i - j·v0)·K, k)` with `u_i = (2i - 1) / N`, the
//!    selectivity `k` from the degree equation, and the functions by the
//!    descending Landen transformation, with the elliptic integrals from
//!    B. C. Carlson's duplication algorithm.
//! 2. A frequency transformation, still in the analogue domain, from the
//!    prototype to the band asked for, with every edge first pre-warped to
//!    `tan(π·f/fs)` so that it lands exactly where it was asked for.
//! 3. The bilinear transform `s = (z - 1) / (z + 1)`, applied to each
//!    second-order factor on its own.
//!
//! Each section is scaled to unity gain at a reference frequency (0 Hz for
//! low-pass and band-stop, half the sample rate for high-pass, the centre of
//! the band for band-pass), and the first one carries the prototype's own
//! gain there, which is below 1 only for a Chebyshev type I or an elliptic
//! design of even order.
//! Sections are ordered from the gentlest to the most resonant.
//!
//! # Conventions
//!
//! - The order is that of the low-pass prototype. A low-pass or high-pass of
//!   order `N` has `N` poles in `ceil(N / 2)` sections; a band-pass or
//!   band-stop of order `N` has `2N` poles in `N` sections.
//! - What "the cutoff" means depends on the family: for Butterworth it is
//!   where the gain is 3.01 dB down; for Chebyshev type I and elliptic, the
//!   edge of the pass band, where the gain is `ripple_db` down for the last
//!   time; for Chebyshev type II, the edge of the stop band, where the gain
//!   first reaches `attenuation_db` down. This is how the classic formulas
//!   and most design tools define them.
//! - Band edges are limited to between [`LOWEST_HZ`] and [`HIGHEST_RATIO`]
//!   of the sample rate, a reversed band is put the right way round, and a
//!   band narrower than [`NARROWEST_RATIO`] is widened to that around its
//!   centre, or, where that would cross a limit, from the limit inwards.
//!   The order is limited to `1..=16`, a ripple to `0.001..=20` dB, and an
//!   attenuation to `1..=200` dB.
//! - The attenuation of an elliptic design is at least 1 dB more than its
//!   ripple, and is raised further, where needed, until no pole of its
//!   prototype has a damping ratio below 1e-8. Only specifications with
//!   little room between ripple and attenuation at a high order need that:
//!   3 dB and 4 dB at order 11 becomes 3 dB and 11.1 dB, and at order 16
//!   the least attenuation is 6.7 dB for 0.1 dB of ripple, 16.2 dB for
//!   1 dB and 22.6 dB for 3 dB. Without it their poles would lie so close
//!   to the imaginary axis that double precision could not place them:
//!   such a filter rings for hours, and its sections round onto the unit
//!   circle. [`Design::family`] gives the attenuation as raised.
//!
//! # Precision
//!
//! A design is computed in double precision, and how closely a filter then
//! follows the formulas depends on the form its sections are held in.
//! [`Sections`] holds them in two.
//!
//! **As coefficients** of a direct form, [`BiquadCoefficients`]: `(b0 +
//! b1·z⁻¹ + b2·z⁻²) / (1 + a1·z⁻¹ + a2·z⁻²)`, each rounded to its last
//! digit. That moves the gain at a frequency, relatively, by about `1e-16`
//! times the sum over the sections of `(1 + |a1| + |a2|) / |1 + a1·z⁻¹ +
//! a2·z⁻²|` and `(|b0| + |b1| + |b2|) / |b0 + b1·z⁻¹ + b2·z⁻²|`, with
//! `z = e^(jω)`. The second term is large only near a zero, where the gain
//! is small anyway. The first is large near a pole close to the unit
//! circle, where the denominator comes down to about `2·d·m`, `d` being
//! the pole's distance from the circle and `m` its distance from `z = 1`
//! (0 Hz) or `z = -1` (half the sample rate), whichever is nearer; for a
//! pair of real poles near 0 Hz, to the product of their distances from
//! `z = 1`. So the error grows as `1/d` for a resonance in the middle of
//! the band, but as `1/d²` for poles near 0 Hz or half the sample rate,
//! which is where a low-pass at a few hertz puts them at a high sample
//! rate. However they round, the coefficients are kept strictly stable: a
//! section whose rounding would put a pole on or beyond the circle has its
//! denominator moved inwards by as few units of the last digit as it
//! takes.
//!
//! **In the form a [`BiquadCascade`](crate::BiquadCascade) runs them**:
//! the gain `g = tan(π·f/fs)` of each section's poles, its damping and its
//! output weights, worked out from the analogue section. These are
//! quotients, exact to the last digit, where the coefficients hold `g²`
//! only as the small difference `1 + a1 + a2`. What is left is the rounding
//! of the gains each step of the cascade computes from them, which moves a
//! pole by about `1e-16` of its distance `d` from the circle: the error
//! grows as `1/d` only. [`IirFilter`](crate::IirFilter) and
//! [`BiquadCascade::from_sections`](crate::BiquadCascade::from_sections)
//! use this form; [`BiquadCascade::new`](crate::BiquadCascade::new), handed
//! the coefficients, can only use those.
//!
//! Measured over 67,200 designs (every family with several ripples and
//! attenuations, type II up to 200 dB and the loosest elliptic
//! specifications allowed; orders 1, 2, 3, 4, 5, 8, 11 and 16; band edges
//! from 1 Hz to 0.49 of the sample rate at 44.1, 48, 96 and 192 kHz; bands
//! down to 0.1 % wide), every pole of the coefficients is strictly inside
//! the unit circle. Wherever the sum above is below 1 %, the gain of the
//! coefficients is within 4 times it of the formula, relatively, where the
//! formula is above -40 dB, and within twice it in plain gain everywhere
//! give or take `1e-11`, which is as far as the formulas themselves can be
//! trusted. The largest ratios seen depend on the platform's mathematics
//! library: 3.5 and 1.8 on Windows, 3.7 and 1.4 on Linux. The gain of the
//! cascade's form, computed, is within `2e-15 / d` of the formula above
//! -40 dB (the largest seen was `1.0e-15 / d`). Some examples, with the
//! largest error above -40 dB, `d` being that of the cascade's form:
//!
//! | Design | `d` | Coefficients | Cascade's form |
//! |---|---|---|---|
//! | 8th-order Butterworth low-pass, 1 kHz at 48 kHz | 0.025 | 1e-13 dB | below 1e-13 dB |
//! | 4th-order Butterworth high-pass, 20 Hz at 48 kHz | 0.001 | 1e-10 dB | below 1e-13 dB |
//! | 2nd-order Butterworth low-pass, 1 Hz at 48 kHz | 9.3e-5 | 7e-8 dB | below 1e-13 dB |
//! | 16th-order Chebyshev I (0.5 dB) low-pass, 10 Hz at 192 kHz | 3.6e-6 | 3e-7 dB | below 1e-13 dB |
//! | 2nd-order Chebyshev II (140 dB) low-pass, 10 Hz at 192 kHz | 1.0e-7 | 0.04 dB | below 1e-13 dB |
//! | 2nd-order Chebyshev II (200 dB) low-pass, 10 Hz at 192 kHz | 3.3e-9 | 14 dB | below 1e-13 dB |
//! | 16th-order Butterworth band-pass, 1 to 1.001 Hz at 192 kHz | 1.6e-9 | 0.002 dB | 5e-12 dB |
//! | 1st-order Chebyshev II (100 dB) band-pass, 1 to 1.001 Hz at 192 kHz | 1.6e-13 | 23 dB | below 1e-13 dB |
//!
//! The two type II low-passes have their poles at 0.0045 Hz and
//! 0.00014 Hz; at 200 dB, `1 + a1 + a2` would be 2e-17, below the last
//! digit of `a1`. The cascade's own rounding (`1e-16 / d`) is too small to
//! show in any of these but the last, whose peak it moves by about
//! 0.005 dB. Measured on impulse responses, a cascade made from the
//! sections of the 2nd-order Butterworth low-pass at 1 Hz follows the
//! formula to within `1e-12`, while one made from their coefficients is
//! off by the `8e-9` the coefficients are.
//!
//! So the coefficients are enough while `d·m` stays above about `1e-12`
//! (an error below `1e-4`), and fall apart below about `1e-16`: a type II
//! low-pass whose pass band is a small fraction of a hertz (a high
//! attenuation at a low order and a low cutoff), or a band a fraction of a
//! hertz wide at a few hertz. The cascade's form is enough while `d` stays
//! above about `1e-12`. Below about `1e-15` neither form can hold a pole to
//! better than a tenth of its distance from the circle, and below `1e-16`
//! not at all: the coefficients keep it just inside the circle, and the
//! cascade, whose step then loses the damping to rounding, runs it on the
//! edge of stability. Of the designs measured, 88 are there: band-passes and band-stops 0.1 % wide at 1 Hz
//! with an elliptic specification whose prototype has a pole damped by
//! less than 1e-7, and first-order type II band-passes with 200 dB of
//! attenuation, at most 10 % wide, at 100 Hz and below. The sections must
//! also be *run* in double precision; see
//! [`BiquadCascade`](crate::BiquadCascade).

use core::f64::consts::{LN_10, PI};
use core::ops::Deref;

use crate::BiquadCoefficients;
use crate::cascade::{self, Row};
use crate::curve::CascadeResponse;
use crate::elliptic::{Elliptic, least_attenuation};
use crate::fft::Complex;
use crate::numeric::limit;

/// Lowest band edge, in hertz.
pub const LOWEST_HZ: f64 = 1.0;

/// Highest band edge, as a fraction of the sample rate.
pub const HIGHEST_RATIO: f64 = 0.49;

/// Narrowest band, as the ratio of its upper edge to its lower edge.
pub const NARROWEST_RATIO: f64 = 1.001;

/// Highest order of a design.
pub const MAX_ORDER: usize = 16;

/// Most sections a design produces: a band-pass or band-stop of the
/// highest order.
pub const MAX_SECTIONS: usize = MAX_ORDER;

/// The shape of a design's response: how it trades flatness for steepness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Family {
    /// Maximally flat: no ripple anywhere, 3.01 dB down at the cutoff, and
    /// falling by `6·order` dB per octave far beyond it.
    Butterworth,
    /// Ripples in the pass band, between 0 and `-ripple_db`, and falls
    /// faster than Butterworth beyond the cutoff, which is the edge of the
    /// pass band.
    ChebyshevI {
        /// Depth of the ripple in the pass band, in decibels.
        ripple_db: f64,
    },
    /// Flat in the pass band and ripples in the stop band, never rising
    /// above `-attenuation_db` there. The cutoff is the edge of the stop
    /// band.
    ChebyshevII {
        /// How far down the stop band is held, in decibels.
        attenuation_db: f64,
    },
    /// Elliptic, or Cauer: ripples in both bands, between 0 and
    /// `-ripple_db` in the pass band and never above `-attenuation_db` in
    /// the stop band, with the narrowest transition between them that any
    /// filter of the order can have. The cutoff is the edge of the pass
    /// band; the stop band begins where the pre-warped frequency is `1/k`
    /// times that of the cutoff, with `k` set by the order, the ripple and
    /// the attenuation (the degree equation).
    Elliptic {
        /// Depth of the ripple in the pass band, in decibels.
        ripple_db: f64,
        /// How far down the stop band is held, in decibels.
        attenuation_db: f64,
    },
}

/// The frequencies a design passes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Band {
    /// Passes what is below the cutoff.
    Lowpass {
        /// The cutoff, in hertz.
        cutoff_hz: f64,
    },
    /// Passes what is above the cutoff.
    Highpass {
        /// The cutoff, in hertz.
        cutoff_hz: f64,
    },
    /// Passes what is between the two edges.
    Bandpass {
        /// The lower edge, in hertz.
        low_hz: f64,
        /// The upper edge, in hertz.
        high_hz: f64,
    },
    /// Removes what is between the two edges.
    Bandstop {
        /// The lower edge, in hertz.
        low_hz: f64,
        /// The upper edge, in hertz.
        high_hz: f64,
    },
}

/// A classic IIR filter: a family, a band and an order.
///
/// It holds no sample rate: [`sections`](Self::sections) designs the
/// filter for one, and [`magnitude_at`](Self::magnitude_at) gives the
/// response the formulas promise, which the sections can be checked
/// against. Both are real-time safe: they allocate nothing.
///
/// ```
/// use manguebyte_dsp::iir::{Band, Design};
///
/// // A sixth-order Chebyshev type II band-stop: nothing between 900 and
/// // 1100 Hz comes through louder than 60 dB down.
/// let band = Band::Bandstop { low_hz: 900.0, high_hz: 1_100.0 };
/// let design = Design::chebyshev_ii(band, 6, 60.0);
/// let sections = design.sections(44_100.0);
/// assert_eq!(sections.len(), 6);
/// for hz in [900.0, 950.0, 1_000.0, 1_050.0, 1_100.0] {
///     assert!(sections.magnitude_at(hz, 44_100.0) <= 1e-3 * (1.0 + 1e-9));
/// }
/// assert!((sections.magnitude_at(100.0, 44_100.0) - 1.0).abs() < 1e-3);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Design {
    family: Family,
    band: Band,
    order: usize,
}

impl Design {
    /// A design of `family`, passing `band`, of order `order`. See the
    /// [module documentation](self#conventions) for what is limited and
    /// how.
    ///
    /// Real-time safe: it allocates nothing. An elliptic specification is
    /// checked against the least damping its poles may have, which costs
    /// one prototype; one that has to be given more attenuation costs about
    /// thirty, to find how much.
    pub fn new(family: Family, band: Band, order: usize) -> Self {
        let order = order.clamp(1, MAX_ORDER);
        let family = match family {
            Family::Butterworth => Family::Butterworth,
            Family::ChebyshevI { ripple_db } => Family::ChebyshevI {
                ripple_db: limit(ripple_db, 0.001, 20.0),
            },
            Family::ChebyshevII { attenuation_db } => Family::ChebyshevII {
                attenuation_db: limit(attenuation_db, 1.0, 200.0),
            },
            Family::Elliptic {
                ripple_db,
                attenuation_db,
            } => {
                let ripple_db = limit(ripple_db, 0.001, 20.0);
                let attenuation_db = limit(attenuation_db, ripple_db + 1.0, 200.0);
                Family::Elliptic {
                    ripple_db,
                    attenuation_db: least_attenuation(order, ripple_db, attenuation_db, 200.0),
                }
            }
        };
        Self {
            family,
            band,
            order,
        }
    }

    /// A Butterworth design.
    pub fn butterworth(band: Band, order: usize) -> Self {
        Self::new(Family::Butterworth, band, order)
    }

    /// A Chebyshev type I design with `ripple_db` of ripple in the pass
    /// band.
    pub fn chebyshev_i(band: Band, order: usize, ripple_db: f64) -> Self {
        Self::new(Family::ChebyshevI { ripple_db }, band, order)
    }

    /// A Chebyshev type II design whose stop band is `attenuation_db` down.
    pub fn chebyshev_ii(band: Band, order: usize, attenuation_db: f64) -> Self {
        Self::new(Family::ChebyshevII { attenuation_db }, band, order)
    }

    /// An elliptic design with `ripple_db` of ripple in the pass band and
    /// its stop band `attenuation_db` down.
    ///
    /// ```
    /// use manguebyte_dsp::iir::{Band, Design};
    ///
    /// // Fifth order: 0.5 dB of ripple up to 1 kHz, and 60 dB down from
    /// // a little below 1.8 kHz.
    /// let design = Design::elliptic(Band::Lowpass { cutoff_hz: 1_000.0 }, 5, 0.5, 60.0);
    /// let sections = design.sections(48_000.0);
    /// let db = |hz: f64| 20.0 * sections.magnitude_at(hz, 48_000.0).log10();
    /// assert!(db(500.0) <= 1e-6 && db(500.0) >= -0.5 - 1e-6);
    /// assert!((db(1_000.0) + 0.5).abs() < 1e-6);
    /// assert!(db(1_800.0) <= -60.0 + 1e-6);
    /// ```
    pub fn elliptic(band: Band, order: usize, ripple_db: f64, attenuation_db: f64) -> Self {
        Self::new(
            Family::Elliptic {
                ripple_db,
                attenuation_db,
            },
            band,
            order,
        )
    }

    /// The family, with its parameters as limited: for an elliptic design,
    /// the attenuation as raised to keep its poles damped (see the
    /// [module documentation](self#conventions)).
    pub fn family(&self) -> Family {
        self.family
    }

    /// The band, as it was given. The limits are applied for a sample rate,
    /// by [`sections`](Self::sections) and [`magnitude_at`](Self::magnitude_at).
    pub fn band(&self) -> Band {
        self.band
    }

    /// The order, as limited.
    pub fn order(&self) -> usize {
        self.order
    }

    /// How many sections [`sections`](Self::sections) produces.
    pub fn num_sections(&self) -> usize {
        match self.band {
            Band::Lowpass { .. } | Band::Highpass { .. } => self.order.div_ceil(2),
            Band::Bandpass { .. } | Band::Bandstop { .. } => self.order,
        }
    }

    /// The second-order sections of the filter at `sample_rate`.
    /// Real-time safe.
    pub fn sections(&self, sample_rate: f64) -> Sections {
        let edges = Edges::new(self.band, sample_rate);
        let prototype = Prototype::new(self.family, self.order);
        let mut sections = Sections::EMPTY;
        let mut push = |analog: Analog| {
            if sections.len < MAX_SECTIONS {
                sections.sections[sections.len] = analog.bilinear();
                sections.rows[sections.len] = analog.state_variable();
                sections.len += 1;
            }
        };
        // The real pole first, then the pairs from the gentlest to the most
        // resonant.
        if let Some(pole) = prototype.real {
            push(edges.real_section(pole));
        }
        for &(pole, zero) in prototype.pairs[..prototype.count].iter().rev() {
            let (first, second) = edges.pair_sections(pole, zero);
            push(first);
            if let Some(second) = second {
                push(second);
            }
        }
        // The prototype's gain at the reference frequency goes into the
        // first section.
        if prototype.gain != 1.0 {
            let first = &mut sections.sections[0];
            first.b0 *= prototype.gain;
            first.b1 *= prototype.gain;
            first.b2 *= prototype.gain;
            for weight in &mut sections.rows[0][2..] {
                *weight *= prototype.gain;
            }
        }
        sections
    }

    /// The frequency response of the filter at `sample_rate`, for
    /// drawing: that of its [`sections`](Self::sections) as an
    /// [`IirFilter`](crate::IirFilter) runs them. Real-time safe. See
    /// [`curve`](crate::curve).
    pub fn response(&self, sample_rate: f64) -> CascadeResponse {
        self.sections(sample_rate).response(sample_rate)
    }

    /// The gain the formulas of the family give at `frequency`, for a
    /// filter at `sample_rate`: what [`sections`](Self::sections) should
    /// realise, independently of how it does it.
    ///
    /// With `x` the frequency mapped onto the low-pass prototype (pre-warped
    /// and transformed to the band) and `T_N` the Chebyshev polynomial of
    /// the order, the gain is `1 / sqrt(1 + x^(2N))` for Butterworth,
    /// `1 / sqrt(1 + ε²·T_N(x)²)` for type I,
    /// `1 / sqrt(1 + 1 / (ε²·T_N(1/x)²))` for type II and
    /// `1 / sqrt(1 + ε²·R_N(x)²)` for elliptic, `R_N` being the elliptic
    /// rational function. It can be trusted to about `1e-11`, relatively:
    /// mapping a frequency onto a narrow band subtracts numbers close to
    /// each other.
    pub fn magnitude_at(&self, frequency: f64, sample_rate: f64) -> f64 {
        let edges = Edges::new(self.band, sample_rate);
        let w = (PI * limit(frequency, 0.0, sample_rate / 2.0) / sample_rate).tan();
        let x = edges.prototype_frequency(w);
        let order = self.order as i32;
        match self.family {
            Family::Butterworth => 1.0 / (1.0 + x.powi(2 * order)).sqrt(),
            Family::ChebyshevI { ripple_db } => {
                let t = chebyshev_polynomial(order, x);
                1.0 / (1.0 + epsilon_squared(ripple_db) * t * t).sqrt()
            }
            Family::ChebyshevII { attenuation_db } => {
                let t = chebyshev_polynomial(order, 1.0 / x);
                let e2 = 1.0 / epsilon_squared(attenuation_db);
                if t == 0.0 {
                    0.0
                } else {
                    1.0 / (1.0 + 1.0 / (e2 * t * t)).sqrt()
                }
            }
            Family::Elliptic {
                ripple_db,
                attenuation_db,
            } => {
                let prototype = Elliptic::new(self.order, ripple_db, attenuation_db);
                let r = prototype.rational(x);
                1.0 / (1.0 + prototype.ripple * r * r).sqrt()
            }
        }
    }
}

/// `10^(db/10) - 1`: the `ε²` of a ripple of `db` decibels.
fn epsilon_squared(db: f64) -> f64 {
    (db * LN_10 / 10.0).exp_m1()
}

/// The Chebyshev polynomial of the first kind, `T_n(x)`, up to its sign
/// for negative `x`.
fn chebyshev_polynomial(n: i32, x: f64) -> f64 {
    let x = x.abs();
    if x <= 1.0 {
        (f64::from(n) * x.acos()).cos()
    } else {
        (f64::from(n) * x.acosh()).cosh()
    }
}

/// The second-order sections of a design, for one sample rate.
///
/// A fixed-size list, so making one never allocates. It dereferences to a
/// slice of [`BiquadCoefficients`]: the design rounded to the coefficients
/// of a direct form, which every pole of is strictly inside the unit
/// circle. It also holds each section in the form a
/// [`BiquadCascade`](crate::BiquadCascade) runs it, worked out from the
/// design directly; [`BiquadCascade::from_sections`](crate::BiquadCascade::from_sections)
/// uses that, and so does [`IirFilter`](crate::IirFilter). See the
/// [module documentation](self#precision) for what the difference is worth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sections {
    sections: [BiquadCoefficients; MAX_SECTIONS],
    /// The same sections in state-variable form, from the analogue
    /// sections rather than from the coefficients above.
    rows: [Row; MAX_SECTIONS],
    len: usize,
}

impl Sections {
    const EMPTY: Self = Self {
        sections: [BiquadCoefficients::IDENTITY; MAX_SECTIONS],
        rows: [cascade::IDENTITY; MAX_SECTIONS],
        len: 0,
    };

    /// The sections, in the order they are to be run.
    pub fn as_slice(&self) -> &[BiquadCoefficients] {
        &self.sections[..self.len]
    }

    /// The sections in state-variable form, in the order they are to be
    /// run.
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows[..self.len]
    }

    /// The gain of all the sections in a row at `frequency`.
    ///
    /// Each section is evaluated in a form that keeps its digits where the
    /// plain one, `1 + a1·cos(ω) + a2·cos(2ω)` and so on, cancels them
    /// away: close to 0 Hz and close to half the sample rate, where a
    /// low-frequency design has its poles and a high-frequency design its
    /// zeros. That makes this the response of the coefficients as they are
    /// stored, to nearly the last digit.
    pub fn magnitude_at(&self, frequency: f64, sample_rate: f64) -> f64 {
        let omega = 2.0 * PI * frequency / sample_rate;
        self.as_slice()
            .iter()
            .map(|section| {
                let numerator = polynomial_size(section.b0, section.b1, section.b2, omega);
                let denominator = polynomial_size(1.0, section.a1, section.a2, omega);
                numerator / denominator
            })
            .product()
    }

    /// The distance from the origin of the pole furthest out. Below 1 the
    /// filter is stable; how far below says how long it rings.
    ///
    /// It is computed without the cancellation that the roots of `z² +
    /// a1·z + a2` suffer near 0 Hz. A pole closer to the circle than about
    /// `1e-16` gives 1, which the sections of a design never do.
    pub fn largest_pole_radius(&self) -> f64 {
        self.as_slice().iter().map(pole_radius).fold(0.0, f64::max)
    }

    /// The frequency response of the sections run at `sample_rate` by a
    /// [`BiquadCascade`](crate::BiquadCascade) made from them, for
    /// drawing. See [`curve`](crate::curve).
    pub fn response(&self, sample_rate: f64) -> CascadeResponse {
        CascadeResponse::from_rows(self.rows(), sample_rate)
    }
}

impl Deref for Sections {
    type Target = [BiquadCoefficients];

    fn deref(&self) -> &[BiquadCoefficients] {
        self.as_slice()
    }
}

/// `|c0 + c1·e^(-jω) + c2·e^(-2jω)|`, which is `|(c0 + c2)·cos ω + c1 +
/// j·(c0 - c2)·sin ω|`.
///
/// The real part is expanded around whichever of 0 Hz and half the sample
/// rate is nearer: `((c0 + c1) + c2) - (c0 + c2)·2·sin²(ω/2)` or `((c1 -
/// c0) - c2) + (c0 + c2)·2·cos²(ω/2)`. Where it matters, near a pole or a
/// zero close to one of those frequencies, the sums are exact in that order:
/// each adds numbers of opposite sign within a factor of two of each other
/// (Sterbenz's lemma). So is `c0 - c2` for a denominator. The squared sines
/// are small numbers known to full precision, so nothing cancels that
/// matters.
fn polynomial_size(c0: f64, c1: f64, c2: f64, omega: f64) -> f64 {
    let outer = c0 + c2;
    let real = if omega < PI / 2.0 {
        let half = (omega / 2.0).sin();
        ((c0 + c1) + c2) - outer * 2.0 * half * half
    } else {
        let half = (omega / 2.0).cos();
        ((c1 - c0) - c2) + outer * 2.0 * half * half
    };
    real.hypot((c0 - c2) * omega.sin())
}

/// The denominator of a section mapped back through the bilinear transform,
/// `A·s² + B·s + C`: `[A, B, C] = [1 - a1 + a2, 2·(1 - a2), 1 + a1 + a2]`.
///
/// The section is strictly stable exactly when all three are above zero
/// (Jury's conditions). Wherever one of them is close to zero, the sums
/// taken in this order are exact (Sterbenz's lemma), so the test is exact
/// for the coefficients as they are stored.
fn denominator(section: &BiquadCoefficients) -> [f64; 3] {
    let (a1, a2) = (section.a1, section.a2);
    [(1.0 - a1) + a2, 2.0 * (1.0 - a2), (1.0 + a1) + a2]
}

/// How far inside the unit circle the pole of one section that is nearest
/// to it lies: `1 - |z|`. Zero or less for a section that is not strictly
/// stable.
///
/// The poles are found as roots `s` of `A·s² + B·s + C` (see
/// [`denominator`]) and mapped to `z = (1 + s) / (1 - s)`, and `1 - |z|`
/// is written so that nothing cancels: `-2s / (1 - s)` for a real `s`
/// above -1, `2 / (1 - s)` below it, and `-4·Re(s) / (|1 - s|·(|1 - s| +
/// |1 + s|))` for a complex one. The plain route, the roots of `z² + a1·z +
/// a2`, loses all its digits for a real pole pair near 0 Hz: the
/// discriminant `a1² - 4·a2` is then the difference of two numbers near 4.
pub(crate) fn pole_distance(section: &BiquadCoefficients) -> f64 {
    let [a, b, c] = denominator(section);
    if !(a > 0.0 && b > 0.0 && c > 0.0) {
        // A pole on or beyond the circle; the plain radius says how far.
        return (1.0 - plain_pole_radius(section)).min(0.0);
    }
    roots_distance(a, b, c)
}

/// How far inside the unit circle the roots of `A·s² + B·s + C`, all three
/// above zero, lie once mapped to `z = (1 + s) / (1 - s)`: see
/// [`pole_distance`].
fn roots_distance(a: f64, b: f64, c: f64) -> f64 {
    let discriminant = b * b - 4.0 * a * c;
    if discriminant >= 0.0 {
        let q = -(b + discriminant.sqrt()) / 2.0;
        [q / a, c / q]
            .into_iter()
            .map(|s| {
                if s > -1.0 {
                    -2.0 * s / (1.0 - s)
                } else {
                    2.0 / (1.0 - s)
                }
            })
            .fold(f64::INFINITY, f64::min)
    } else {
        let (re, im) = (-b / (2.0 * a), (-discriminant).sqrt() / (2.0 * a));
        let (below, above) = ((1.0 - re).hypot(im), (1.0 + re).hypot(im));
        -4.0 * re / (below * (below + above))
    }
}

/// The distance from the origin of the further pole of one section: `1 -`
/// [`pole_distance`] for a stable section, which rounds to 1 for a pole
/// closer to the circle than about 1e-16.
pub(crate) fn pole_radius(section: &BiquadCoefficients) -> f64 {
    let distance = pole_distance(section);
    if distance > 0.0 {
        1.0 - distance
    } else {
        plain_pole_radius(section).max(1.0)
    }
}

/// The radius of the further root of `z² + a1·z + a2`, the plain way.
fn plain_pole_radius(section: &BiquadCoefficients) -> f64 {
    let (a1, a2) = (section.a1, section.a2);
    let discriminant = a1 * a1 - 4.0 * a2;
    if discriminant < 0.0 {
        a2.sqrt()
    } else {
        (a1.abs() + discriminant.sqrt()) / 2.0
    }
}

/// The next number above `x`, for a finite `x`. (`f64::next_up` is newer
/// than the Rust this crate supports.)
fn next_up(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let bits = x.to_bits();
    f64::from_bits(if x > 0.0 { bits + 1 } else { bits - 1 })
}

/// The next number below `x`, for a finite `x`.
fn next_down(x: f64) -> f64 {
    -next_up(-x)
}

/// `section` with its denominator moved by as few units of the last digit
/// as it takes for every pole to be strictly inside the unit circle, with a
/// radius below 1 even once rounded, and by the textbook formula too.
///
/// The bilinear transform of a stable analogue section is stable, but
/// rounded to `a1` and `a2` a pole within about 1e-16 of the circle, or a
/// real pole pair whose `1 + a1 + a2` is below the last digit of `a1` (a
/// pass band a fraction of a hertz wide, near 0 Hz), can land on it or
/// beyond. The response is then far from the design's anyway (see the
/// module documentation); this keeps it stable. Moving `a2` down moves a
/// complex pair inwards; moving `a1` up moves a pole near `z = 1` inwards,
/// and down one near `z = -1`. The last of these also settles the
/// textbook radius `(|a1| + sqrt(a1² - 4·a2)) / 2`, whose discriminant near
/// 0 Hz is the difference of two numbers near 4 and can round to above
/// zero for a pair that is complex and inside: a step of `a1` takes the
/// pair further from that edge. A first-order section, whose `a2` is 0,
/// only ever has `a1` moved.
fn stabilised(mut section: BiquadCoefficients) -> BiquadCoefficients {
    for _ in 0..64 {
        let [a, b, c] = denominator(&section);
        // Each of Jury's conditions; not a number counts as failing.
        let (a_holds, b_holds, c_holds) = (a > 0.0, b > 0.0, c > 0.0);
        let inside = a_holds && b_holds && c_holds;
        if inside && pole_radius(&section) < 1.0 && plain_pole_radius(&section) < 1.0 {
            break;
        }
        let complex = inside && b * b < 4.0 * a * c;
        if !b_holds || (complex && pole_radius(&section) >= 1.0) {
            section.a2 = next_down(section.a2);
        } else if !c_holds || (a_holds && c <= a) {
            section.a1 = next_up(section.a1);
        } else {
            section.a1 = next_down(section.a1);
        }
    }
    section
}

type C64 = Complex<f64>;

fn divide(a: C64, b: C64) -> C64 {
    let size = b.norm_sqr();
    C64::new(
        (a.re * b.re + a.im * b.im) / size,
        (a.im * b.re - a.re * b.im) / size,
    )
}

/// The square root with a non-negative real part, computed without
/// cancellation on either side of the imaginary axis.
fn square_root(a: C64) -> C64 {
    if a.re == 0.0 && a.im == 0.0 {
        return a;
    }
    let size = a.norm();
    if a.re >= 0.0 {
        let re = ((size + a.re) / 2.0).sqrt();
        C64::new(re, a.im / (2.0 * re))
    } else {
        let im = ((size - a.re) / 2.0).sqrt().copysign(a.im);
        C64::new(a.im / (2.0 * im), im)
    }
}

/// The roots of `s² - c·s + w2`, the larger first, computed so that
/// neither loses digits to cancellation: the larger by adding numbers that
/// point the same way, the smaller from the product of the two, `w2`.
fn quadratic_roots(c: C64, w2: f64) -> (C64, C64) {
    let half = c.scale(0.5);
    let mut root = square_root(half * half - C64::new(w2, 0.0));
    if half.re * root.re + half.im * root.im < 0.0 {
        root = -root;
    }
    let large = half + root;
    (large, divide(C64::new(w2, 0.0), large))
}

/// The analogue low-pass prototype, with its edge at 1 rad/s.
struct Prototype {
    /// One pole of each conjugate pair (the one above the real axis), with
    /// the height on the imaginary axis of the zero pair that goes with it,
    /// if it has one.
    pairs: [(C64, Option<f64>); MAX_ORDER / 2],
    count: usize,
    /// The real pole of an odd order.
    real: Option<f64>,
    /// The gain at 0 rad/s.
    gain: f64,
}

impl Prototype {
    fn new(family: Family, order: usize) -> Self {
        let n = order as f64;
        // Butterworth poles lie on the unit circle. For type I, `sinh μ`
        // and `cosh μ` squash that circle into an ellipse; type II uses the
        // same ellipse, designed for its stop band, and inverts it.
        let ellipse = |mu: f64| (mu.sinh(), mu.cosh());
        let (sinh, cosh) = match family {
            Family::Butterworth => (1.0, 1.0),
            Family::ChebyshevI { ripple_db } => {
                ellipse((1.0 / epsilon_squared(ripple_db).sqrt()).asinh() / n)
            }
            Family::ChebyshevII { attenuation_db } => {
                ellipse(epsilon_squared(attenuation_db).sqrt().asinh() / n)
            }
            Family::Elliptic {
                ripple_db,
                attenuation_db,
            } => return Self::elliptic(order, ripple_db, attenuation_db),
        };
        let mut prototype = Self {
            pairs: [(C64::ZERO, None); MAX_ORDER / 2],
            count: order / 2,
            real: None,
            gain: 1.0,
        };
        for k in 0..order / 2 {
            let theta = (2 * k + 1) as f64 * PI / (2.0 * n);
            let (sin, cos) = theta.sin_cos();
            // On the ellipse (the unit circle for Butterworth).
            let ellipse = C64::new(-sinh * sin, cosh * cos);
            prototype.pairs[k] = match family {
                Family::ChebyshevII { .. } => {
                    // The reciprocal, reflected back above the real axis,
                    // and a zero where T_N(1/x) is zero.
                    (ellipse.scale(1.0 / ellipse.norm_sqr()), Some(1.0 / cos))
                }
                _ => (ellipse, None),
            };
        }
        if order % 2 == 1 {
            prototype.real = Some(match family {
                Family::Butterworth => -1.0,
                Family::ChebyshevI { .. } => -sinh,
                // (An elliptic prototype was made above.)
                Family::ChebyshevII { .. } | Family::Elliptic { .. } => -1.0 / sinh,
            });
        }
        if let Family::ChebyshevI { ripple_db } = family {
            if order % 2 == 0 {
                prototype.gain = 1.0 / (1.0 + epsilon_squared(ripple_db)).sqrt();
            }
        }
        prototype
    }

    /// The elliptic prototype: its poles, and zeros at `±j / (k·ζ_i)`.
    fn elliptic(order: usize, ripple_db: f64, attenuation_db: f64) -> Self {
        let elliptic = Elliptic::new(order, ripple_db, attenuation_db);
        let mut prototype = Self {
            pairs: [(C64::ZERO, None); MAX_ORDER / 2],
            count: elliptic.count,
            real: elliptic.real,
            gain: elliptic.gain(),
        };
        let k = elliptic.selectivity.k;
        for (pair, (&pole, &zero)) in prototype
            .pairs
            .iter_mut()
            .zip(elliptic.poles.iter().zip(&elliptic.zeros))
            .take(elliptic.count)
        {
            *pair = (pole, Some(1.0 / (k * zero)));
        }
        prototype
    }
}

/// The band's edges, pre-warped: `tan(π·f/fs)`.
#[derive(Debug, Clone, Copy)]
enum Edges {
    Lowpass(f64),
    Highpass(f64),
    /// The centre squared and the width.
    Bandpass(f64, f64),
    /// The centre squared and the width.
    Bandstop(f64, f64),
}

impl Edges {
    fn new(band: Band, sample_rate: f64) -> Self {
        let sample_rate = if sample_rate > 0.0 && sample_rate.is_finite() {
            sample_rate
        } else {
            44_100.0
        };
        let highest = HIGHEST_RATIO * sample_rate;
        let lowest = LOWEST_HZ.min(highest / 2.0);
        let warp = |hz: f64| (PI * hz / sample_rate).tan();
        let edge = |hz: f64| warp(limit(hz, lowest, highest));
        let pair = |low: f64, high: f64| {
            let (low, high) = (limit(low, lowest, highest), limit(high, lowest, highest));
            let (mut low, mut high) = (low.min(high), low.max(high));
            if high < low * NARROWEST_RATIO {
                // Widened around its centre, or from a limit inwards where
                // that would cross it.
                let centre = (low * high).sqrt();
                let half = NARROWEST_RATIO.sqrt();
                (low, high) = if centre * half > highest {
                    (highest / NARROWEST_RATIO, highest)
                } else if centre / half < lowest {
                    (lowest, lowest * NARROWEST_RATIO)
                } else {
                    (centre / half, centre * half)
                };
            }
            let (low, high) = (warp(low), warp(high));
            (low * high, high - low)
        };
        match band {
            Band::Lowpass { cutoff_hz } => Self::Lowpass(edge(cutoff_hz)),
            Band::Highpass { cutoff_hz } => Self::Highpass(edge(cutoff_hz)),
            Band::Bandpass { low_hz, high_hz } => {
                let (centre2, width) = pair(low_hz, high_hz);
                Self::Bandpass(centre2, width)
            }
            Band::Bandstop { low_hz, high_hz } => {
                let (centre2, width) = pair(low_hz, high_hz);
                Self::Bandstop(centre2, width)
            }
        }
    }

    /// The prototype frequency a pre-warped frequency `w` is mapped to.
    fn prototype_frequency(self, w: f64) -> f64 {
        match self {
            Self::Lowpass(cutoff) => w / cutoff,
            Self::Highpass(cutoff) => cutoff / w,
            Self::Bandpass(centre2, width) => (w * w - centre2).abs() / (width * w),
            Self::Bandstop(centre2, width) => width * w / (w * w - centre2).abs(),
        }
    }

    /// The section a real prototype pole `pole` turns into.
    fn real_section(self, pole: f64) -> Analog {
        let (numerator, denominator) = match self {
            Self::Lowpass(cutoff) => ([0.0, 0.0, 1.0], [0.0, 1.0, -pole * cutoff]),
            Self::Highpass(cutoff) => ([0.0, 1.0, 0.0], [0.0, 1.0, -cutoff / pole]),
            Self::Bandpass(centre2, width) => ([0.0, 1.0, 0.0], [1.0, -width * pole, centre2]),
            Self::Bandstop(centre2, width) => ([1.0, 0.0, centre2], [1.0, -width / pole, centre2]),
        };
        Analog::new(numerator, denominator).normalised(self)
    }

    /// The sections a conjugate pair of prototype poles turns into, with
    /// its pair of zeros at `±j·zero` if it has one: one section for a
    /// low-pass or high-pass, two for a band-pass or band-stop.
    fn pair_sections(self, pole: C64, zero: Option<f64>) -> (Analog, Option<Analog>) {
        let size2 = pole.norm_sqr();
        let quadratic = |pole: C64| [1.0, -2.0 * pole.re, pole.norm_sqr()];
        match self {
            Self::Lowpass(cutoff) => {
                let numerator =
                    zero.map_or([0.0, 0.0, 1.0], |zero| [1.0, 0.0, (zero * cutoff).powi(2)]);
                let denominator = [1.0, -2.0 * pole.re * cutoff, size2 * cutoff * cutoff];
                (Analog::new(numerator, denominator).normalised(self), None)
            }
            Self::Highpass(cutoff) => {
                let numerator =
                    zero.map_or([1.0, 0.0, 0.0], |zero| [1.0, 0.0, (cutoff / zero).powi(2)]);
                let denominator = [
                    1.0,
                    -2.0 * pole.re * cutoff / size2,
                    cutoff * cutoff / size2,
                ];
                (Analog::new(numerator, denominator).normalised(self), None)
            }
            Self::Bandpass(centre2, width) => {
                let (large, small) = quadratic_roots(pole.scale(width), centre2);
                let (high, low) = match zero {
                    Some(zero) => {
                        let (high, low) = imaginary_pair(width * zero, centre2);
                        ([1.0, 0.0, high * high], [1.0, 0.0, low * low])
                    }
                    None => ([0.0, 1.0, 0.0], [0.0, 1.0, 0.0]),
                };
                (
                    Analog::new(high, quadratic(large)).normalised(self),
                    Some(Analog::new(low, quadratic(small)).normalised(self)),
                )
            }
            Self::Bandstop(centre2, width) => {
                let inverse = divide(C64::new(width, 0.0), pole);
                let (large, small) = quadratic_roots(inverse, centre2);
                let (high, low) = match zero {
                    Some(zero) => {
                        let (high, low) = imaginary_pair(width / zero, centre2);
                        ([1.0, 0.0, high * high], [1.0, 0.0, low * low])
                    }
                    None => ([1.0, 0.0, centre2], [1.0, 0.0, centre2]),
                };
                (
                    Analog::new(high, quadratic(large)).normalised(self),
                    Some(Analog::new(low, quadratic(small)).normalised(self)),
                )
            }
        }
    }

    /// Where a section's gain is set to 1: where the prototype's 0 rad/s
    /// lands in the band.
    fn reference(self) -> Reference {
        match self {
            Self::Lowpass(_) | Self::Bandstop(..) => Reference::Zero,
            Self::Highpass(_) => Reference::Infinity,
            Self::Bandpass(centre2, _) => Reference::At(centre2.sqrt()),
        }
    }
}

/// The two heights `h > l` with `h·l = w2` and `h - l = b`: where the
/// zeros of a band-pass or band-stop section go on the imaginary axis.
fn imaginary_pair(b: f64, w2: f64) -> (f64, f64) {
    let high = (b * b / 4.0 + w2).sqrt() + b / 2.0;
    (high, w2 / high)
}

/// Where a section's gain is set to 1.
#[derive(Debug, Clone, Copy)]
enum Reference {
    Zero,
    Infinity,
    At(f64),
}

/// A section of the analogue filter: `(n0·s² + n1·s + n2) / (d0·s² + d1·s
/// + d2)`. A first-order section has `n0 = d0 = 0`.
#[derive(Debug, Clone, Copy)]
struct Analog {
    numerator: [f64; 3],
    denominator: [f64; 3],
}

impl Analog {
    fn new(numerator: [f64; 3], denominator: [f64; 3]) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// The same section scaled to unity gain at the band's reference.
    fn normalised(mut self, edges: Edges) -> Self {
        let [n0, n1, n2] = self.numerator;
        let [d0, d1, d2] = self.denominator;
        let scale = match edges.reference() {
            Reference::Zero => d2 / n2,
            Reference::Infinity => {
                if d0 != 0.0 {
                    d0 / n0
                } else {
                    d1 / n1
                }
            }
            Reference::At(w) => {
                let at = |[a, b, c]: [f64; 3]| (c - a * w * w).hypot(b * w);
                at(self.denominator) / at(self.numerator)
            }
        };
        for coefficient in &mut self.numerator {
            *coefficient *= scale;
        }
        self
    }

    /// The digital section, by the bilinear transform `s = (z - 1) / (z +
    /// 1)`, rounded to coefficients whose poles are strictly inside the
    /// unit circle (see [`stabilised`]).
    fn bilinear(self) -> BiquadCoefficients {
        let [n0, n1, n2] = self.numerator;
        let [d0, d1, d2] = self.denominator;
        stabilised(if d0 == 0.0 {
            // First order: multiplied through by (1 + 1/z) only, so that no
            // pole and zero cancel on the unit circle.
            let a0 = d1 + d2;
            BiquadCoefficients {
                b0: (n1 + n2) / a0,
                b1: (n2 - n1) / a0,
                b2: 0.0,
                a1: (d2 - d1) / a0,
                a2: 0.0,
            }
        } else {
            let a0 = d0 + d1 + d2;
            BiquadCoefficients {
                b0: (n0 + n1 + n2) / a0,
                b1: 2.0 * (n2 - n0) / a0,
                b2: (n0 - n1 + n2) / a0,
                a1: 2.0 * (d2 - d0) / a0,
                a2: (d0 - d1 + d2) / a0,
            }
        })
    }

    /// The same digital section in the state-variable form a
    /// [`BiquadCascade`](crate::BiquadCascade) runs (see [`Row`]), worked
    /// out from the analogue coefficients, so that nothing is lost on the
    /// way.
    ///
    /// The bilinear transform's `s` is the cascade's `g·σ`, so `g` is the
    /// pre-warped frequency of the poles, `sqrt(d2 / d0)`, `k = d1 / (d0·g)`,
    /// and the weights are `n0 / d0`, `n1 / (d0·g)` and `n2 / d2`: each a
    /// quotient, exact to the last digit, where the coefficients of a
    /// direct form hold `d2 / d0` only as the small difference
    /// `1 + a1 + a2` of numbers near 1 and 2. A first-order section is run
    /// as itself times `(s + 1) / (s + 1)`, as its coefficients would be.
    fn state_variable(self) -> Row {
        let [n0, n1, n2] = self.numerator;
        let [d0, d1, d2] = self.denominator;
        if d0 == 0.0 {
            // (n1·s + n2)(s + 1) / ((d1·s + d2)(s + 1)).
            let g = (d2 / d1).sqrt();
            [g, g + 1.0 / g, n1 / d1, (n1 + n2) / (d1 * g), n2 / d2]
        } else {
            let g = (d2 / d0).sqrt();
            [g, d1 / (d0 * g), n0 / d0, n1 / (d0 * g), n2 / d2]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [f64; 4] = [44_100.0, 48_000.0, 96_000.0, 192_000.0];

    fn db(gain: f64) -> f64 {
        20.0 * gain.log10()
    }

    /// `count + 1` frequencies spread evenly over the logarithm from `low`
    /// to `high`, both included.
    fn spread(low: f64, high: f64, count: usize) -> impl Iterator<Item = f64> {
        (0..=count).map(move |i| low * (high / low).powf(i as f64 / count as f64))
    }

    /// The frequencies a band maps onto prototype frequency `x`: one for a
    /// low-pass or high-pass, two for a band-pass or band-stop.
    fn frequencies_at(band: Band, x: f64, rate: f64) -> Vec<f64> {
        let unwarp = |w: f64| w.atan() * rate / PI;
        // Below and above the centre: the two roots of w² ∓ b·w - centre².
        let pair = |b: f64, centre2: f64| {
            let root = (b * b + 4.0 * centre2).sqrt();
            vec![unwarp((root - b) / 2.0), unwarp((root + b) / 2.0)]
        };
        match Edges::new(band, rate) {
            Edges::Lowpass(cutoff) => vec![unwarp(x * cutoff)],
            Edges::Highpass(cutoff) => vec![unwarp(cutoff / x)],
            Edges::Bandpass(centre2, width) => pair(x * width, centre2),
            Edges::Bandstop(_, _) if x == 0.0 => vec![0.0, rate / 2.0],
            Edges::Bandstop(centre2, width) => pair(width / x, centre2),
        }
    }

    /// How far the pole nearest the unit circle is from it.
    fn distance(sections: &Sections) -> f64 {
        sections
            .iter()
            .map(pole_distance)
            .fold(f64::INFINITY, f64::min)
    }

    /// The same for the sections in the form the cascade runs them: the
    /// poles of `s² + k·g·s + g²`, mapped as in [`pole_distance`].
    fn form_distance(sections: &Sections) -> f64 {
        sections
            .rows()
            .iter()
            .map(|&[g, k, ..]| roots_distance(1.0, k * g, g * g))
            .fold(f64::INFINITY, f64::min)
    }

    /// The smallest size the denominator `1 + a1·z⁻¹ + a2·z⁻²` of a section
    /// takes on the unit circle.
    ///
    /// Through `z⁻¹ = (1 - s) / (1 + s)` and `s = j·w`, its square is
    /// `f(u) = ((C - A·u)² + B²·u) / (1 + u)²` with `u = w²` and `A, B, C`
    /// as in [`denominator`]. Its least value is at `u = 0`, at infinity, or
    /// where `f'` is zero, which is the one point `u* = (2C² + 2AC - B²) /
    /// (2A² + 2AC - B²)`.
    fn floor_on_circle(section: &BiquadCoefficients) -> f64 {
        let [a, b, c] = denominator(section);
        let at = |u: f64| ((c - a * u).powi(2) + b * b * u) / (1.0 + u).powi(2);
        let mut least = (c * c).min(a * a);
        let u = (2.0 * c * c + 2.0 * a * c - b * b) / (2.0 * a * a + 2.0 * a * c - b * b);
        if u > 0.0 && u.is_finite() {
            least = least.min(at(u));
        }
        least.sqrt()
    }

    /// What the precision section of the module documentation says the
    /// rounding of the coefficients costs at `frequency`, relative: 1e-16
    /// (rounded up, half a unit of the last digit of a number below 2) times
    /// the sum over the sections of `(1 + |a1| + |a2|) / |D|` and `(|b0| +
    /// |b1| + |b2|) / |N|`, `D` and `N` being the denominator and the
    /// numerator on the unit circle there.
    fn coefficient_law(sections: &Sections, frequency: f64, rate: f64) -> f64 {
        let omega = 2.0 * PI * frequency / rate;
        let sum: f64 = sections
            .iter()
            .map(|s| {
                (1.0 + s.a1.abs() + s.a2.abs()) / polynomial_size(1.0, s.a1, s.a2, omega)
                    + (s.b0.abs() + s.b1.abs() + s.b2.abs())
                        / polynomial_size(s.b0, s.b1, s.b2, omega)
            })
            .sum();
        1.1e-16 * sum
    }

    /// How far the formulas themselves can be trusted, relative: they map
    /// the frequency onto the prototype, which for a narrow band subtracts
    /// numbers close to each other, and an elliptic one then goes through
    /// the rational function of order 16. The largest seen was 3.7e-12.
    const FORMULA: f64 = 1e-11;

    /// The tolerances the coefficients are held to at `frequency`: in
    /// decibels, from the relative law, and in plain gain.
    fn coefficient_tolerance(sections: &Sections, frequency: f64, rate: f64) -> (f64, f64) {
        let law = coefficient_law(sections, frequency, rate);
        (
            20.0 * (1.0 + 4.0 * law + FORMULA).log10(),
            2.0 * law + FORMULA,
        )
    }

    /// The gain of the sections in the form the cascade runs them in:
    /// `(high·σ² + band·σ + low) / (σ² + k·σ + 1)` with `σ = j·tan(ω/2) / g`.
    fn form_magnitude(sections: &Sections, frequency: f64, rate: f64) -> f64 {
        let w = (PI * frequency / rate).tan();
        sections
            .rows()
            .iter()
            .map(|&[g, k, high, band, low]| {
                let x = w / g;
                let numerator = (low - high * x * x).hypot(band * x);
                let denominator = ((1.0 - x) * (1.0 + x)).hypot(k * x);
                numerator / denominator
            })
            .product()
    }

    const SHAPES: [Band; 4] = [
        Band::Lowpass { cutoff_hz: 2_000.0 },
        Band::Highpass { cutoff_hz: 2_000.0 },
        Band::Bandpass {
            low_hz: 1_000.0,
            high_hz: 4_000.0,
        },
        Band::Bandstop {
            low_hz: 1_000.0,
            high_hz: 4_000.0,
        },
    ];

    #[test]
    fn butterworth_is_three_decibels_down_at_its_edges_and_flat_without_a_ripple() {
        const RATE: f64 = 48_000.0;
        let half_power = -10.0 * 2f64.log10();
        for order in 1..=MAX_ORDER {
            for band in SHAPES {
                let design = Design::butterworth(band, order);
                let sections = design.sections(RATE);
                assert_eq!(sections.len(), design.num_sections());
                let edges = match band {
                    Band::Lowpass { cutoff_hz } | Band::Highpass { cutoff_hz } => {
                        vec![cutoff_hz]
                    }
                    Band::Bandpass { low_hz, high_hz } | Band::Bandstop { low_hz, high_hz } => {
                        vec![low_hz, high_hz]
                    }
                };
                for edge in edges {
                    let at = db(sections.magnitude_at(edge, RATE));
                    assert!(
                        (at - half_power).abs() < 1e-9,
                        "order {order}, {band:?}: {at} dB at {edge} Hz"
                    );
                }

                // Unity where the band is, and never above it.
                let unity = match band {
                    Band::Lowpass { .. } => vec![0.0],
                    Band::Highpass { .. } => vec![RATE / 2.0],
                    Band::Bandpass { .. } => frequencies_at(band, 0.0, RATE),
                    Band::Bandstop { .. } => vec![0.0, RATE / 2.0],
                };
                for frequency in unity {
                    let gain = sections.magnitude_at(frequency, RATE);
                    assert!((gain - 1.0).abs() < 1e-12, "{gain} at {frequency} Hz");
                }

                // No ripple: from the pass band outwards the gain only
                // falls, to the last digits.
                let centre = match Edges::new(band, RATE) {
                    Edges::Bandpass(centre2, _) | Edges::Bandstop(centre2, _) => {
                        centre2.sqrt().atan() * RATE / PI
                    }
                    _ => 0.0,
                };
                let frequencies: Vec<f64> = spread(1.0, 0.4999 * RATE, 2_000).collect();
                let gains: Vec<f64> = frequencies
                    .iter()
                    .map(|&f| sections.magnitude_at(f, RATE))
                    .collect();
                assert!(gains.iter().all(|&gain| gain <= 1.0 + 1e-12));
                for (pair, frequencies) in gains.windows(2).zip(frequencies.windows(2)) {
                    if frequencies[0] < centre && frequencies[1] > centre {
                        // The turning point of a band-pass or band-stop.
                        continue;
                    }
                    let rising = match band {
                        Band::Lowpass { .. } => false,
                        Band::Highpass { .. } => true,
                        Band::Bandpass { .. } => frequencies[1] <= centre,
                        Band::Bandstop { .. } => frequencies[0] >= centre,
                    };
                    let (from, to) = if rising {
                        (pair[0], pair[1])
                    } else {
                        (pair[1], pair[0])
                    };
                    assert!(
                        from <= to * (1.0 + 1e-12),
                        "order {order}, {band:?}: a ripple at {frequencies:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn butterworth_falls_six_decibels_per_octave_for_each_order() {
        const RATE: f64 = 192_000.0;
        for order in 1..=MAX_ORDER {
            let expected = 20.0 * 2f64.log10() * order as f64;
            // Far above a low-pass at 10 Hz, and far below a high-pass at
            // 20 kHz, where the bilinear transform bends the frequency axis
            // by less than 0.03 dB over an octave.
            let low = Design::butterworth(Band::Lowpass { cutoff_hz: 10.0 }, order).sections(RATE);
            let slope = db(low.magnitude_at(800.0, RATE)) - db(low.magnitude_at(400.0, RATE));
            assert!(
                (slope + expected).abs() < 0.03,
                "order {order}: {slope} dB per octave"
            );
            let high = Design::butterworth(
                Band::Highpass {
                    cutoff_hz: 20_000.0,
                },
                order,
            );
            let high = high.sections(RATE);
            let slope = db(high.magnitude_at(500.0, RATE)) - db(high.magnitude_at(250.0, RATE));
            assert!(
                (slope - expected).abs() < 0.03,
                "order {order}: {slope} dB per octave"
            );
            // A band-pass of order N has N poles on each side.
            let band = Band::Bandpass {
                low_hz: 9_000.0,
                high_hz: 11_000.0,
            };
            let sections = Design::butterworth(band, order).sections(RATE);
            let below =
                db(sections.magnitude_at(100.0, RATE)) - db(sections.magnitude_at(50.0, RATE));
            assert!((below - expected).abs() < 0.03, "order {order}: {below}");
        }
    }

    #[test]
    fn chebyshev_type_i_ripples_between_zero_and_its_depth_in_the_pass_band() {
        const RATE: f64 = 48_000.0;
        for ripple in [0.01, 0.5, 1.0, 3.0] {
            for order in 1..=MAX_ORDER {
                let n = order as f64;
                for band in SHAPES {
                    let design = Design::chebyshev_i(band, order, ripple);
                    let sections = design.sections(RATE);
                    // Each gain in decibels, with the tolerance the
                    // precision section of the module documentation gives
                    // it.
                    let at = |x: f64| -> Vec<(f64, f64)> {
                        frequencies_at(band, x, RATE)
                            .into_iter()
                            .map(|f| {
                                let (tolerance, _) = coefficient_tolerance(&sections, f, RATE);
                                (db(sections.magnitude_at(f, RATE)), tolerance)
                            })
                            .collect()
                    };
                    // The peaks, where T_N(x) is 0, are at 0 dB.
                    for k in 0..order.div_ceil(2) {
                        for (gain, tolerance) in at(((2 * k + 1) as f64 * PI / (2.0 * n)).cos()) {
                            assert!(
                                gain.abs() < tolerance,
                                "{ripple} dB, {order}, {band:?}: {gain}"
                            );
                        }
                    }
                    // The troughs, where T_N(x) is ±1, are at -ripple, and
                    // the last of them is the edge of the band.
                    for k in 0..=order / 2 {
                        for (gain, tolerance) in at((k as f64 * PI / n).cos()) {
                            assert!(
                                (gain + ripple).abs() < tolerance,
                                "{ripple} dB, {order}, {band:?}: {gain}"
                            );
                        }
                    }
                    // In between it stays within the two; beyond the edge
                    // it is below them.
                    for i in 0..=400 {
                        let x = i as f64 / 400.0;
                        for (gain, tolerance) in at(x) {
                            assert!(
                                gain < tolerance && gain > -ripple - tolerance,
                                "{gain} at {x}"
                            );
                        }
                        for (gain, _) in at(1.0 + 10.0 * x + 1e-6) {
                            assert!(gain < -ripple, "{gain} beyond the edge");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn chebyshev_type_ii_holds_its_stop_band_down_and_its_pass_band_flat() {
        const RATE: f64 = 48_000.0;
        for attenuation in [20.0, 40.0, 60.0, 100.0] {
            for order in 1..=MAX_ORDER {
                let n = order as f64;
                for band in SHAPES {
                    let design = Design::chebyshev_ii(band, order, attenuation);
                    let sections = design.sections(RATE);
                    // Each gain, with the tolerances in decibels and in
                    // plain gain that the precision section of the module
                    // documentation gives it.
                    let at = |x: f64| -> Vec<(f64, f64, f64)> {
                        frequencies_at(band, x, RATE)
                            .into_iter()
                            .map(|f| {
                                let (tolerance, plain) = coefficient_tolerance(&sections, f, RATE);
                                (sections.magnitude_at(f, RATE), tolerance, plain)
                            })
                            .collect()
                    };
                    let floor = 10f64.powf(-attenuation / 20.0);
                    // The peaks of the stop band, where T_N(1/x) is ±1, are
                    // at -attenuation, and the first of them is its edge.
                    for k in 0..order.div_ceil(2) {
                        for (gain, _, plain) in at(1.0 / (k as f64 * PI / n).cos()) {
                            assert!(
                                (gain - floor).abs() < plain,
                                "{attenuation} dB, {order}, {band:?}: {gain}"
                            );
                        }
                    }
                    // Between them are its zeros.
                    for k in 0..order / 2 {
                        let x = 1.0 / ((2 * k + 1) as f64 * PI / (2.0 * n)).cos();
                        for (gain, _, plain) in at(x) {
                            assert!(gain < plain, "a zero at {gain}");
                        }
                    }
                    // Nowhere in the stop band is it any higher.
                    for x in spread(1.0, 1e4, 400) {
                        for (gain, _, plain) in at(x) {
                            assert!(gain < floor + plain, "{gain} at {x}");
                        }
                    }
                    // The pass band starts at exactly 0 dB and falls
                    // without a ripple to the edge.
                    for (gain, tolerance, _) in at(0.0) {
                        assert!(db(gain).abs() < tolerance, "{gain}");
                    }
                    let mut before = at(0.0);
                    for i in 1..=400 {
                        let now = at(i as f64 / 400.0);
                        for (now, before) in now.iter().zip(&before) {
                            assert!(
                                now.0 <= before.0 * (1.0 + 1e-12),
                                "a ripple in the pass band"
                            );
                        }
                        before = now;
                    }
                }
            }
        }
    }

    #[test]
    fn elliptic_ripples_in_both_bands_within_its_specification() {
        use crate::elliptic::{Elliptic, Landen};
        use crate::fft::Complex;

        const RATE: f64 = 48_000.0;
        for (ripple, attenuation) in [(0.1, 40.0), (0.5, 60.0), (1.0, 80.0), (3.0, 25.0)] {
            for order in 1..=MAX_ORDER {
                let n = order as f64;
                let prototype = Elliptic::new(order, ripple, attenuation);
                let landen = Landen::new(prototype.selectivity);
                let k = prototype.selectivity.k;
                // Where theory puts the extremes: cd(m·K/N, k).
                let extreme = |m: usize| landen.cd(Complex::new(m as f64 / n, 0.0)).re;
                let floor = 10f64.powf(-attenuation / 20.0);
                for band in SHAPES {
                    let design = Design::elliptic(band, order, ripple, attenuation);
                    // None of these specifications is one the least
                    // damping changes.
                    let family = Family::Elliptic {
                        ripple_db: ripple,
                        attenuation_db: attenuation,
                    };
                    assert_eq!(design.family(), family);
                    let sections = design.sections(RATE);
                    // Each gain, with the tolerances in decibels and in
                    // plain gain that the precision section of the module
                    // documentation gives it.
                    let at = |x: f64| -> Vec<(f64, f64, f64)> {
                        frequencies_at(band, x, RATE)
                            .into_iter()
                            .map(|f| {
                                let (tolerance, plain) = coefficient_tolerance(&sections, f, RATE);
                                (sections.magnitude_at(f, RATE), tolerance, plain)
                            })
                            .collect()
                    };
                    let what = format!("{ripple} dB / {attenuation} dB, order {order}, {band:?}");
                    for m in 0..=order {
                        let x = extreme(m);
                        if x < 1e-12 && order % 2 == 0 {
                            // cd(K) = 0 maps to infinity in the stop band.
                            continue;
                        }
                        // In the pass band: 0 dB where R_N is 0, -ripple
                        // where it is ±1, the edge of the band included.
                        let expected = if m % 2 == 0 { -ripple } else { 0.0 };
                        for (gain, tolerance, _) in at(x) {
                            let gain = db(gain);
                            assert!(
                                (gain - expected).abs() < tolerance,
                                "{what}: {gain} at x = {x}"
                            );
                        }
                        // In the stop band, at 1 / (k·x): -attenuation where
                        // R_N is ±1/k1, the edge of the stop band included,
                        // and a zero where it is infinite.
                        if x > 1e-12 {
                            for (gain, _, plain) in at(1.0 / (k * x)) {
                                if m % 2 == 0 {
                                    assert!(
                                        (gain - floor).abs() < plain,
                                        "{what}: {gain} at 1/(k {x})"
                                    );
                                } else {
                                    assert!(gain < plain, "{what}: a zero at {gain}");
                                }
                            }
                        }
                    }
                    // Between those points it stays within the limits.
                    for i in 0..=400 {
                        let x = i as f64 / 400.0;
                        for (gain, tolerance, _) in at(x) {
                            let gain = db(gain);
                            assert!(
                                gain < tolerance && gain > -ripple - tolerance,
                                "{what}: {gain} at {x}"
                            );
                        }
                    }
                    for x in spread(1.0 / k, 1e4 / k, 400) {
                        for (gain, _, plain) in at(x) {
                            assert!(
                                gain < floor + plain,
                                "{what}: {gain} in the stop band at {x}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn elliptic_is_steeper_than_chebyshev_for_the_same_order_and_ripple() {
        // What the narrowest transition means: at the same order, ripple
        // and attenuation, the elliptic stop band begins well before the
        // frequency at which a Chebyshev type I reaches the same depth.
        const RATE: f64 = 192_000.0;
        let band = Band::Lowpass { cutoff_hz: 1_000.0 };
        for order in [3, 5, 8] {
            let elliptic = Design::elliptic(band, order, 0.5, 60.0);
            let chebyshev = Design::chebyshev_i(band, order, 0.5);
            let depth = |design: &Design| {
                // The lowest frequency above the cutoff where the design is
                // 60 dB down.
                spread(1_000.0, 50_000.0, 20_000)
                    .find(|&f| design.magnitude_at(f, RATE) <= 1e-3 * (1.0 + 1e-9))
                    .unwrap_or(f64::INFINITY)
            };
            let (e, c) = (depth(&elliptic), depth(&chebyshev));
            assert!(e < 0.8 * c, "order {order}: {e} Hz against {c} Hz");
        }
    }

    /// Every family with a few settings, every kind of band with edges from
    /// 1 Hz to 0.49 of the sample rate, down to 0.1 % wide. The settings
    /// include type II attenuations of 140 and 200 dB and the loosest
    /// elliptic specifications there are (1 dB more attenuation than
    /// ripple), which the least damping then raises.
    fn the_whole_range(rate: f64) -> Vec<Design> {
        let families = [
            Family::Butterworth,
            Family::ChebyshevI { ripple_db: 0.01 },
            Family::ChebyshevI { ripple_db: 0.5 },
            Family::ChebyshevI { ripple_db: 3.0 },
            Family::ChebyshevII {
                attenuation_db: 20.0,
            },
            Family::ChebyshevII {
                attenuation_db: 60.0,
            },
            Family::ChebyshevII {
                attenuation_db: 100.0,
            },
            Family::ChebyshevII {
                attenuation_db: 140.0,
            },
            Family::ChebyshevII {
                attenuation_db: 200.0,
            },
            Family::Elliptic {
                ripple_db: 0.01,
                attenuation_db: 100.0,
            },
            Family::Elliptic {
                ripple_db: 0.5,
                attenuation_db: 60.0,
            },
            Family::Elliptic {
                ripple_db: 3.0,
                attenuation_db: 25.0,
            },
            Family::Elliptic {
                ripple_db: 0.1,
                attenuation_db: 1.1,
            },
            Family::Elliptic {
                ripple_db: 1.0,
                attenuation_db: 2.0,
            },
            Family::Elliptic {
                ripple_db: 3.0,
                attenuation_db: 4.0,
            },
        ];
        let edges = [
            1.0,
            10.0,
            20.0,
            55.0,
            200.0,
            1_000.0,
            5_000.0,
            0.2 * rate,
            0.4 * rate,
            0.49 * rate,
        ];
        let mut bands = Vec::new();
        for (i, &low_hz) in edges.iter().enumerate() {
            bands.push(Band::Lowpass { cutoff_hz: low_hz });
            bands.push(Band::Highpass { cutoff_hz: low_hz });
            for &high_hz in &edges[i + 1..] {
                bands.push(Band::Bandpass { low_hz, high_hz });
                bands.push(Band::Bandstop { low_hz, high_hz });
            }
        }
        for low_hz in [1.0, 10.0, 100.0, 1_000.0, 10_000.0] {
            for ratio in [NARROWEST_RATIO, 1.02, 1.1] {
                let high_hz = low_hz * ratio;
                bands.push(Band::Bandpass { low_hz, high_hz });
                bands.push(Band::Bandstop { low_hz, high_hz });
            }
        }
        let mut designs = Vec::new();
        for family in families {
            for order in [1, 2, 3, 4, 5, 8, 11, 16] {
                for &band in &bands {
                    designs.push(Design::new(family, band, order));
                }
            }
        }
        designs
    }

    /// Whether a design is one of the two kinds the module documentation
    /// names as beyond double precision: a band-pass or band-stop 0.1 %
    /// wide at 1 Hz with an elliptic specification whose prototype has a
    /// pole damped by less than 1e-7, or a first-order type II band-pass
    /// with 200 dB of attenuation, at most 10 % wide, at 100 Hz or below.
    fn is_beyond_double_precision(design: &Design) -> bool {
        match (design.family(), design.band(), design.order()) {
            (
                Family::Elliptic {
                    ripple_db,
                    attenuation_db,
                },
                Band::Bandpass { low_hz, high_hz } | Band::Bandstop { low_hz, high_hz },
                order,
            ) => {
                let damping = Elliptic::new(order, ripple_db, attenuation_db).smallest_damping();
                low_hz == 1.0 && high_hz <= NARROWEST_RATIO && damping < 1e-7
            }
            (Family::ChebyshevII { attenuation_db }, Band::Bandpass { low_hz, high_hz }, 1) => {
                attenuation_db == 200.0 && low_hz <= 100.0 && high_hz <= 1.1 * low_hz
            }
            _ => false,
        }
    }

    #[test]
    fn every_design_in_the_range_is_stable_and_follows_its_formula() {
        // How many designs have a pole closer to the circle than 1e-15, as
        // the documentation counts them.
        const BEYOND: usize = 88;
        let mut count = 0;
        let mut beyond = 0;
        // The largest errors seen, against what the documentation states:
        // of the coefficients relatively and in plain gain, each over the
        // law, and of the cascade's form, times its `d`.
        let mut worst = [0.0f64; 3];
        for rate in RATES {
            let probes: Vec<f64> = spread(1.0, 0.499 * rate, 20).collect();
            for design in the_whole_range(rate) {
                let sections = design.sections(rate);
                // Every pole of the coefficients strictly inside the unit
                // circle, the rounded radius included.
                assert!(
                    distance(&sections) > 0.0 && sections.largest_pole_radius() < 1.0,
                    "{design:?} at {rate} Hz is not stable"
                );
                // The cascade's form: a gain above zero, a damping above
                // zero (a stable analogue section), finite weights.
                for &[g, k, high, band, low] in sections.rows() {
                    assert!(g > 0.0 && k > 0.0, "{design:?}: g {g}, k {k}");
                    assert!([g, k, high, band, low].iter().all(|x| x.is_finite()));
                }
                let d = form_distance(&sections);
                if d < 1e-15 {
                    assert!(is_beyond_double_precision(&design), "{design:?}: d {d:e}");
                    beyond += 1;
                }
                let mut frequencies = probes.clone();
                match design.band() {
                    Band::Lowpass { cutoff_hz } | Band::Highpass { cutoff_hz } => {
                        frequencies.push(cutoff_hz);
                    }
                    Band::Bandpass { low_hz, high_hz } | Band::Bandstop { low_hz, high_hz } => {
                        frequencies.extend([low_hz, high_hz, (low_hz * high_hz).sqrt()]);
                    }
                }
                for frequency in frequencies {
                    let measured = sections.magnitude_at(frequency, rate);
                    let formula = design.magnitude_at(frequency, rate);
                    let what = format!("{design:?} at {rate} Hz, {frequency} Hz");
                    let law = coefficient_law(&sections, frequency, rate);
                    if law < 0.01 {
                        let plain = (measured - formula).abs();
                        assert!(
                            plain <= 2.0 * law + FORMULA,
                            "{what}: {measured} against {formula}, law {law:e}"
                        );
                        if plain > FORMULA {
                            worst[1] = worst[1].max(plain / law);
                        }
                        if formula > 0.01 {
                            let relative = (measured / formula - 1.0).abs();
                            assert!(
                                relative <= 4.0 * law + FORMULA,
                                "{what}: {measured} against {formula}, law {law:e}"
                            );
                            if relative > FORMULA {
                                worst[0] = worst[0].max(relative / law);
                            }
                        }
                    }
                    if formula > 0.01 {
                        let form = form_magnitude(&sections, frequency, rate);
                        let relative = (form / formula - 1.0).abs();
                        assert!(
                            relative <= 2e-15 / d + FORMULA,
                            "{what}: {form} against {formula}, d {d:e}"
                        );
                        if relative > FORMULA {
                            worst[2] = worst[2].max(relative * d);
                        }
                    }
                }
                count += 1;
            }
        }
        assert_eq!(count, 67_200);
        assert_eq!(beyond, BEYOND, "designs beyond double precision");
        // The bounds the documentation states. The largest values seen
        // differ between platforms' mathematics libraries (3.5 and 1.8 on
        // Windows, 3.7 and 1.4 on Linux), so they are not pinned here.
        assert!(
            worst[0] < 4.0 && worst[1] < 2.0 && worst[2] < 2e-15,
            "{worst:?}"
        );
    }

    #[test]
    fn the_examples_of_precision_are_what_the_documentation_says() {
        // For each: `d` of the cascade's form, and the largest error above
        // -40 dB of the coefficients and of the cascade's form, in
        // decibels, from a thousandth of the edge to a hundred times it, or
        // from half the lower edge to twice the upper one, and at 0 Hz.
        let lowpass = |cutoff_hz| Band::Lowpass { cutoff_hz };
        let bandpass = |low_hz, high_hz| Band::Bandpass { low_hz, high_hz };
        let rows = [
            (
                Design::butterworth(lowpass(1_000.0), 8),
                48_000.0,
                0.025,
                1e-12,
                1e-13,
            ),
            (
                Design::butterworth(Band::Highpass { cutoff_hz: 20.0 }, 4),
                48_000.0,
                0.001,
                1e-9,
                1e-13,
            ),
            (
                Design::butterworth(lowpass(1.0), 2),
                48_000.0,
                9.3e-5,
                1e-7,
                1e-13,
            ),
            (
                Design::chebyshev_i(lowpass(10.0), 16, 0.5),
                192_000.0,
                3.6e-6,
                1e-6,
                1e-13,
            ),
            (
                Design::chebyshev_ii(lowpass(10.0), 2, 140.0),
                192_000.0,
                1.0e-7,
                0.05,
                1e-13,
            ),
            (
                Design::chebyshev_ii(lowpass(10.0), 2, 200.0),
                192_000.0,
                3.3e-9,
                20.0,
                1e-13,
            ),
            (
                Design::butterworth(bandpass(1.0, 1.001), 16),
                192_000.0,
                1.6e-9,
                0.003,
                1e-11,
            ),
            (
                Design::chebyshev_ii(bandpass(1.0, 1.001), 1, 100.0),
                192_000.0,
                1.6e-13,
                30.0,
                1e-13,
            ),
        ];
        for (design, rate, d, coefficients, form) in rows {
            let sections = design.sections(rate);
            let (low, high) = match design.band() {
                Band::Lowpass { cutoff_hz } | Band::Highpass { cutoff_hz } => {
                    (cutoff_hz / 1_000.0, (cutoff_hz * 100.0).min(0.499 * rate))
                }
                Band::Bandpass { low_hz, high_hz } | Band::Bandstop { low_hz, high_hz } => {
                    (low_hz / 2.0, high_hz * 2.0)
                }
            };
            let (mut worst_coefficients, mut worst_form) = (0.0f64, 0.0f64);
            for frequency in spread(low, high, 400).chain([0.0]) {
                let formula = design.magnitude_at(frequency, rate);
                if formula > 0.01 {
                    let measured = sections.magnitude_at(frequency, rate);
                    worst_coefficients = worst_coefficients.max(db(measured / formula).abs());
                    let computed = form_magnitude(&sections, frequency, rate);
                    worst_form = worst_form.max(db(computed / formula).abs());
                }
            }
            let measured_d = form_distance(&sections);
            assert!(
                (measured_d / d - 1.0).abs() < 0.05,
                "{design:?}: d is {measured_d}"
            );
            // The figure quoted, and not a tenth of it: it is what the
            // coefficients lose, not a loose bound on it.
            assert!(
                worst_coefficients < coefficients && worst_coefficients > coefficients / 10.0,
                "{design:?}: {worst_coefficients} dB"
            );
            assert!(worst_form < form, "{design:?}: {worst_form} dB");
        }
        // The two type II low-passes: poles at 0.0045 Hz and 0.00014 Hz, and
        // at 200 dB a `1 + a1 + a2` of 2e-17, below the last digit of `a1`.
        // The gain of the coefficients at 0 Hz shows it.
        for (attenuation, hertz) in [(140.0, 0.0045), (200.0, 0.00014)] {
            let sections = Design::chebyshev_ii(lowpass(10.0), 2, attenuation).sections(192_000.0);
            let pole = sections.rows()[0][0].atan() * 192_000.0 / PI;
            assert!((pole / hertz - 1.0).abs() < 0.05, "{pole} Hz");
        }
        let sections = Design::chebyshev_ii(lowpass(10.0), 2, 200.0).sections(192_000.0);
        let g = sections.rows()[0][0];
        assert!((4.0 * g * g / 2e-17 - 1.0).abs() < 0.1, "{}", 4.0 * g * g);
        let design = Design::chebyshev_ii(lowpass(10.0), 2, 140.0);
        let at_zero = db(design.sections(192_000.0).magnitude_at(0.0, 192_000.0));
        assert!(at_zero.abs() > 0.01 && at_zero.abs() < 0.05, "{at_zero} dB");
        // And the one beyond double precision: a pole 1.6e-18 from the
        // circle, which the coefficients hold only as close as 1.1e-16.
        let design = Design::chebyshev_ii(bandpass(1.0, 1.001), 1, 200.0);
        let sections = design.sections(192_000.0);
        assert!(is_beyond_double_precision(&design));
        assert!((form_distance(&sections) / 1.64e-18 - 1.0).abs() < 0.05);
        assert!(distance(&sections) > 0.0 && distance(&sections) < 2e-16);
    }

    #[test]
    fn the_denominator_is_smallest_near_a_pole_at_two_d_m() {
        // A complex pair near the circle: the smallest size of the
        // denominator is close to 2·d·m.
        for (design, rate) in [
            (
                Design::butterworth(
                    Band::Bandpass {
                        low_hz: 1_000.0,
                        high_hz: 1_010.0,
                    },
                    2,
                ),
                48_000.0,
            ),
            (
                Design::chebyshev_i(Band::Lowpass { cutoff_hz: 30.0 }, 8, 1.0),
                96_000.0,
            ),
        ] {
            let sections = design.sections(rate);
            let section = sections
                .iter()
                .max_by(|a, b| pole_radius(a).total_cmp(&pole_radius(b)))
                .unwrap();
            let d = pole_distance(section);
            // The pole itself, as `a2 = r²` and `a1 = -2r·cos θ`.
            let r = section.a2.sqrt();
            let theta = (-section.a1 / (2.0 * r)).acos();
            let m = (1.0 - r * theta.cos()).hypot(r * theta.sin());
            let floor = floor_on_circle(section);
            assert!(
                (floor / (2.0 * d * m) - 1.0).abs() < 0.05,
                "{floor} against {}",
                2.0 * d * m
            );
        }
        // A real pair near 0 Hz: the product of their distances from 1,
        // which is `C / (1 + s1)(1 + s2)` and close to `1 + a1 + a2`.
        let sections =
            Design::chebyshev_ii(Band::Lowpass { cutoff_hz: 10.0 }, 2, 100.0).sections(192_000.0);
        let section = &sections[0];
        let [_, _, c] = denominator(section);
        assert!((floor_on_circle(section) / c - 1.0).abs() < 1e-3);
        assert!(c < 1e-11);
    }

    #[test]
    fn single_precision_coefficients_would_not_do() {
        let rounded = |sections: &Sections| -> Vec<BiquadCoefficients> {
            sections
                .iter()
                .map(|s| BiquadCoefficients {
                    b0: f64::from(s.b0 as f32),
                    b1: f64::from(s.b1 as f32),
                    b2: f64::from(s.b2 as f32),
                    a1: f64::from(s.a1 as f32),
                    a2: f64::from(s.a2 as f32),
                })
                .collect()
        };
        // A pole outside the unit circle.
        let design = Design::chebyshev_i(Band::Lowpass { cutoff_hz: 1.0 }, 16, 0.5);
        let sections = design.sections(192_000.0);
        assert!(sections.largest_pole_radius() < 1.0);
        let largest = rounded(&sections)
            .iter()
            .map(pole_radius)
            .fold(0.0, f64::max);
        assert!(largest > 1.0, "{largest}");
        // A response off by more than 10 dB.
        let design = Design::butterworth(Band::Lowpass { cutoff_hz: 10.0 }, 16);
        let sections = design.sections(192_000.0);
        let rounded = rounded(&sections);
        let worst = spread(0.1, 100.0, 200)
            .map(|f| {
                let gain: f64 = rounded
                    .iter()
                    .map(|s| s.magnitude_at(f, 192_000.0))
                    .product();
                db(gain / design.magnitude_at(f, 192_000.0)).abs()
            })
            .fold(0.0, f64::max);
        assert!(worst > 10.0, "{worst}");
    }

    #[test]
    fn the_response_is_evaluated_without_losing_its_digits() {
        // Where nothing is close to 0 Hz or half the sample rate, the
        // careful evaluation and the plain one agree.
        let design = Design::chebyshev_i(
            Band::Bandpass {
                low_hz: 500.0,
                high_hz: 5_000.0,
            },
            6,
            1.0,
        );
        let sections = design.sections(48_000.0);
        for frequency in spread(20.0, 20_000.0, 100) {
            let plain: f64 = sections
                .iter()
                .map(|s| s.magnitude_at(frequency, 48_000.0))
                .product();
            let careful = sections.magnitude_at(frequency, 48_000.0);
            assert!((plain / careful - 1.0).abs() < 1e-11, "{frequency} Hz");
        }
        // A double pole close to 0 Hz, then one close to half the sample
        // rate, at p = ±(1 - 2^-20). Its coefficients a1 = -2p and a2 = p²
        // are exact in binary, and its gain 1 / |1 - p/z|² is known to the
        // last digit: 1 / ((1 - |p|)² + 4|p|·s²), with s = sin(ω/2) near
        // 0 Hz and cos(ω/2) near half the sample rate.
        const RATE: f64 = 48_000.0;
        for sign in [1.0, -1.0] {
            let p: f64 = sign * (1.0 - 2f64.powi(-20));
            let section = BiquadCoefficients {
                b0: 1.0,
                b1: 0.0,
                b2: 0.0,
                a1: -2.0 * p,
                a2: p * p,
            };
            let mut sections = Sections::EMPTY;
            sections.sections[0] = section;
            sections.len = 1;
            let (mut careful_error, mut plain_error) = (0.0f64, 0.0f64);
            for offset in spread(1e-3, 1.0, 100) {
                let frequency = if sign > 0.0 {
                    offset
                } else {
                    RATE / 2.0 - offset
                };
                let omega = 2.0 * PI * frequency / RATE;
                let s = if sign > 0.0 {
                    (omega / 2.0).sin()
                } else {
                    (omega / 2.0).cos()
                };
                let exact = 1.0 / ((1.0 - p.abs()).powi(2) + 4.0 * p.abs() * s * s);
                let careful = sections.magnitude_at(frequency, RATE);
                let plain = section.magnitude_at(frequency, RATE);
                careful_error = careful_error.max((careful / exact - 1.0).abs());
                plain_error = plain_error.max((plain / exact - 1.0).abs());
            }
            assert!(careful_error < 1e-12, "careful {careful_error}");
            assert!(plain_error > 1e-6, "plain {plain_error}");
        }
    }

    #[test]
    fn the_sections_have_the_counts_and_the_order_described() {
        let lowpass = Band::Lowpass { cutoff_hz: 1_000.0 };
        for order in 1..=MAX_ORDER {
            for band in SHAPES {
                let design = Design::butterworth(band, order);
                let expected = match band {
                    Band::Lowpass { .. } | Band::Highpass { .. } => order.div_ceil(2),
                    _ => order,
                };
                assert_eq!(design.num_sections(), expected);
                assert_eq!(design.sections(48_000.0).len(), expected);
            }
            // An odd low-pass starts with its first-order section, and the
            // sections then grow more resonant: their poles move out.
            let sections = Design::butterworth(lowpass, order).sections(48_000.0);
            if order % 2 == 1 {
                assert_eq!((sections[0].b2, sections[0].a2), (0.0, 0.0));
            }
            let radii: Vec<f64> = sections.iter().map(pole_radius).collect();
            assert!(radii.windows(2).all(|pair| pair[0] <= pair[1]), "{radii:?}");
        }
        assert_eq!(MAX_SECTIONS, MAX_ORDER);
        let largest = Design::chebyshev_ii(SHAPES[3], MAX_ORDER, 60.0).sections(96_000.0);
        assert_eq!(largest.len(), MAX_SECTIONS);
        assert_eq!(largest.as_slice(), &*largest);
    }

    #[test]
    fn settings_are_limited_and_garbage_does_no_harm() {
        let lowpass = Band::Lowpass { cutoff_hz: 1_000.0 };
        assert_eq!(Design::butterworth(lowpass, 0).order(), 1);
        assert_eq!(Design::butterworth(lowpass, 99).order(), MAX_ORDER);
        assert_eq!(
            Design::chebyshev_i(lowpass, 4, f64::NAN).family(),
            Family::ChebyshevI { ripple_db: 0.001 }
        );
        assert_eq!(
            Design::chebyshev_i(lowpass, 4, 1e9).family(),
            Family::ChebyshevI { ripple_db: 20.0 }
        );
        assert_eq!(
            Design::chebyshev_ii(lowpass, 4, -5.0).family(),
            Family::ChebyshevII {
                attenuation_db: 1.0
            }
        );
        assert_eq!(Design::butterworth(lowpass, 3).band(), lowpass);
        assert_eq!(
            Design::elliptic(lowpass, 4, 3.0, 2.0).family(),
            Family::Elliptic {
                ripple_db: 3.0,
                attenuation_db: 4.0
            }
        );
        assert_eq!(
            Design::elliptic(lowpass, 4, f64::NAN, f64::NAN).family(),
            Family::Elliptic {
                ripple_db: 0.001,
                attenuation_db: 1.001
            }
        );
        let sections = Design::elliptic(lowpass, 16, 0.001, 1.001).sections(48_000.0);
        assert!(sections.largest_pole_radius() < 1.0);

        // Edges beyond the limits are moved to them.
        let rate = 48_000.0;
        let at = |band: Band| Design::butterworth(band, 4).sections(rate);
        let top = Band::Lowpass {
            cutoff_hz: HIGHEST_RATIO * rate,
        };
        for cutoff_hz in [1e9, f64::INFINITY, rate] {
            assert_eq!(at(Band::Lowpass { cutoff_hz }), at(top));
        }
        let bottom = Band::Highpass {
            cutoff_hz: LOWEST_HZ,
        };
        for cutoff_hz in [0.0, -5.0, f64::NAN, f64::NEG_INFINITY, 0.5] {
            assert_eq!(at(Band::Highpass { cutoff_hz }), at(bottom));
        }
        // A band the wrong way round is turned round.
        let band = Band::Bandpass {
            low_hz: 200.0,
            high_hz: 2_000.0,
        };
        let reversed = Band::Bandpass {
            low_hz: 2_000.0,
            high_hz: 200.0,
        };
        assert_eq!(at(band), at(reversed));
        // A band narrower than the limit is widened around its centre.
        let narrow = Band::Bandstop {
            low_hz: 1_000.0,
            high_hz: 1_000.0,
        };
        let half = NARROWEST_RATIO.sqrt();
        let widened = Band::Bandstop {
            low_hz: 1_000.0 / half,
            high_hz: 1_000.0 * half,
        };
        let (a, b) = (at(narrow), at(widened));
        for (a, b) in a.iter().zip(b.iter()) {
            for (x, y) in [(a.b0, b.b0), (a.b1, b.b1), (a.a1, b.a1), (a.a2, b.a2)] {
                assert!((x - y).abs() < 1e-12);
            }
        }

        // Nonsense sample rates count as 44.1 kHz; nothing panics, and
        // every result is stable.
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let design = Design::butterworth(band, 4);
            assert_eq!(design.sections(rate), design.sections(44_100.0));
            let sections = Design::chebyshev_i(band, 9, 1.0).sections(rate);
            assert!(sections.largest_pole_radius() < 1.0);
        }
        // A sample rate too low for the lowest edge moves that edge down.
        let sections = Design::butterworth(lowpass, 2).sections(1.0);
        assert!(sections.largest_pole_radius() < 1.0);
        assert!(sections.magnitude_at(0.0, 1.0) > 0.999);
    }

    #[test]
    fn a_narrow_band_at_a_limit_is_widened_inwards() {
        // A Butterworth band is 3.01 dB down at its edges. A band at the
        // upper limit, narrower than the narrowest, is widened down from
        // that limit and not past it; one at the lower limit, up from it.
        const RATE: f64 = 48_000.0;
        let half_power = -10.0 * 2f64.log10();
        let top = HIGHEST_RATIO * RATE;
        for band in [
            Band::Bandpass {
                low_hz: top,
                high_hz: top,
            },
            Band::Bandstop {
                low_hz: 1e9,
                high_hz: top * 0.9999,
            },
        ] {
            let sections = Design::butterworth(band, 2).sections(RATE);
            for edge in [top / NARROWEST_RATIO, top] {
                let at = db(sections.magnitude_at(edge, RATE));
                assert!(
                    (at - half_power).abs() < 1e-6,
                    "{band:?}: {at} dB at {edge} Hz"
                );
            }
        }
        let beyond = Design::butterworth(
            Band::Bandpass {
                low_hz: top,
                high_hz: top,
            },
            2,
        )
        .sections(RATE);
        assert!(db(beyond.magnitude_at(0.4902 * RATE, RATE)) < half_power);
        for band in [
            Band::Bandpass {
                low_hz: 0.5,
                high_hz: 1.0,
            },
            Band::Bandstop {
                low_hz: -3.0,
                high_hz: 1.0005,
            },
        ] {
            let sections = Design::butterworth(band, 2).sections(RATE);
            for edge in [LOWEST_HZ, LOWEST_HZ * NARROWEST_RATIO] {
                // (A band this narrow this low is where the coefficients
                // lose digits: 2e-5 dB here, within what the precision
                // section of the module documentation allows.)
                let (tolerance, _) = coefficient_tolerance(&sections, edge, RATE);
                let at = db(sections.magnitude_at(edge, RATE));
                assert!(
                    (at - half_power).abs() < tolerance,
                    "{band:?}: {at} dB at {edge} Hz"
                );
            }
        }
    }

    #[test]
    fn an_elliptic_specification_too_loose_for_its_order_gets_more_attenuation() {
        // 3 dB and 4 dB at order 11, or 1 dB and 2 dB at order 13, would put
        // poles within 1e-16 of the imaginary axis, and sections at 1 kHz
        // onto the unit circle. The attenuation is raised to where the
        // least damped pole has a damping of 1e-8, and the design is then
        // what it says: stable, with its ripple and attenuation where they
        // belong.
        const RATE: f64 = 48_000.0;
        let band = Band::Lowpass { cutoff_hz: 1_000.0 };
        for (order, ripple_db, attenuation_db, raised) in
            [(11, 3.0, 4.0, 11.11), (13, 1.0, 2.0, 9.65)]
        {
            let design = Design::elliptic(band, order, ripple_db, attenuation_db);
            let Family::Elliptic {
                attenuation_db: limited,
                ..
            } = design.family()
            else {
                unreachable!()
            };
            assert!((limited - raised).abs() < 0.01, "{order}: {limited} dB");
            let sections = design.sections(RATE);
            let d = distance(&sections);
            assert!(
                d > 5e-10 && sections.largest_pole_radius() < 1.0,
                "{order}: {d}"
            );
            // The formula keeps to the pass band, and the sections too, to
            // within what their poles 6.5e-10 from the circle allow them
            // (5e-6 dB at the edge of the band).
            for i in 0..=200 {
                let frequency = 1_000.0 * f64::from(i) / 200.0;
                let (tolerance, _) = coefficient_tolerance(&sections, frequency, RATE);
                for (gain, tolerance) in [
                    (design.magnitude_at(frequency, RATE), 1e-9),
                    (sections.magnitude_at(frequency, RATE), tolerance),
                ] {
                    let gain = db(gain);
                    assert!(
                        gain <= tolerance && gain >= -ripple_db - tolerance,
                        "{gain} at {frequency}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_elliptic_formula_stays_inside_its_bands_at_the_least_damping() {
        // The designs closest to the limit at every order, with every
        // ripple: the formula keeps to its own pass band and stop band.
        const RATE: f64 = 48_000.0;
        let band = Band::Lowpass { cutoff_hz: 1_000.0 };
        for order in 2..=MAX_ORDER {
            for ripple_db in [0.01, 0.1, 1.0, 3.0, 10.0] {
                let design = Design::elliptic(band, order, ripple_db, 0.0);
                let Family::Elliptic { attenuation_db, .. } = design.family() else {
                    unreachable!()
                };
                let prototype = Elliptic::new(order, ripple_db, attenuation_db);
                let what = format!("{order}, {ripple_db} dB, {attenuation_db} dB");
                for i in 0..=400 {
                    let x = f64::from(i) / 400.0;
                    let gain = db(design.magnitude_at(1_000.0 * x, RATE));
                    assert!(
                        gain <= 1e-9 && gain >= -ripple_db - 1e-9,
                        "{what}: {gain} at {x}"
                    );
                }
                assert!((db(design.magnitude_at(1_000.0, RATE)) + ripple_db).abs() < 1e-9);
                // The stop band, from its edge at 1/k of the pre-warped
                // cutoff (where the gain falls so steeply that the last
                // digit of the frequency is worth 1e-9 dB).
                let warp = |hz: f64| (PI * hz / RATE).tan();
                let unwarp = |w: f64| w.atan() * RATE / PI;
                let edge = unwarp(warp(1_000.0) / prototype.selectivity.k);
                for frequency in spread(edge, 0.499 * RATE, 200) {
                    let gain = db(design.magnitude_at(frequency, RATE));
                    assert!(
                        gain <= -attenuation_db + 1e-6,
                        "{what}: {gain} at {frequency}"
                    );
                }
            }
        }
    }
}
