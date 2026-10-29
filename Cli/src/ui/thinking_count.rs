//! A thinking's token count on screen, climbing to what Anthropic reported.
//!
//! Anthropic reports a thinking about once a second, a few hundred tokens at a
//! time (`Event::ThinkingTokens`), so the number itself moves in steps. Claude
//! Code shows the same reports climbing one number after another (2.1.309, its
//! spinner's counter, read from its bundle 2026-10-23): what is shown chases
//! the reported number in fractional animation ticks, a step every 50 ms —
//! three ticks while it is less than 70 away, 15% of the way (at least
//! eight) while less than 200, fifty beyond — and never passes it. Claude Code
//! jumps straight there when more than 2,000 characters behind; here a large
//! distance is closed over one report's interval instead, so nothing is seen
//! to leap (owner, 2026-10-23). Nothing shown is ever more than Anthropic
//! reported: the chase only decides how fast the screen reaches it.
//!
//! Claude Code's counter is its whole turn's; this one is a thinking's, and a
//! thinking ends when its round goes on to write — its answer or a tool's
//! arguments. What is left then (the last reports, and the count the usage
//! bills, which comes only after the tool's arguments) is closed within
//! [`SETTLE_STEPS`] instead of ticking on beside the tool (owner, 2026-10-23).
use std::time::{Duration, Instant};

/// One step of the chase.
const STEP: Duration = Duration::from_millis(50);

/// Fractional animation ticks per reported token, never source characters.
const SUBSTEPS_PER_TOKEN: u64 = 4;

/// Steps over which a distance Claude Code would jump is closed instead:
/// Anthropic's reports come about a second apart (measured 2026-10-23), so the
/// screen reaches each one as the next arrives.
const CATCH_UP_STEPS: u64 = 20;

/// Steps over which an ended thinking's count reaches its report: a quarter
/// of a second, evenly.
const SETTLE_STEPS: u64 = 5;

/// Where the count on screen stands, and when it last stepped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chase {
    shown: u64,
    stepped: Instant,
    /// The report it is going to, in fractional animation ticks, and the step far from it:
    /// fifty, or the distance over [`CATCH_UP_STEPS`] when that is more —
    /// fixed when the report came, so the distance closes evenly.
    toward: u64,
    pace: u64,
    /// The thinking ended: what is left is closed in [`SETTLE_STEPS`].
    settling: bool,
}

impl Chase {
    /// A count that starts from nothing, as a thinking's first report comes.
    pub fn start(now: Instant) -> Chase {
        Chase {
            shown: 0,
            stepped: now,
            toward: 0,
            pace: 50,
            settling: false,
        }
    }

    /// A count already at `tokens`: a window opened while a thinking runs
    /// shows what was reported so far, as Claude Code's counter starts at
    /// what it finds when it first appears.
    pub fn at(tokens: u64, now: Instant) -> Chase {
        let shown = tokens.saturating_mul(SUBSTEPS_PER_TOKEN);
        Chase {
            shown,
            stepped: now,
            toward: shown,
            pace: 50,
            settling: false,
        }
    }

    /// The thinking ended: whatever it still has to reach, now or when the
    /// billed count comes, it reaches within [`SETTLE_STEPS`].
    pub fn settle(&mut self) {
        if !self.settling {
            self.settling = true;
            self.pace = self
                .shown
                .abs_diff(self.toward)
                .div_ceil(SETTLE_STEPS)
                .max(1);
        }
    }

    /// The tokens on screen.
    pub fn tokens(&self) -> u64 {
        (self.shown + SUBSTEPS_PER_TOKEN / 2) / SUBSTEPS_PER_TOKEN
    }

    /// Step toward `target` tokens for the time gone by since the last step.
    /// Whether the number on screen changed.
    pub fn advance(&mut self, target: u64, now: Instant) -> bool {
        let before = self.tokens();
        let target = target.saturating_mul(SUBSTEPS_PER_TOKEN);
        if target != self.toward {
            self.toward = target;
            let gap = self.shown.abs_diff(target);
            self.pace = if self.settling {
                gap.div_ceil(SETTLE_STEPS).max(1)
            } else {
                gap.div_ceil(CATCH_UP_STEPS).max(50)
            };
        }
        // The clock steps whether or not there is a way to go, as Claude
        // Code's does every frame: a report coming after a still moment is
        // climbed to from then, never by the steps the moment held.
        let steps = now.saturating_duration_since(self.stepped).as_millis() / STEP.as_millis();
        if steps == 0 {
            return false;
        }
        self.stepped += STEP * steps as u32;
        for _ in 0..steps {
            let gap = self.shown.abs_diff(target);
            if gap == 0 {
                break;
            }
            let step = if self.settling {
                self.pace
            } else if gap < 70 {
                3
            } else if gap < 200 {
                (gap * 15).div_ceil(100).max(8)
            } else {
                self.pace
            };
            self.shown = if self.shown < target {
                (self.shown + step).min(target)
            } else {
                self.shown.saturating_sub(step).max(target)
            };
        }
        self.tokens() != before
    }

