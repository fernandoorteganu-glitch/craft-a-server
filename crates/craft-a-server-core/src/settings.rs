//! Limits of the per-server settings the user can change, shared by the
//! daemon (which enforces them), the panel and the command line.

/// Fewest chunks offered; below this the world looks broken.
pub const VIEW_DISTANCE_MIN: u8 = 4;
/// Most chunks offered. The game allows 32, but every step costs a lot of
/// memory and CPU, which is what a small PC cannot spare.
pub const VIEW_DISTANCE_MAX: u8 = 16;
/// What the game uses when nothing is set.
pub const VIEW_DISTANCE_DEFAULT: u8 = 10;

/// Whether `v` is in the offered range.
pub fn valid_view_distance(v: u8) -> bool {
    (VIEW_DISTANCE_MIN..=VIEW_DISTANCE_MAX).contains(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_contains_the_default_and_nothing_outside() {
        assert!(valid_view_distance(VIEW_DISTANCE_MIN));
        assert!(valid_view_distance(VIEW_DISTANCE_MAX));
        assert!(valid_view_distance(VIEW_DISTANCE_DEFAULT));
        assert!(!valid_view_distance(VIEW_DISTANCE_MIN - 1));
        assert!(!valid_view_distance(VIEW_DISTANCE_MAX + 1));
    }
}
