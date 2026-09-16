use fixture_refactor_with_tests::*;

#[test]
fn the_three_totals_agree_on_the_same_items() {
    let items = [Item { cents: 100, qty: 2 }, Item { cents: 50, qty: 1 }];
    assert_eq!(total_cents(&items), 250);
    assert_eq!(total_cents_with_tax(&items, 10), 275);
    assert_eq!(total_cents_with_discount(&items, 50), 200);
}

#[test]
fn an_empty_list_totals_zero_everywhere() {
    assert_eq!(total_cents(&[]), 0);
    assert_eq!(total_cents_with_tax(&[], 20), 0);
    assert_eq!(total_cents_with_discount(&[], 5), 0);
}
