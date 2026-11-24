//! Which console the title runs on. Titles pick settings (resolution,
//! memory budgets) by it, so the user chooses; Series X unless told.

use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
}

impl Console {
    /// Title memory in bytes: (GPU-optimal, standard). Series X in the
    /// advanced configuration; Series S has one range, counted as standard
    /// (7,776 MiB, the published console-mode figure).
    // shortcut: later system updates gave Series S titles more (amount not
    // published here); Grand Theft Auto VI sizes nothing from this figure
    // (it goes by the device type), so update it if a title does.
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
        }
    }

    /// Compute units of the GPU.
    pub const fn compute_units(self) -> u32 {
        match self {
            Console::SeriesX => 52,
            Console::SeriesS => 20,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

/// Set before the title starts.
pub fn set(console: Console) {
    SERIES_S.store(console == Console::SeriesS, Ordering::Relaxed);
}

pub fn current() -> Console {
    if SERIES_S.load(Ordering::Relaxed) {
        Console::SeriesS
    } else {
        Console::SeriesX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_x_is_the_default() {
        assert_eq!(Console::default(), Console::SeriesX);
    }

    #[test]
    fn title_memory_matches_the_published_figures() {
        // Series X: 10 GiB GPU-optimal plus 3.5 GiB standard, 13.5 GiB in all.
        let (gpu_optimal, standard) = Console::SeriesX.title_memory();
        assert_eq!(gpu_optimal, 10 * GIB);
        assert_eq!(standard, 3 * GIB + GIB / 2);
        assert_eq!(gpu_optimal + standard, 27 * GIB / 2);
        // Series S: one range of 7,776 MiB, counted as standard.
        assert_eq!(Console::SeriesS.title_memory(), (0, 7_776 * 1024 * 1024));
    }

    #[test]
    fn names_say_which_console() {
        assert_eq!(Console::SeriesX.name(), "Xbox Series X");
        assert_eq!(Console::SeriesS.name(), "Xbox Series S");
    }
}
