//! 度量。红线是拿分位数说话的 —— 平均值在实时音频里几乎没有意义，
//! 用户记住的是最糟的那 5%。

#[derive(Default, Clone)]
pub struct Histogram {
    samples: Vec<f64>,
    sorted: bool,
}

impl Histogram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            samples: Vec::with_capacity(n),
            sorted: false,
        }
    }

    pub fn push(&mut self, v: f64) {
        self.samples.push(v);
        self.sorted = false;
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn ensure_sorted(&mut self) {
        if !self.sorted {
            self.samples
                .sort_by(|a, b| a.partial_cmp(b).expect("latency samples are never NaN"));
            self.sorted = true;
        }
    }

    /// 最近邻分位数（不插值）。`q` 取值 0.0 到 1.0。
    pub fn pct(&mut self, q: f64) -> f64 {
        if self.samples.is_empty() {
            return f64::NAN;
        }
        self.ensure_sorted();
        let idx = ((self.samples.len() - 1) as f64 * q).round() as usize;
        self.samples[idx]
    }

    pub fn min(&mut self) -> f64 {
        self.pct(0.0)
    }

    pub fn max(&mut self) -> f64 {
        self.pct(1.0)
    }

    pub fn mean(&self) -> f64 {
        if self.samples.is_empty() {
            return f64::NAN;
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    pub fn summary(&mut self) -> Summary {
        Summary {
            n: self.len(),
            min: self.min(),
            p50: self.pct(0.50),
            p95: self.pct(0.95),
            p99: self.pct(0.99),
            max: self.max(),
            mean: self.mean(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut h = Histogram::new();
        for v in 1..=100 {
            h.push(v as f64);
        }
        assert_eq!(h.min(), 1.0);
        assert_eq!(h.max(), 100.0);
        assert_eq!(h.pct(0.50), 51.0); // 最近邻不插值：round(99*0.5)=50 -> samples[50]
        assert_eq!(h.pct(0.95), 95.0);
        assert!((h.mean() - 50.5).abs() < 1e-9);
    }

    #[test]
    fn empty_is_nan_not_panic() {
        let mut h = Histogram::new();
        assert!(h.pct(0.5).is_nan());
        assert!(h.mean().is_nan());
    }
}
