//! Sliding-window exponential backoff restart policy (`Services\RestartPolicy` equivalent).

use crate::config::ResolvedRestartPolicy;
use std::collections::VecDeque;
use std::time::Duration;

/// Decision returned by [`RestartPolicy::record_exit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// Wait `delay` before starting attempt `attempt` (1-based within the current window).
    Restart {
        attempt: u32,
        delay: Duration,
        exits_in_window: usize,
    },
    /// Exceeded `max_restarts` within `restart_window`; transition app to `Failed` (or exit `1067` in Phase 1).
    GiveUp { exits_in_window: usize },
}

/// Pure sliding-window restart policy calculator.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    options: ResolvedRestartPolicy,
    exit_timestamps_ms: VecDeque<u64>,
}

impl RestartPolicy {
    pub fn new(options: ResolvedRestartPolicy) -> Self {
        Self {
            options,
            exit_timestamps_ms: VecDeque::new(),
        }
    }

    /// Update the policy options (e.g. on configuration reload).
    pub fn update_options(&mut self, options: ResolvedRestartPolicy) {
        self.options = options;
    }

    /// Reset recorded exits (called on manual `start` or `restart` command).
    pub fn reset(&mut self) {
        self.exit_timestamps_ms.clear();
    }

    /// Record an unexpected exit at `now_ms` (monotonic milliseconds) and compute the restart decision.
    pub fn record_exit(&mut self, now_ms: u64) -> RestartDecision {
        let window_ms = self.options.restart_window.as_millis() as u64;

        // Evict exits older than RestartWindow
        while let Some(&oldest) = self.exit_timestamps_ms.front() {
            if now_ms.saturating_sub(oldest) > window_ms {
                self.exit_timestamps_ms.pop_front();
            } else {
                break;
            }
        }

        self.exit_timestamps_ms.push_back(now_ms);
        let n = self.exit_timestamps_ms.len();

        if (n as u32) > self.options.max_restarts {
            return RestartDecision::GiveUp { exits_in_window: n };
        }

        // Delay = min(InitialDelay * 2^(n-1), MaxDelay)
        let shift = (n.saturating_sub(1)).min(30) as u32;
        let multiplier = 1u64 << shift;
        let initial_ms = self.options.initial_delay.as_millis() as u64;
        let max_ms = self.options.max_delay.as_millis() as u64;
        let raw_delay_ms = initial_ms.saturating_mul(multiplier);
        let capped_ms = raw_delay_ms.min(max_ms);

        RestartDecision::Restart {
            attempt: n as u32,
            delay: Duration::from_millis(capped_ms),
            exits_in_window: n,
        }
    }

    /// Return the number of exits currently remembered in the window.
    pub fn exits_in_window(&self) -> usize {
        self.exit_timestamps_ms.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> RestartPolicy {
        RestartPolicy::new(ResolvedRestartPolicy {
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            max_restarts: 5,
            restart_window: Duration::from_secs(600), // 10 minutes
        })
    }

    #[test]
    fn record_exit_should_follow_exponential_backoff_and_give_up_on_sixth_exit() {
        // Arrange
        let mut policy = default_policy();
        let mut now_ms = 1_000_000;

        // Act & Assert: restarts 1..=5 double each time (2s, 4s, 8s, 16s, 32s)
        let expected_secs = [2, 4, 8, 16, 32];
        for (idx, &secs) in expected_secs.iter().enumerate() {
            now_ms += 1_000;
            let decision = policy.record_exit(now_ms);
            assert_eq!(
                decision,
                RestartDecision::Restart {
                    attempt: (idx + 1) as u32,
                    delay: Duration::from_secs(secs),
                    exits_in_window: idx + 1,
                }
            );
        }

        // 6th exit within 10 minutes gives up
        now_ms += 1_000;
        assert_eq!(
            policy.record_exit(now_ms),
            RestartDecision::GiveUp { exits_in_window: 6 }
        );
    }

    #[test]
    fn record_exit_should_cap_delay_at_max_delay() {
        // Arrange
        let mut policy = RestartPolicy::new(ResolvedRestartPolicy {
            initial_delay: Duration::from_secs(10),
            max_delay: Duration::from_secs(25),
            max_restarts: 5,
            restart_window: Duration::from_secs(600),
        });

        // Act & Assert
        assert_eq!(
            policy.record_exit(1_000),
            RestartDecision::Restart {
                attempt: 1,
                delay: Duration::from_secs(10),
                exits_in_window: 1
            }
        );
        assert_eq!(
            policy.record_exit(2_000),
            RestartDecision::Restart {
                attempt: 2,
                delay: Duration::from_secs(20),
                exits_in_window: 2
            }
        );
        assert_eq!(
            policy.record_exit(3_000),
            RestartDecision::Restart {
                attempt: 3,
                delay: Duration::from_secs(25),
                exits_in_window: 3
            }
        );
    }

    #[test]
    fn record_exit_should_forget_exits_older_than_restart_window() {
        // Arrange
        let mut policy = default_policy();

        // Act: 3 crashes near t = 0
        policy.record_exit(1_000);
        policy.record_exit(2_000);
        policy.record_exit(3_000);

        // Next crash 11 minutes later (660_000 ms > 600_000 ms window)
        let decision = policy.record_exit(3_000 + 660_000);

        // Assert: restarts at initial delay (2s) as attempt 1
        assert_eq!(
            decision,
            RestartDecision::Restart {
                attempt: 1,
                delay: Duration::from_secs(2),
                exits_in_window: 1
            }
        );
    }

    #[test]
    fn record_exit_should_give_up_immediately_when_max_restarts_is_zero() {
        // Arrange
        let mut policy = RestartPolicy::new(ResolvedRestartPolicy {
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            max_restarts: 0,
            restart_window: Duration::from_secs(600),
        });

        // Act & Assert
        assert_eq!(
            policy.record_exit(1_000),
            RestartDecision::GiveUp { exits_in_window: 1 }
        );
    }
}
