//! A crate whose tests already pass. The control task: the agent must look, not touch.

/// Greatest common divisor by Euclid's algorithm.
pub fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcd_of_coprimes_is_one() {
        assert_eq!(gcd(9, 28), 1);
    }

    #[test]
    fn gcd_of_multiples_is_the_smaller() {
        assert_eq!(gcd(12, 36), 12);
    }
}
