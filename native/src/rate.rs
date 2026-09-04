//! Sample rates as FRACTIONS, quantised to a configurable resolution.
//!
//! # Why a fraction and not basis points
//!
//! Every rate in this collector is written the way a person says it: `0.1` is a
//! tenth of the traffic, `1` is all of it, `0` is none. Basis points were the
//! older spelling and they read badly in a config file — `apm_sample_rate=10000`
//! is not obviously "100%", and the gap between `100` and `1000` is a silent
//! factor of ten in production overhead.
//!
//! # What the denominator is for
//!
//! A fraction has to become a die roll eventually, and the die needs a number of
//! faces. That is `sample_rate_denominator` (default 1000): the RESOLUTION of
//! every rate in the process. At the default, the finest rate anyone can express
//! is one request in a thousand — plenty for a service doing a few hundred
//! requests a second, and not nearly enough for one doing a hundred thousand,
//! which is why an admin can raise it. Rates are quantised to it, so at N=1000 a
//! rate of `0.00049` rounds to zero and samples nothing. That is stated loudly
//! rather than silently rounded, because a rate that quietly became zero is a
//! service that quietly stopped reporting.
//!
//! # The basis-point migration
//!
//! Files written before this change say `apm_sample_rate=10000`, and read as a
//! fraction that is 10,000× the traffic rather than 100% of it. A fraction is by
//! definition in [0, 1], so ANY value above 1 is unambiguously the old spelling
//! and is converted from basis points instead. The one genuinely ambiguous value
//! is exactly `1`: basis points said 0.01%, a fraction says 100%. It is read as
//! the fraction, because someone writing `1` today means "all of it" — see
//! `AMBIGUOUS_ONE` and the heartbeat line, which names the interpretation so it
//! can be caught by reading a log rather than a bill.

/// Faces on the sampling die when nothing overrides it: one-in-a-thousand
/// granularity, which is finer than any rate a human writes by hand.
pub const DEFAULT_DENOMINATOR: u32 = 1000;

/// The resolution band. Below 10 a "rate" is a coin toss with no useful
/// gradations; above a million the quantisation is far finer than the sample
/// count any real window contains, so the extra precision is imaginary.
pub const MIN_DENOMINATOR: u32 = 10;
pub const MAX_DENOMINATOR: u32 = 1_000_000;

/// The value that means one thing in each spelling. Documented as a constant so
/// the test that pins the choice cannot drift from the comment explaining it.
pub const AMBIGUOUS_ONE: f64 = 1.0;

/// How a written rate was understood, so the collector can say so out loud.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    /// A fraction in [0, 1], the current spelling.
    Fraction,
    /// A value above 1, read as legacy basis points out of 10_000.
    LegacyBasisPoints,
}

/// A resolved rate: `parts` faces out of `denominator` sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleRate {
    pub parts: u32,
    pub denominator: u32,
    pub spelling: Spelling,
}

impl SampleRate {
    /// Never samples. The default for anything a service has not opted into.
    pub const fn off(denominator: u32) -> Self {
        Self {
            parts: 0,
            denominator,
            spelling: Spelling::Fraction,
        }
    }

    /// Was a non-zero rate quantised away to nothing? The caller reports this;
    /// a rate that rounds to zero looks identical to one that was set to zero,
    /// and only one of those is what its author meant.
    pub fn rounded_to_nothing(self, written: f64) -> bool {
        self.parts == 0 && written > 0.0
    }

    /// The fraction this actually samples, for logging back to a human.
    pub fn effective_fraction(self) -> f64 {
        if self.denominator == 0 {
            return 0.0;
        }
        f64::from(self.parts) / f64::from(self.denominator)
    }
}

/// Clamp an admin-supplied resolution into the usable band.
pub fn clamp_denominator(written: u32) -> u32 {
    if written == 0 {
        return DEFAULT_DENOMINATOR;
    }
    written.clamp(MIN_DENOMINATOR, MAX_DENOMINATOR)
}

