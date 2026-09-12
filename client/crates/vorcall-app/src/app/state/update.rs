//! Where the self-updater is, and the three questions the window asks of it.
//!
//! `Disabled` is decided at boot and never leaves: such a build neither checks
//! nor swaps anything.

use std::time::Instant;

use vorcall_core::update::{Ready, Version};

pub enum UpdateState {
    Disabled(String),
    Idle,
    Checking,
    UpToDate {
        at: Instant,
    },
    NoBuild {
        platform: String,
    },
    Downloading {
        version: Version,
        received: u64,
        total: u64,
        required: bool,
    },
    Ready {
        ready: Ready,
        dismissed: bool,
    },
    Failed {
        message: String,
        required: bool,
        at: Instant,
    },
    Restarting,
}

impl UpdateState {
    /// Whether another check would only get in the way of what is already going
    /// on — or of a build that does not update itself at all.
    pub fn busy(&self) -> bool {
        match self {
            Self::Disabled(_) | Self::Checking | Self::Downloading { .. } | Self::Restarting => {
                true
            }
            // "Later" only puts the banner away. The next check reuses the file
            // already on disk, and its result brings the banner back.
            Self::Ready { dismissed, .. } => !dismissed,
            Self::Idle | Self::UpToDate { .. } | Self::NoBuild { .. } | Self::Failed { .. } => {
                false
            }
        }
    }

    /// Whether the loading creature is on screen, which is what makes its
    /// per-frame clock worth running.
    pub fn shows_creature(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. })
    }

    /// Whether the update takes the whole window instead of a banner. `forced` is
    /// `App::force_required`: neither a check nor a restart in flight carries the
    /// manifest that made the update required, and the retry after a failed
    /// required update must not flash the chat back into view.
    pub fn shows_required(&self, forced: bool) -> bool {
        match self {
            Self::Downloading { required, .. } | Self::Failed { required, .. } => *required,
            Self::Ready { ready, .. } => ready.required,
            Self::Checking | Self::Restarting => forced,
            Self::Disabled(_) | Self::Idle | Self::UpToDate { .. } | Self::NoBuild { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use vorcall_core::update::{Asset, Manifest};

    use super::*;

    fn version(raw: &str) -> Version {
        raw.parse().expect("a version")
    }

    fn ready(required: bool, manual: bool) -> Ready {
        Ready {
            manifest: Manifest {
                version: version("0.5.0"),
                notes: String::new(),
                published_at: String::new(),
                min_version: version("0.5.0"),
                platforms: BTreeMap::new(),
            },
            asset: Asset {
                path: "vorcall-linux-x86_64".to_owned(),
                sha256: String::new(),
                size: 1,
            },
            file: PathBuf::from("vorcall-linux-x86_64"),
            manual,
            required,
        }
    }

    fn downloading(required: bool) -> UpdateState {
        UpdateState::Downloading {
            version: version("0.5.0"),
            received: 1,
            total: 2,
            required,
        }
    }

    #[test]
    fn a_check_waits_for_what_is_already_going_on() {
        assert!(UpdateState::Disabled("debug build".to_owned()).busy());
        assert!(UpdateState::Checking.busy());
        assert!(downloading(false).busy());
        assert!(UpdateState::Restarting.busy());

        assert!(!UpdateState::Idle.busy());
        assert!(!UpdateState::UpToDate { at: Instant::now() }.busy());
        assert!(
            !UpdateState::NoBuild {
                platform: "linux-x86_64".to_owned()
            }
            .busy()
        );
        assert!(
            !UpdateState::Failed {
                message: "no".to_owned(),
                required: false,
                at: Instant::now()
            }
            .busy()
        );
    }

    #[test]
    fn a_dismissed_update_does_not_hold_off_the_next_check() {
        assert!(
            UpdateState::Ready {
                ready: ready(false, false),
                dismissed: false
            }
            .busy()
        );
        assert!(
            !UpdateState::Ready {
                ready: ready(false, false),
                dismissed: true
            }
            .busy()
        );
    }

    #[test]
    fn only_a_required_update_takes_the_window() {
        assert!(downloading(true).shows_required(false));
        assert!(!downloading(false).shows_required(true));

        assert!(
            UpdateState::Ready {
                ready: ready(true, false),
                dismissed: true
            }
            .shows_required(false)
        );
        assert!(
            !UpdateState::Ready {
                ready: ready(false, false),
                dismissed: false
            }
            .shows_required(true)
        );
        assert!(
            UpdateState::Failed {
                message: "no".to_owned(),
                required: true,
                at: Instant::now()
            }
            .shows_required(false)
        );
        assert!(!UpdateState::Idle.shows_required(true));
    }

    #[test]
    fn a_check_or_a_restart_takes_the_window_only_when_it_was_required() {
        assert!(UpdateState::Checking.shows_required(true));
        assert!(!UpdateState::Checking.shows_required(false));
        assert!(UpdateState::Restarting.shows_required(true));
        assert!(!UpdateState::Restarting.shows_required(false));
    }

    #[test]
    fn the_creature_runs_while_a_check_does() {
        assert!(UpdateState::Checking.shows_creature());
        assert!(downloading(false).shows_creature());

        assert!(!UpdateState::Idle.shows_creature());
        assert!(!UpdateState::Restarting.shows_creature());
        assert!(
            !UpdateState::Ready {
                ready: ready(false, false),
                dismissed: false
            }
            .shows_creature()
        );
    }
}
