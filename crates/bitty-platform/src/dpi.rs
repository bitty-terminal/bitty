//! DPI-aware size and scale types.
//!
//! All conversions are total, deterministic, and panic-free. Rounding uses
//! round-half-away-from-zero (`f64::round`), matching the convention of the
//! upstream windowing layer, so identical inputs produce identical outputs on
//! every platform.

use crate::error::PlatformError;

/// Ratio of physical pixels to logical pixels.
///
/// Must be finite and strictly positive; [`ScaleFactor::new`] rejects anything
/// else while [`ScaleFactor::new_sanitized`] deterministically coerces invalid
/// input to [`ScaleFactor::ONE`] for paths where the value originates from a
/// trusted upstream source that is nonetheless not worth failing over.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScaleFactor(f64);

impl ScaleFactor {
    /// A scale factor of `1.0` (no scaling).
    pub const ONE: Self = Self(1.0);

    /// Validates a scale factor.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::InvalidScaleFactor`] if `value` is zero,
    /// negative, NaN, or infinite.
    pub fn new(value: f64) -> Result<Self, PlatformError> {
        if value.is_finite() && value > 0.0 {
            Ok(Self(value))
        } else {
            Err(PlatformError::InvalidScaleFactor(value))
        }
    }

    /// Coerces any non-finite or non-positive input to [`ScaleFactor::ONE`].
    ///
    /// Use only for values that come from the platform layer itself; user- or
    /// config-supplied values must go through [`ScaleFactor::new`].
    pub fn new_sanitized(value: f64) -> Self {
        Self::new(value).unwrap_or(Self::ONE)
    }

    /// Returns the raw ratio (physical pixels per logical pixel).
    pub fn get(self) -> f64 {
        self.0
    }
}

/// A length expressed in logical pixels.
///
/// Finite but sign-unconstrained: sizes are built on
/// [`LogicalSize::new`](crate::dpi::LogicalSize::new), which additionally
/// rejects negatives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalPixel(f64);

impl LogicalPixel {
    /// Validates a finite logical-pixel length.
    ///
    /// # Errors
    ///
    /// Returns a validation error if `value` is NaN or infinite.
    pub fn new(value: f64) -> Result<Self, PlatformError> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(PlatformError::InvalidScaleFactor(value))
        }
    }

    /// Returns the raw length in logical pixels.
    pub fn get(self) -> f64 {
        self.0
    }
}

/// A width/height pair in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    width: LogicalPixel,
    height: LogicalPixel,
}

impl LogicalSize {
    /// Validates a logical size; components must be finite and >= 0.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::InvalidScaleFactor`] if either component is
    /// NaN, infinite, or negative.
    pub fn new(width: f64, height: f64) -> Result<Self, PlatformError> {
        let width = LogicalPixel::new(width)?;
        if width.get() < 0.0 {
            return Err(PlatformError::InvalidScaleFactor(width.get()));
        }
        let height = LogicalPixel::new(height)?;
        if height.get() < 0.0 {
            return Err(PlatformError::InvalidScaleFactor(height.get()));
        }
        Ok(Self { width, height })
    }

    /// Width in logical pixels.
    pub fn width(self) -> LogicalPixel {
        self.width
    }

    /// Height in logical pixels.
    pub fn height(self) -> LogicalPixel {
        self.height
    }

    /// Converts to physical pixels at `scale`, rounding half away from zero
    /// and saturating at `u32::MAX`.
    pub fn to_physical(self, scale: ScaleFactor) -> PhysicalSize {
        PhysicalSize::new(
            saturating_round_px(self.width.get() * scale.get()),
            saturating_round_px(self.height.get() * scale.get()),
        )
    }
}

/// A width/height pair in physical (device) pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PhysicalSize {
    width: u32,
    height: u32,
}

impl PhysicalSize {
    /// Creates a physical size.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Width in physical pixels.
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Height in physical pixels.
    pub const fn height(self) -> u32 {
        self.height
    }

    /// Converts to logical pixels at `scale`.
    ///
    /// # Errors
    ///
    /// Returns an error if division by the validated scale factor would still
    /// overflow into a non-finite length (only possible for extreme ratios).
    pub fn to_logical(self, scale: ScaleFactor) -> Result<LogicalSize, PlatformError> {
        LogicalSize::new(
            f64::from(self.width) / scale.get(),
            f64::from(self.height) / scale.get(),
        )
    }
}

