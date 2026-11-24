use std::{sync::OnceLock, time::Duration};

pub fn test_timeout(duration: Duration) -> Duration {
    static SCALE: OnceLock<f64> = OnceLock::new();
    let scale = SCALE.get_or_init(|| match std::env::var("EMP_TEST_TIME_SCALE") {
        Ok(value) => value
            .parse::<f64>()
            .ok()
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .expect("EMP_TEST_TIME_SCALE must be a positive finite number"),
        Err(std::env::VarError::NotPresent) => 1.0,
        Err(error) => panic!("EMP_TEST_TIME_SCALE: {error}"),
    });
    duration.mul_f64(*scale)
}
