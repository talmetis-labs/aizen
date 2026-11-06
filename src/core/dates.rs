// Copyright 2026 Aizen authors. Licensed under Apache-2.0.
// Extracted from Aizen; original implementation comments and tests retained below.

//! Shared local dates and Persona self-memory recency arithmetic.

/// Whole days between `date` (`YYYY-MM-DD`) and `today` (same format). Negative clamped to 0.
/// Returns `None` if either date can't be parsed.
#[cfg(test)]
pub fn age_days(date: &str, today: &str) -> Option<f64> {
    let d = chrono::NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let t = chrono::NaiveDate::parse_from_str(today.trim(), "%Y-%m-%d").ok()?;
    Some((t - d).num_days().max(0) as f64)
}

/// Today as `YYYY-MM-DD` (local).
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Recency multiplier `exp(-ageDays / half_life)` used by Persona self-memory ranking.
pub fn recency_factor(age_days: f64, half_life_days: f64) -> f64 {
    let age = age_days.max(0.0);
    (-age / half_life_days).exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn age_is_whole_days() {
        assert_eq!(age_days("2026-06-01", "2026-06-01"), Some(0.0));
        assert_eq!(age_days("2026-05-02", "2026-06-01"), Some(30.0));
        assert_eq!(age_days("2026-07-01", "2026-06-01"), Some(0.0));
    }
    #[test]
    fn recency_decays() {
        assert!((recency_factor(0.0, 30.0) - 1.0).abs() < 1e-9);
        assert!((recency_factor(30.0, 30.0) - std::f64::consts::E.recip()).abs() < 1e-9);
    }
    #[test]
    fn invalid_dates_and_future_recency() {
        assert_eq!(age_days("2026-02-30", "2026-03-01"), None);
        assert_eq!(age_days("n/a", "2026-03-01"), None);
        assert_eq!(recency_factor(-10.0, 45.0), 1.0);
    }
}
