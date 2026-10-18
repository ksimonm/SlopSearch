// Minimal port of org.HdrHistogram.DoubleHistogram / AbstractHistogram semantics
// (as used by OpenSearch HdrPercentilesAggregator / HdrPercentileRanksAggregator).

use std::collections::BTreeMap;

pub struct HdrHistogram {
    sub_bucket_half_count_magnitude: u32,
    sub_bucket_half_count: u64,
    sub_bucket_mask: u64,
    leading_zero_count_base: u32,
    counts_array_length: i64,
    low: f64,
    high: f64,
    ratio: f64,
    counts: BTreeMap<i64, i64>,
    raw: Vec<f64>,
}

fn bucket_index_for(leading_zero_count_base: u32, sub_bucket_mask: u64, value: u64) -> i32 {
    (leading_zero_count_base as i32) - ((value | sub_bucket_mask).leading_zeros() as i32)
}

impl HdrHistogram {
    pub fn new(sig_digits: u32) -> Self {
        let largest = 2 * 10u64.pow(sig_digits);
        let mut m = 0u32;
        while (1u64 << m) < largest {
            m += 1;
        }
        let sub_bucket_half_count_magnitude = m - 1;
        let sub_bucket_count = 1u64 << m;
        let sub_bucket_half_count = sub_bucket_count / 2;
        let sub_bucket_mask = sub_bucket_count - 1;

        // deriveInternalHighestToLowestValueRatio(2) and deriveIntegerValueRange
        let internal_ratio: i64 = 1 << (64 - 2u64.leading_zeros() + 1);
        let integer_value_range = sub_bucket_half_count as i64 * internal_ratio;
        let highest = integer_value_range - 1;
        // establishSize: buckets needed to cover highest
        let mut smallest = sub_bucket_count as i64;
        let mut buckets: i64 = 1;
        while smallest <= highest {
            smallest <<= 1;
            buckets += 1;
        }
        let counts_array_length = (buckets + 1) * sub_bucket_half_count as i64;

        let low = 2f64.powf(800.0);
        let mut h = HdrHistogram {
            sub_bucket_half_count_magnitude,
            sub_bucket_half_count,
            sub_bucket_mask,
            leading_zero_count_base: 64 - m,
            counts_array_length,
            low,
            high: low * internal_ratio as f64,
            ratio: low / sub_bucket_half_count as f64,
            counts: BTreeMap::new(),
            raw: Vec::new(),
        };
        h.ratio = h.low / h.sub_bucket_half_count as f64;
        h
    }

    fn set_range(&mut self, low: f64, high: f64) {
        self.low = low;
        self.high = high;
        self.ratio = low / self.sub_bucket_half_count as f64;
    }

    fn counts_index(&self, value: i64) -> i64 {
        let value = value as u64;
        let bucket_index =
            bucket_index_for(self.leading_zero_count_base, self.sub_bucket_mask, value);
        let sub_bucket_index = value >> bucket_index;
        ((bucket_index as i64 + 1) << self.sub_bucket_half_count_magnitude)
            + (sub_bucket_index as i64 - self.sub_bucket_half_count as i64)
    }

    fn value_from_index(&self, index: i64) -> i64 {
        let mut bucket_index = (index >> self.sub_bucket_half_count_magnitude) - 1;
        let mut sub_bucket_index = (index & (self.sub_bucket_half_count as i64 - 1))
            + self.sub_bucket_half_count as i64;
        if bucket_index < 0 {
            sub_bucket_index -= self.sub_bucket_half_count as i64;
            bucket_index = 0;
        }
        sub_bucket_index << bucket_index
    }

    fn size_of_equivalent_value_range(&self, value: i64) -> i64 {
        let bucket_index =
            bucket_index_for(self.leading_zero_count_base, self.sub_bucket_mask, value as u64);
        1i64 << bucket_index
    }

    fn lowest_equivalent_value(&self, value: i64) -> i64 {
        let bucket_index =
            bucket_index_for(self.leading_zero_count_base, self.sub_bucket_mask, value as u64);
        let sub_bucket_index = (value as u64 >> bucket_index) as i64;
        sub_bucket_index << bucket_index
    }

    fn highest_equivalent_value(&self, value: i64) -> i64 {
        self.lowest_equivalent_value(value) + self.size_of_equivalent_value_range(value) - 1
    }

    fn rebuild(&mut self) {
        self.counts.clear();
        let d2i = 1.0 / self.ratio;
        for v in &self.raw {
            let iv = (*v * d2i) as i64;
            let idx = self.counts_index(iv);
            *self.counts.entry(idx).or_insert(0) += 1;
        }
    }

    pub fn record_value(&mut self, value: f64) {
        if value.is_nan() || value < 0.0 {
            return;
        }
        self.raw.push(value);
        let mut shifted = false;
        while value < self.low {
            let (l, h) = (self.low / 2.0, self.high / 2.0);
            self.set_range(l, h);
            shifted = true;
        }
        while value >= self.high {
            let (l, h) = (self.low * 2.0, self.high * 2.0);
            self.set_range(l, h);
            shifted = true;
        }
        if shifted {
            self.rebuild();
        } else {
            let iv = (value * (1.0 / self.ratio)) as i64;
            let idx = self.counts_index(iv);
            *self.counts.entry(idx).or_insert(0) += 1;
        }
    }

    pub fn value_at_percentile(&self, percentile: f64) -> f64 {
        let total = self.raw.len();
        if total == 0 {
            return 0.0;
        }
        let requested = next_after_down(percentile).max(0.0).min(100.0);
        let mut count_at = (requested * total as f64 / 100.0).ceil() as i64;
        if count_at < 1 {
            count_at = 1;
        }
        let mut cum = 0i64;
        for (idx, c) in &self.counts {
            cum += *c;
            if cum >= count_at {
                let va = self.value_from_index(*idx);
                let res = if percentile == 0.0 {
                    self.lowest_equivalent_value(va)
                } else {
                    self.highest_equivalent_value(va)
                };
                return res as f64 * self.ratio;
            }
        }
        0.0
    }

    pub fn percentile_at_or_below_value(&self, value: f64) -> f64 {
        let total = self.raw.len();
        if total == 0 {
            return 100.0;
        }
        let mut iv = (value * (1.0 / self.ratio)) as i64;
        if iv < 0 {
            iv = 0;
        }
        let target = self.counts_index(iv).min(self.counts_array_length - 1);
        let mut cum = 0i64;
        for (idx, c) in &self.counts {
            if *idx <= target {
                cum += *c;
            } else {
                break;
            }
        }
        100.0 * cum as f64 / total as f64
    }
}

fn next_after_down(x: f64) -> f64 {
    if x <= 0.0 {
        x
    } else {
        f64::from_bits(x.to_bits() - 1)
    }
}
