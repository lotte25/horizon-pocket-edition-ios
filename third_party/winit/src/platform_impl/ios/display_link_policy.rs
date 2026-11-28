//! Pure policy shared by the UIKit link and host regression tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Policy {
    pub requested: Option<u32>,
    pub maximum: u32,
    pub active: bool,
}

impl Policy {
    /// No link is needed while stopped/covered/inactive. Hardware and the
    /// game's effective frame limit (including Battery Mode) bound the hint.
    pub fn rate(self) -> Option<u32> {
        if !self.active {
            return None;
        }
        self.requested
            .filter(|&rate| rate != 0)
            .map(|rate| rate.min(self.maximum.max(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::Policy;

    #[test]
    fn startup_cover_background_and_resume_do_not_restart_a_stopped_game() {
        let mut policy = Policy {
            maximum: 120,
            ..Policy::default()
        };
        assert_eq!(policy.rate(), None);
        policy.active = true;
        assert_eq!(
            policy.rate(),
            None,
            "launcher alone never requests display callbacks"
        );
        policy.requested = Some(120);
        assert_eq!(policy.rate(), Some(120));
        policy.active = false;
        assert_eq!(policy.rate(), None);
        policy.active = true;
        assert_eq!(policy.rate(), Some(120));
        policy.requested = None;
        policy.active = false;
        policy.active = true;
        assert_eq!(
            policy.rate(),
            None,
            "returning to launcher must survive a resume"
        );
        policy.requested = Some(0);
        assert_eq!(policy.rate(), None);
    }

    #[test]
    fn hardware_and_effective_game_limits_bound_the_hint() {
        for (requested, maximum, expected) in [
            (260, 120, 120),
            (120, 60, 60),
            (60, 120, 60),
            (30, 120, 30),
            (10, 120, 10),
        ] {
            assert_eq!(
                Policy {
                    requested: Some(requested),
                    maximum,
                    active: true
                }
                .rate(),
                Some(expected)
            );
        }
    }
}
