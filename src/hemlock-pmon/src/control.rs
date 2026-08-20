//! Fan control: linear interpolation over the manifest's curve.

use hemlock_platform::schema::FanControl;

/// Target PWM percent for a temperature, per the curve semantics:
/// below the first point -> its pwm; above the last -> 100%; linear
/// interpolation between adjacent points.
pub fn target_pwm(control: &FanControl, temp_c: f64) -> u32 {
    let curve = &control.curve;
    let Some(first) = curve.first() else {
        return 100; // defensive: lint rejects empty curves
    };
    if temp_c <= first.temp_c {
        return first.pwm_percent;
    }
    for pair in curve.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if temp_c <= b.temp_c {
            let span = b.temp_c - a.temp_c;
            let frac = (temp_c - a.temp_c) / span;
            let pwm = a.pwm_percent as f64 + frac * (b.pwm_percent as f64 - a.pwm_percent as f64);
            return pwm.round() as u32;
        }
    }
    100
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use hemlock_platform::schema::CurvePoint;

    fn e1031_curve() -> FanControl {
        FanControl {
            sensor: "front-inlet-ambient-right".into(),
            interval_secs: 2,
            curve: vec![
                CurvePoint {
                    temp_c: 29.0,
                    pwm_percent: 40,
                },
                CurvePoint {
                    temp_c: 46.0,
                    pwm_percent: 100,
                },
            ],
        }
    }

    #[test]
    fn clamps_below_and_above() {
        let fc = e1031_curve();
        assert_eq!(target_pwm(&fc, 10.0), 40);
        assert_eq!(target_pwm(&fc, 29.0), 40);
        assert_eq!(target_pwm(&fc, 46.0), 100);
        assert_eq!(target_pwm(&fc, 80.0), 100);
    }

    #[test]
    fn interpolates_linearly() {
        let fc = e1031_curve();
        // Midpoint of 29..46 = 37.5C -> midpoint of 40..100 = 70%.
        assert_eq!(target_pwm(&fc, 37.5), 70);
        assert_eq!(target_pwm(&fc, 33.25), 55);
    }

    #[test]
    fn multi_segment_curve() {
        let fc = FanControl {
            sensor: "s".into(),
            interval_secs: 5,
            curve: vec![
                CurvePoint {
                    temp_c: 20.0,
                    pwm_percent: 20,
                },
                CurvePoint {
                    temp_c: 30.0,
                    pwm_percent: 40,
                },
                CurvePoint {
                    temp_c: 50.0,
                    pwm_percent: 100,
                },
            ],
        };
        assert_eq!(target_pwm(&fc, 25.0), 30);
        assert_eq!(target_pwm(&fc, 40.0), 70);
    }
}