    /// Whether it still has a way to go to `target` tokens.
    #[cfg(test)]
    pub fn moving(&self, target: u64) -> bool {
        self.shown != target.saturating_mul(SUBSTEPS_PER_TOKEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(chase: &mut Chase, target: u64, from: Instant, ms: u64) -> Vec<u64> {
        (1..=ms / 50)
            .map(|i| {
                chase.advance(target, from + Duration::from_millis(i * 50));
                chase.tokens()
            })
            .collect()
    }

    /// Claude Code's steps, in characters: three while under 70 away, 15% (at
    /// least eight) while under 200, fifty beyond.
    #[test]
    fn it_steps_the_way_claude_codes_counter_does() {
        let t0 = Instant::now();
        let mut chase = Chase::start(t0);
        // 50 tokens = 200 characters away: fifty, then 15% of 150 (23), ...
        chase.advance(50, t0 + STEP);
        assert_eq!(chase.shown, 50);
        chase.advance(50, t0 + STEP * 2);
        assert_eq!(chase.shown, 50 + 23);
        chase.advance(50, t0 + STEP * 3);
        assert_eq!(chase.shown, 73 + 20, "15% of 127 is 19.05, up to 20");
        let mut near = Chase::at(10, t0);
        near.advance(20, t0 + STEP);
        assert_eq!(near.shown, 43, "three characters while under 70 away");
        // Several steps owed at once are all taken.
        let mut late = Chase::start(t0);
        late.advance(50, t0 + STEP * 2);
        assert_eq!(late.shown, 73);
        // Less than a step gone by: nothing moves.
        let mut early = Chase::start(t0);
        assert!(!early.advance(50, t0 + Duration::from_millis(49)));
    }

    /// Owner, 2026-10-23: what is on screen is never more than Anthropic
    /// reported, climbs one number after another, and reaches the report
    /// exactly; a later, lower billed count is gone down to the same way.
    #[test]
    fn it_never_passes_the_report_and_reaches_it_exactly() {
        let t0 = Instant::now();
        let mut chase = Chase::start(t0);
        let shown = walk(&mut chase, 350, t0, 3000);
        assert!(shown.iter().all(|n| *n <= 350), "{shown:?}");
        assert!(shown.windows(2).all(|w| w[0] <= w[1]), "{shown:?}");
        assert_eq!(*shown.last().unwrap(), 350);
        assert!(!chase.moving(350));
        let down = walk(&mut chase, 340, t0 + Duration::from_millis(3000), 1000);
        assert!(down.iter().all(|n| (340..=350).contains(n)), "{down:?}");
        assert_eq!(*down.last().unwrap(), 340);
    }

    /// Where Claude Code would jump (more than 2,000 characters behind), the
    /// distance is closed evenly over a report's interval — no step more than
    /// a twentieth of it — and the last few tokens one by one, as Claude
    /// Code's counter ends; a distance it steps fifty at a time is stepped
    /// fifty at a time here too.
    #[test]
    fn a_large_report_is_reached_evenly_without_a_leap() {
        let t0 = Instant::now();
        let mut chase = Chase::at(1_000, t0);
        let shown = walk(&mut chase, 3_000, t0, 2500);
        let steps: Vec<u64> = std::iter::once(1_000)
            .chain(shown.iter().copied())
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| w[1] - w[0])
            .collect();
        assert!(steps.iter().all(|s| *s <= 2_000 / 20 + 1), "{steps:?}");
        assert!(
            shown[19] >= 2_950,
            "most of it within a second: {}",
            shown[19]
        );
        assert_eq!(*shown.last().unwrap(), 3_000, "{shown:?}");
        let mut usual = Chase::at(100, t0);
        usual.advance(350, t0 + STEP);
        assert_eq!(usual.shown, 400 + 50, "Claude Code's fifty");
    }

    /// A report coming after a still moment is climbed to from that moment:
    /// the steps the moment held are not taken at once.
    #[test]
    fn a_still_moment_is_not_paid_back_in_one_leap() {
        let t0 = Instant::now();
        let mut chase = Chase::at(50, t0);
        for i in 1..=20u64 {
            chase.advance(50, t0 + STEP * i as u32);
        }
        let then = t0 + STEP * 20;
        chase.advance(300, then + STEP);
        assert!(chase.tokens() <= 50 + 13, "{}", chase.tokens());
    }

    /// Owner, 2026-10-23: once the thinking ended — its round writing its
    /// answer or a tool's arguments — what is left, the billed count that
    /// comes after included, is reached within a quarter of a second, evenly,
    /// never past it; not ticked on beside the tool.
    #[test]
    fn an_ended_thinking_reaches_its_count_within_a_quarter_second() {
        let t0 = Instant::now();
        let mut chase = Chase::at(50, t0);
        chase.settle();
        let shown = walk(&mut chase, 69, t0, 250);
        assert_eq!(*shown.last().unwrap(), 69, "{shown:?}");
        assert!(shown.iter().all(|n| *n <= 69), "{shown:?}");
        assert!(shown.windows(2).all(|w| w[0] < w[1]), "evenly: {shown:?}");
        let mut far = Chase::at(1_950, t0);
        far.settle();
        assert_eq!(*walk(&mut far, 2_120, t0, 250).last().unwrap(), 2_120);
    }

    #[test]
    fn a_count_found_running_starts_where_it_stands() {
        let t0 = Instant::now();
        let chase = Chase::at(750, t0);
        assert_eq!(chase.tokens(), 750);
        assert!(!chase.moving(750));
    }
}
