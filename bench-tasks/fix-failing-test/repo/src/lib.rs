//! Small arithmetic helpers.

/// The n-th triangular number: 1 + 2 + … + n.
pub fn triangular(n: u64) -> u64 {
    (1..n).sum()
}

/// Whether `n` is a triangular number.
pub fn is_triangular(n: u64) -> bool {
    let mut k = 0;
    while triangular(k) < n {
        k += 1;
    }
    triangular(k) == n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangular_of_four_is_ten() {
        assert_eq!(triangular(4), 10);
    }

    #[test]
    fn triangular_of_zero_is_zero() {
        assert_eq!(triangular(0), 0);
    }

    #[test]
    fn ten_is_triangular_and_eleven_is_not() {
        assert!(is_triangular(10));
        assert!(!is_triangular(11));
    }
}
