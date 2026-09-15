//! An inventory that counts what it holds.

#[derive(Default)]
pub struct Inventory {
    items: Vec<(String, u32)>,
}

impl Inventory {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn add(&mut self, name: &str, qty: u32) {
        self.items.push((name.to_string(), qty));
    }

    /// How many distinct entries were added.
    pub fn entries(&self) -> usize {
        self.items.len() as u32
    }

    /// Total quantity across all entries.
    pub fn total(&self) -> u32 {
        self.items.iter().map(|(_, q)| *q).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_entries_and_quantities() {
        let mut inv = Inventory::new();
        inv.add("bolt", 3);
        inv.add("nut", 5);
        assert_eq!(inv.entries(), 2);
        assert_eq!(inv.total(), 8);
    }
}
