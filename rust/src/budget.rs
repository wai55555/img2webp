//! Memory budget manager (design spec §3.4).
//!
//! The dispatcher is the single owner of the ledger: it grants an estimate
//! before a decode starts and releases it when the completion notification
//! arrives. Workers never touch the ledger, so they cannot block on budget.

use std::collections::HashMap;

/// Pixel limit: never fail an image the Python version converts successfully.
/// Pillow warns above 89_478_485 pixels and raises at twice that value, so the
/// Rust limit is the Python error threshold (decision D-LIMIT).
pub const MAX_PIXELS: u64 = 178_956_970;

/// libwebp refuses sides above this value (BUG-07).
pub const MAX_SIDE: u32 = 16383;

/// Default share of the available memory used for the budget.
const BUDGET_RATIO: f64 = 0.5;
const BUDGET_MIN: u64 = 256 * 1024 * 1024;
const BUDGET_MAX: u64 = 8 * 1024 * 1024 * 1024;
/// Temporary metadata buffers and the encoder bookkeeping.
const METADATA_ALLOWANCE: u64 = 16 * 1024 * 1024;
/// Safety factor applied to the estimate (provisional in Phase 1; confirmed in
/// Phase 5 with real peak measurements).
pub const SAFETY_FACTOR: f64 = 2.0;

pub fn total_budget() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let available = system.available_memory();
    let scaled = (available as f64 * BUDGET_RATIO) as u64;
    scaled.clamp(BUDGET_MIN, BUDGET_MAX)
}

/// Estimates the memory a conversion job needs from header information only.
pub fn estimate(probe: &crate::probe::Probe, _frame_bytes: u64) -> u64 {
    let pixels = probe.width as u64 * probe.height as u64;
    let animated = probe.frames > 1 && (probe.format == crate::probe::Format::Gif || probe.is_apng);
    let frames = if animated {
        probe.frames.max(1) as u64
    } else {
        1
    };
    let retained = pixels * 4 * frames;
    // Orient/colour temporary copies.
    let temporary = pixels * 4 * 2;
    // AnimEncoder accumulates its own output next to the retained frames.
    let encoder = if frames > 1 {
        pixels * 2 * frames
    } else {
        pixels * 4
    };
    let sixteen_bit = if probe.bit_depth == 16 && probe.format == crate::probe::Format::Png {
        let channels = probe.channels().max(1) as u64;
        pixels * channels * 2 * 2
    } else {
        0
    };
    let raw = retained
        .saturating_add(temporary)
        .saturating_add(encoder)
        .saturating_add(sixteen_bit)
        .saturating_add(METADATA_ALLOWANCE);
    ((raw as f64) * SAFETY_FACTOR) as u64
}

#[derive(Debug, PartialEq, Eq)]
pub enum Grant {
    Granted(u64),
    Wait,
    TooLarge,
}

pub struct Budget {
    total: u64,
    used: u64,
    grants: HashMap<u64, u64>,
    next_generation: u64,
}

impl Budget {
    pub fn new(total: u64) -> Self {
        Budget {
            total,
            used: 0,
            grants: HashMap::new(),
            next_generation: 1,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    pub fn grant(&mut self, estimate: u64) -> Grant {
        if estimate > self.total {
            return Grant::TooLarge;
        }
        if self.used.saturating_add(estimate) > self.total {
            return Grant::Wait;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.used = self.used.saturating_add(estimate);
        self.grants.insert(generation, estimate);
        Grant::Granted(generation)
    }

    /// Releases a grant; unknown generations are ignored (late or duplicated
    /// notifications must not corrupt the ledger).
    pub fn release(&mut self, generation: u64) -> bool {
        match self.grants.remove(&generation) {
            Some(estimate) => {
                self.used = self.used.saturating_sub(estimate);
                true
            }
            None => false,
        }
    }

    pub fn is_granted(&self, generation: u64) -> bool {
        self.grants.contains_key(&generation)
    }

    pub fn granted_generations(&self) -> Vec<u64> {
        self.grants.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_fifo_releasable_and_bounded() {
        let mut budget = Budget::new(100);
        assert_eq!(budget.grant(60), Grant::Granted(1));
        assert_eq!(budget.grant(60), Grant::Wait);
        assert_eq!(budget.grant(101), Grant::TooLarge);
        assert!(budget.release(1));
        assert!(!budget.release(1));
        assert_eq!(budget.grant(100), Grant::Granted(2));
        assert_eq!(budget.used(), 100);
    }
}
