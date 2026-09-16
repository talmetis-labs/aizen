//! Line-item totals. Three public functions, each of which re-sums the items by hand — the
//! duplication the task asks to remove.

/// One line item: a unit price in cents and a quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub cents: u64,
    pub qty: u64,
}

/// The plain total.
pub fn total_cents(items: &[Item]) -> u64 {
    let mut sum = 0;
    for it in items {
        sum += it.cents * it.qty;
    }
    sum
}

/// The total plus `tax_percent` percent of it (integer arithmetic, rounded down).
pub fn total_cents_with_tax(items: &[Item], tax_percent: u64) -> u64 {
    let mut sum = 0;
    for it in items {
        sum += it.cents * it.qty;
    }
    sum + sum * tax_percent / 100
}

/// The total minus a flat discount, never below zero.
pub fn total_cents_with_discount(items: &[Item], off_cents: u64) -> u64 {
    let mut sum = 0;
    for it in items {
        sum += it.cents * it.qty;
    }
    sum.saturating_sub(off_cents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discount_never_goes_negative() {
        let items = [Item { cents: 10, qty: 1 }];
        assert_eq!(total_cents_with_discount(&items, 100), 0);
    }
}
