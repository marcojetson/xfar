//! Easing and progress helpers for transient overlays.

use std::time::Duration;

/// Ease-out cubic: quick to start, gentle to settle. `t` is clamped to `[0, 1]`.
/// This is the one curve xfar uses, so motion feels consistent everywhere.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

/// Linear progress of `elapsed` through `duration`, clamped to `[0, 1]`.
/// A zero-length duration is treated as already complete.
pub fn progress(elapsed: Duration, duration: Duration) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_endpoints_and_bounds() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // Clamps outside [0, 1].
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
    }

    #[test]
    fn ease_is_monotonic_and_front_loaded() {
        let (a, b) = (ease_out_cubic(0.25), ease_out_cubic(0.75));
        assert!(a < b, "easing must increase");
        // Ease-out: more than half the distance is covered by the halfway point.
        assert!(ease_out_cubic(0.5) > 0.5);
    }

    #[test]
    fn progress_maps_and_clamps() {
        assert_eq!(progress(Duration::ZERO, Duration::from_millis(100)), 0.0);
        assert_eq!(
            progress(Duration::from_millis(50), Duration::from_millis(100)),
            0.5
        );
        // Past the end clamps to 1; a zero-length window is already complete.
        assert_eq!(
            progress(Duration::from_millis(200), Duration::from_millis(100)),
            1.0
        );
        assert_eq!(progress(Duration::from_millis(5), Duration::ZERO), 1.0);
    }
}
