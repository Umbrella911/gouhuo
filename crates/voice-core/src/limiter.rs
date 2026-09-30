// SPDX-License-Identifier: MPL-2.0

//! Memoryless mixer limiter: unity gain below the knee, continuous value and slope.
//! No look-ahead buffer or added latency. This still compresses loud mixtures;
//! it cannot restore audio already clipped at capture.

pub fn soft_limit(sample: f32) -> f32 {
    const KNEE: f32 = 0.7;
    let magnitude = sample.abs();
    if magnitude <= KNEE {
        sample
    } else {
        sample.signum() * (KNEE + (1.0 - KNEE) * ((magnitude - KNEE) / (1.0 - KNEE)).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knee_has_no_jump_on_either_side() {
        for knee in [-0.7, 0.7] {
            let delta = soft_limit(knee + 1e-5) - soft_limit(knee - 1e-5);
            assert!((delta - 2e-5).abs() < 2e-7, "jump at {knee}: {delta}");
        }
    }

    #[test]
    fn transfer_is_monotonic_bounded_and_preserves_quiet_audio() {
        let mut previous = -1.0;
        for i in -10000..=10000 {
            let x = i as f32 / 1000.0;
            let y = soft_limit(x);
            assert!(y.is_finite() && y.abs() <= 1.0 && y >= previous);
            assert_eq!(y, -soft_limit(-x));
            if x.abs() <= 0.7 {
                assert_eq!(y, x);
            }
            previous = y;
        }
    }
}
