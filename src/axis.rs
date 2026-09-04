//! Coordinate axes read out of the Zarr store.
//!
//! Every EDR query eventually becomes "which indices of which axis", so the
//! axes are materialised once at startup and all snapping, range selection and
//! extent reporting is done against the real coordinate values rather than an
//! assumed grid.

/// A monotonic 1-D numeric coordinate axis (latitude, longitude, level).
///
/// ERA5 stores latitude descending (90 → -90), so ascending order is recorded
/// rather than assumed and every lookup goes through it.
#[derive(Debug, Clone)]
pub struct NumericAxis {
    pub values: Vec<f64>,
    ascending: bool,
}

impl NumericAxis {
    pub fn new(values: Vec<f64>) -> Self {
        let ascending = values.len() < 2 || values[0] <= values[values.len() - 1];
        Self { values, ascending }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn min(&self) -> f64 {
        if self.ascending {
            self.values[0]
        } else {
            self.values[self.values.len() - 1]
        }
    }

    pub fn max(&self) -> f64 {
        if self.ascending {
            self.values[self.values.len() - 1]
        } else {
            self.values[0]
        }
    }

    /// Nominal spacing, or `None` for an irregular axis such as ERA5's 37
    /// pressure levels.
    pub fn step(&self) -> Option<f64> {
        if self.values.len() < 2 {
            return None;
        }
        let step = self.values[1] - self.values[0];
        let regular = self
            .values
            .windows(2)
            .all(|w| ((w[1] - w[0]) - step).abs() < step.abs() * 1e-6);
        regular.then_some(step)
    }

    /// Index of the coordinate closest to `target`.
    pub fn nearest_index(&self, target: f64) -> usize {
        // Binary search in ascending order, then map back for descending axes.
        let n = self.values.len();
        let asc = |i: usize| if self.ascending { i } else { n - 1 - i };
        let key = |i: usize| self.values[asc(i)];

        let (mut lo, mut hi) = (0usize, n - 1);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if key(mid) < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // `lo` is the first value >= target; the nearest is `lo` or `lo - 1`.
        if lo > 0 && (target - key(lo - 1)).abs() <= (key(lo) - target).abs() {
            asc(lo - 1)
        } else {
            asc(lo)
        }
    }

    /// All indices whose value falls in `[lo, hi]`, in axis storage order.
    pub fn indices_in_range(&self, lo: f64, hi: f64) -> Vec<usize> {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        (0..self.values.len())
            .filter(|&i| self.values[i] >= lo && self.values[i] <= hi)
            .collect()
    }
}

/// The time axis, held as microseconds since the Unix epoch — the unit
/// zarr-datafusion decodes CF times into.
#[derive(Debug, Clone)]
pub struct TimeAxis {
    pub values: Vec<i64>,
}

impl TimeAxis {
    pub fn new(values: Vec<i64>) -> Self {
        Self { values }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Nominal spacing in microseconds, or `None` if the axis is irregular.
    ///
    /// Used to step a time picker by the cadence the store actually holds, so
    /// an instant between two timesteps cannot be chosen.
    pub fn step(&self) -> Option<i64> {
        if self.values.len() < 2 {
            return None;
        }
        let step = self.values[1] - self.values[0];
        let regular = self.values.windows(2).all(|w| w[1] - w[0] == step);
        (regular && step > 0).then_some(step)
    }

    pub fn nearest_index(&self, target: i64) -> usize {
        match self.values.binary_search(&target) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) if i >= self.values.len() => self.values.len() - 1,
            Err(i) => {
                if target - self.values[i - 1] <= self.values[i] - target {
                    i - 1
                } else {
                    i
                }
            }
        }
    }

    /// Half-open-free inclusive range lookup: indices with `lo <= t <= hi`.
    pub fn indices_in_range(&self, lo: i64, hi: i64) -> std::ops::Range<usize> {
        let start = self.values.partition_point(|&v| v < lo);
        let end = self.values.partition_point(|&v| v <= hi);
        start..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn era5_lat() -> NumericAxis {
        // 90 .. -90 by 0.25, as ERA5 stores it.
        NumericAxis::new((0..721).map(|i| 90.0 - i as f64 * 0.25).collect())
    }

    #[test]
    fn descending_axis_reports_extent_and_step() {
        let lat = era5_lat();
        assert_eq!(lat.min(), -90.0);
        assert_eq!(lat.max(), 90.0);
        assert_eq!(lat.step(), Some(-0.25));
    }

    #[test]
    fn nearest_index_snaps_on_a_descending_axis() {
        let lat = era5_lat();
        assert_eq!(lat.values[lat.nearest_index(90.0)], 90.0);
        assert_eq!(lat.values[lat.nearest_index(-90.0)], -90.0);
        assert_eq!(lat.values[lat.nearest_index(51.51)], 51.5);
        assert_eq!(lat.values[lat.nearest_index(51.4)], 51.5);
        // Out of range clamps to the end of the axis.
        assert_eq!(lat.values[lat.nearest_index(999.0)], 90.0);
        assert_eq!(lat.values[lat.nearest_index(-999.0)], -90.0);
    }

    #[test]
    fn nearest_index_snaps_on_an_ascending_axis() {
        let lon = NumericAxis::new((0..1440).map(|i| i as f64 * 0.25).collect());
        assert_eq!(lon.values[lon.nearest_index(0.1)], 0.0);
        assert_eq!(lon.values[lon.nearest_index(0.2)], 0.25);
        assert_eq!(lon.values[lon.nearest_index(359.9)], 359.75);
    }

    #[test]
    fn range_selection_is_inclusive_and_in_storage_order() {
        let lat = era5_lat();
        let idx = lat.indices_in_range(50.0, 51.0);
        let vals: Vec<f64> = idx.iter().map(|&i| lat.values[i]).collect();
        assert_eq!(vals, vec![51.0, 50.75, 50.5, 50.25, 50.0]);
    }

    #[test]
    fn irregular_axis_has_no_step() {
        // ERA5 pressure levels are not evenly spaced.
        let level = NumericAxis::new(vec![1.0, 2.0, 3.0, 5.0, 7.0, 10.0]);
        assert_eq!(level.step(), None);
        assert_eq!(level.values[level.nearest_index(4.0)], 3.0);
        assert_eq!(level.values[level.nearest_index(6.0)], 5.0);
    }

    #[test]
    fn time_range_is_inclusive_at_both_ends() {
        let t = TimeAxis::new((0..10).map(|i| i * 3_600_000_000).collect());
        assert_eq!(t.indices_in_range(0, 2 * 3_600_000_000), 0..3);
        assert_eq!(t.indices_in_range(-5, -1), 0..0);
        assert_eq!(t.nearest_index(3_500_000_000), 1);
    }
}