/// Turn a written rate into die faces, honouring both spellings.
///
/// A non-finite or negative value is treated as "off" rather than as an error:
/// this runs in RINIT on every request, and refusing to serve traffic because a
/// config file has a typo in a sampling rate would be a far worse failure than
/// not sampling. Infinity goes the same way rather than clamping to "all of it"
/// — between under- and over-sampling on a broken value, only one of them bills
/// you for profiling every request in production.
pub fn resolve(written: f64, denominator: u32) -> SampleRate {
    let denominator = clamp_denominator(denominator);
    if !written.is_finite() || written <= 0.0 {
        return SampleRate::off(denominator);
    }
    let (fraction, spelling) = if written > AMBIGUOUS_ONE {
        (written / 10_000.0, Spelling::LegacyBasisPoints)
    } else {
        (written, Spelling::Fraction)
    };
    let fraction = fraction.clamp(0.0, 1.0);
    // Round to nearest: at N=1000 a rate of 0.0006 is closer to 1/1000 than to
    // 0, and truncating would turn "one in about 1600" into "never".
    let parts = (fraction * f64::from(denominator)).round();
    let parts = if parts >= f64::from(denominator) {
        denominator
    } else {
        parts as u32
    };
    SampleRate {
        parts,
        denominator,
        spelling,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fraction_is_read_as_written() {
        assert_eq!(resolve(0.1, 1000).parts, 100);
        assert_eq!(resolve(0.5, 1000).parts, 500);
        assert_eq!(resolve(1.0, 1000).parts, 1000);
        assert_eq!(resolve(0.0, 1000).parts, 0);
    }

    #[test]
    fn a_legacy_basis_point_value_still_means_what_it_meant() {
        // The whole installed base writes 10000 for "everything".
        let all = resolve(10_000.0, 1000);
        assert_eq!(all.parts, 1000);
        assert_eq!(all.spelling, Spelling::LegacyBasisPoints);
        // And 100 bps was 1%, which must not become 100%.
        assert_eq!(resolve(100.0, 1000).parts, 10);
        assert_eq!(resolve(2_500.0, 1000).parts, 250);
    }

    #[test]
    fn exactly_one_is_read_as_the_fraction() {
        // Pinned deliberately: `1` is the only value the two spellings disagree
        // about (0.01% vs 100%). Someone writing it today means all of it.
        let one = resolve(AMBIGUOUS_ONE, 1000);
        assert_eq!(one.parts, 1000);
        assert_eq!(one.spelling, Spelling::Fraction);
    }

    #[test]
    fn the_denominator_sets_the_finest_expressible_rate() {
        // At the default resolution, one in two thousand is not expressible and
        // rounds away — the caller is expected to say so.
        let too_fine = resolve(0.0004, 1000);
        assert_eq!(too_fine.parts, 0);
        assert!(too_fine.rounded_to_nothing(0.0004));
        // Raising the resolution makes the same rate land.
        assert_eq!(resolve(0.0004, 100_000).parts, 40);
    }

    #[test]
    fn a_rate_that_was_meant_to_be_zero_is_not_reported_as_lost() {
        assert!(!resolve(0.0, 1000).rounded_to_nothing(0.0));
    }

    #[test]
    fn nonsense_is_off_rather_than_fatal() {
        // Including infinity. It is tempting to read "more than everything" as
        // everything, but these values only ever arrive from a broken config,
        // and of the two ways to be wrong about a broken sampling rate,
        // profiling 100% of production is the expensive one.
        for written in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(resolve(written, 1000).parts, 0, "{written} should be off");
        }
    }

    #[test]
    fn the_resolution_is_clamped_into_a_usable_band() {
        assert_eq!(clamp_denominator(0), DEFAULT_DENOMINATOR);
        assert_eq!(clamp_denominator(1), MIN_DENOMINATOR);
        assert_eq!(clamp_denominator(50_000_000), MAX_DENOMINATOR);
        assert_eq!(clamp_denominator(100_000), 100_000);
    }
}