fn saturating_round_px(value: f64) -> u32 {
    let rounded = value.round();
    if rounded <= 0.0 {
        0
    } else if rounded >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        rounded as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_factor_accepts_positive_finite_values() {
        assert_eq!(ScaleFactor::new(1.0).expect("valid"), ScaleFactor::ONE);
        assert_eq!(ScaleFactor::new(2.5).expect("valid").get(), 2.5);
        assert_eq!(
            ScaleFactor::new(f64::MIN_POSITIVE).expect("valid").get(),
            f64::MIN_POSITIVE
        );
    }

    #[test]
    fn scale_factor_rejects_zero_negative_and_non_finite() {
        assert_eq!(
            ScaleFactor::new(0.0),
            Err(PlatformError::InvalidScaleFactor(0.0))
        );
        assert_eq!(
            ScaleFactor::new(-1.5),
            Err(PlatformError::InvalidScaleFactor(-1.5))
        );
        assert!(matches!(
            ScaleFactor::new(f64::NAN),
            Err(PlatformError::InvalidScaleFactor(v)) if v.is_nan()
        ));
        assert_eq!(
            ScaleFactor::new(f64::INFINITY),
            Err(PlatformError::InvalidScaleFactor(f64::INFINITY))
        );
    }

    #[test]
    fn sanitized_scale_factor_falls_back_to_one() {
        assert_eq!(ScaleFactor::new_sanitized(-3.0), ScaleFactor::ONE);
        assert_eq!(ScaleFactor::new_sanitized(f64::NAN), ScaleFactor::ONE);
        assert_eq!(ScaleFactor::new_sanitized(1.25).get(), 1.25);
    }

    #[test]
    fn logical_to_physical_rounds_half_away_from_zero() {
        let scale = ScaleFactor::new(1.25).expect("valid");
        // 800 * 1.25 == 1000 exactly; 600 * 1.25 == 750 exactly.
        assert_eq!(
            LogicalSize::new(800.0, 600.0)
                .expect("valid")
                .to_physical(scale),
            PhysicalSize::new(1000, 750)
        );
        // 7.6 px @ 1.0 rounds to 8; 7.4 px rounds to 7 (half away from zero on .5 only).
        assert_eq!(
            LogicalSize::new(7.6, 7.4)
                .expect("valid")
                .to_physical(ScaleFactor::ONE),
            PhysicalSize::new(8, 7)
        );
        // Exact .5 cases round away from zero.
        assert_eq!(
            LogicalSize::new(2.5, 3.5)
                .expect("valid")
                .to_physical(ScaleFactor::ONE),
            PhysicalSize::new(3, 4)
        );
    }

    #[test]
    fn physical_to_logical_is_exact_division() {
        let scale = ScaleFactor::new(2.0).expect("valid");
        let logical = PhysicalSize::new(1920, 1080)
            .to_logical(scale)
            .expect("finite");
        assert_eq!(
            (logical.width().get(), logical.height().get()),
            (960.0, 540.0)
        );
    }

    #[test]
    fn logical_physical_round_trip_stays_within_half_physical_pixel() {
        for &scale_value in &[0.75, 1.0, 1.25, 1.5, 2.0] {
            let scale = ScaleFactor::new(scale_value).expect("valid");
            let tolerance = 0.5 / scale_value + 1e-9;
            for &(w, h) in &[(1.0, 1.0), (13.37, 7.77), (800.0, 600.0)] {
                let logical = LogicalSize::new(w, h).expect("valid");
                let back = logical
                    .to_physical(scale)
                    .to_logical(scale)
                    .expect("finite");
                assert!(
                    (back.width().get() - w).abs() <= tolerance,
                    "width drift {back:?} from {logical:?} at scale {scale_value}"
                );
                assert!(
                    (back.height().get() - h).abs() <= tolerance,
                    "height drift {back:?} from {logical:?} at scale {scale_value}"
                );
            }
        }
    }

    #[test]
    fn to_physical_saturates_at_u32_max() {
        let huge = LogicalSize::new(1.0e300, -0.0).expect("valid");
        assert_eq!(
            huge.to_physical(ScaleFactor::ONE),
            PhysicalSize::new(u32::MAX, 0),
            "saturates instead of wrapping"
        );
    }

    #[test]
    fn sizes_reject_negative_nan_and_infinite_components() {
        assert!(LogicalSize::new(-1.0, 10.0).is_err());
        assert!(LogicalSize::new(10.0, -0.5).is_err());
        assert!(LogicalSize::new(f64::NAN, 10.0).is_err());
        assert!(LogicalSize::new(10.0, f64::INFINITY).is_err());
        assert!(LogicalPixel::new(f64::NAN).is_err());
        assert_eq!(LogicalPixel::new(-5.0).expect("sign allowed").get(), -5.0);
    }

    #[test]
    fn physical_size_accessors_are_const_usable() {
        const SIZE: PhysicalSize = PhysicalSize::new(640, 480);
        assert_eq!((SIZE.width(), SIZE.height()), (640, 480));
    }
}
