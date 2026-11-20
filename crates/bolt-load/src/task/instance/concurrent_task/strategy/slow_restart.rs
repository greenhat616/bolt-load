//! Restarting slow runners under the doubling strategy (design §9.5,
//! §14.15): a runner that delivered less than [`SLOW_RATIO`] of the median
//! of the runners over [`SLOW_WINDOW`] is stopped, and its range retried
//! after a backoff.
//!
//! Only runners with a full window are judged: their bytes make the
//! median, and the restart needs at least [`MIN_JUDGED_RUNNERS`] of them.
//! At most one runner is restarted per strategy tick, the slowest.
//!
//! A runner delivers its bytes a frame at a time, so the bytes the planner
//! accepted over a window may be off by up to a frame either way: the
//! bytes it holds at the window's end, and a frame that straddles the
//! window's start. The comparison allows for that, see [`slow_runner`].

use std::time::Duration;

use crate::task::RunnerId;

/// The window a runner is judged over.
pub const SLOW_WINDOW: Duration = Duration::from_secs(10);
/// A runner that delivered less than this share of the median is slow.
pub const SLOW_RATIO: f64 = 0.25;
/// How long a running runner may go without delivering a frame or reading
/// any of its body before it is restarted, with no window and no median
/// needed (design §14.19): three strategy ticks, past the hedge's 2 s idle
/// time.
pub const SILENT_RESTART_AFTER: Duration = Duration::from_millis(4500);
/// The fewest judged runners a median is taken of.
///
/// Every judged runner is active, so this is condition 3 of design §9.5,
/// at least 3 active runners, made stricter: a runner too young to be
/// judged does not count. The median of two runners is their mean, so a
/// single fast runner would set the bar alone.
pub const MIN_JUDGED_RUNNERS: usize = 3;

/// A running runner with a full window.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgedRunner {
    pub runner_id: RunnerId,
    /// The bytes the planner accepted from it over the window; every
    /// judged runner has the same window.
    pub bytes: u64,
    /// The bytes of its chunk the planner has not accepted yet.
    pub remaining: u64,
}

/// The inputs of one decision.
#[derive(Debug)]
pub struct SlowRestartContext {
    /// The runners to judge, in any order.
    pub runners: Vec<JudgedRunner>,
    /// The most bytes a runner holds before it delivers them as a frame:
    /// how far its window's bytes may be off either way.
    pub frame: u64,
    /// The smallest chunk the planner hands out.
    pub min_chunk_size: u64,
    /// Whether new connections wait for the task's congestion to end
    /// (design §8.1).
    pub congested: bool,
    /// The task's speed limit; `None` until there is one (design §10).
    pub speed_limit: Option<f64>,
}

/// The median of `values`, the mean of the two middle ones for an even
/// count; `None` when there are none.
fn median(values: &mut [f64]) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    match values.len() {
        0 => None,
        len if len % 2 == 1 => Some(values[middle]),
        _ => Some((values[middle - 1] + values[middle]) / 2.0),
    }
}

/// The runner to restart, if any: the slowest of those that are slow even
/// at the most a frame could hide, and that have at least twice
/// `min_chunk_size` left, so a runner near its end is left to finish.
///
/// Slow means `bytes + frame < SLOW_RATIO × (median − frame)`, the median
/// of the judged runners' bytes, and `median − frame` at least 0: the
/// runner's most against the median's least. At MiB/s a frame changes
/// nothing; where every runner delivers a frame or two a window, none is
/// judged, as its bytes say too little; a runner that delivered nothing
/// next to fast ones still is.
///
/// None while fewer than [`MIN_JUDGED_RUNNERS`] runners are judged, while
/// the task is congested, as the range could not connect again before the
/// congestion ends anyway, and while a speed limit is set, which slows
/// runners on purpose.
pub fn slow_runner(context: &SlowRestartContext) -> Option<RunnerId> {
    if context.speed_limit.is_some() || context.congested {
        return None;
    }
    if context.runners.len() < MIN_JUDGED_RUNNERS {
        return None;
    }
    let mut bytes: Vec<f64> = context
        .runners
        .iter()
        .map(|runner| runner.bytes as f64)
        .collect();
    let frame = context.frame as f64;
    let bar = SLOW_RATIO * (median(&mut bytes)? - frame).max(0.0);
    let least_left = context.min_chunk_size.saturating_mul(2);
    context
        .runners
        .iter()
        .filter(|runner| {
            (runner.bytes.saturating_add(context.frame) as f64) < bar
                && runner.remaining >= least_left
        })
        .min_by_key(|runner| (runner.bytes, runner.runner_id))
        .map(|runner| runner.runner_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const MIN_CHUNK: u64 = MIB;
    /// The runner's frame.
    const FRAME: u64 = crate::runner::BUFFER_SIZE as u64;

    /// Runners that delivered `bytes`, by ID from 0, each with plenty
    /// left.
    fn runners(bytes: &[u64]) -> Vec<JudgedRunner> {
        bytes
            .iter()
            .enumerate()
            .map(|(runner_id, &bytes)| JudgedRunner {
                runner_id,
                bytes,
                remaining: 64 * MIB,
            })
            .collect()
    }

    fn context(runners: Vec<JudgedRunner>) -> SlowRestartContext {
        SlowRestartContext {
            runners,
            frame: FRAME,
            min_chunk_size: MIN_CHUNK,
            congested: false,
            speed_limit: None,
        }
    }

    fn slow(bytes: &[u64]) -> Option<RunnerId> {
        slow_runner(&context(runners(bytes)))
    }

    /// The median's bytes for which `bytes` is exactly at the bound:
    /// `bytes + frame = SLOW_RATIO × (median − frame)`.
    fn median_at_the_bound(bytes: u64) -> u64 {
        4 * (bytes + FRAME) + FRAME
    }

    /// Slow is below a quarter of the median, a frame allowed on both
    /// sides: exactly at the bound is not slow, as §9.5 asks for
    /// "below"; a byte more on the median's side is.
    #[test]
    fn test_a_runner_below_a_quarter_of_the_median_is_restarted() {
        for bytes in [0, MIB] {
            let median = median_at_the_bound(bytes);
            assert_eq!(slow(&[median, median, bytes]), None, "{bytes}");
            assert_eq!(slow(&[median, median + 4, bytes]), None, "{bytes}");
            assert_eq!(slow(&[median + 4, median + 4, bytes]), Some(2), "{bytes}");
            assert_eq!(slow(&[median, bytes, median]), None, "{bytes}");
        }
        assert_eq!(slow(&[8 * MIB, 8 * MIB, 8 * MIB]), None);
    }

    /// Runners at the same low rate deliver their frames at different
    /// times: in a window, one may show a frame or two more than another,
    /// or none at all. None of them is slow.
    #[test]
    fn test_runners_a_frame_apart_are_not_slow() {
        // 2 KiB/s over 10.5 s is 21 KiB: a frame or none.
        assert_eq!(slow(&[FRAME, FRAME, 0]), None);
        // 6 KiB/s is two frames or one.
        assert_eq!(slow(&[2 * FRAME, 2 * FRAME, FRAME]), None);
        assert_eq!(slow(&[2 * FRAME, 2 * FRAME, 0]), None);
        assert_eq!(slow(&[2 * FRAME, FRAME, 2 * FRAME, 0]), None);
    }

    /// A runner that delivered nothing next to fast ones is slow, however
    /// the frames fall.
    #[test]
    fn test_a_silent_runner_next_to_fast_ones_is_restarted() {
        assert_eq!(slow(&[7 * MIB, 7 * MIB, 0]), Some(2));
        assert_eq!(slow(&[7 * MIB, 0, 7 * MIB - FRAME]), Some(1));
        // A frame short of nothing.
        assert_eq!(slow(&[7 * MIB, 7 * MIB, FRAME]), Some(2));
    }

    /// A runner near its end is left to finish: under twice the minimum
    /// chunk size left it is not restarted, at exactly twice it is.
    #[test]
    fn test_a_runner_near_its_end_is_not_restarted() {
        let mut runners = runners(&[8 * MIB, 8 * MIB, 0]);
        runners[2].remaining = 2 * MIN_CHUNK - 1;
        assert_eq!(slow_runner(&context(runners.clone())), None);
        runners[2].remaining = 2 * MIN_CHUNK;
        assert_eq!(slow_runner(&context(runners)), Some(2));
    }

    /// Two judged runners are too few, however far apart they are; a
    /// third makes a median.
    #[test]
    fn test_no_restart_with_two_runners() {
        let fast = 100 * MIB;
        assert_eq!(slow(&[fast, 0]), None);
        assert_eq!(slow(&[0]), None);
        assert_eq!(slow(&[]), None);
        assert_eq!(slow(&[fast, 0, fast]), Some(1));
    }

    /// The median of an even count is the mean of the two middle ones.
    #[test]
    fn test_the_median_of_an_even_count() {
        // The middle two, 2 and 6 MiB and a frame: the bound is 1 MiB.
        let at = |middle: u64| middle * MIB + FRAME;
        // A runner exactly at it is not slow; with the middle two a byte
        // more in all, it is.
        assert_eq!(slow(&[MIB - FRAME, at(2), at(6), at(6)]), None);
        assert_eq!(slow(&[MIB - FRAME, at(2) + 2, at(6), at(6)]), Some(0));
        // The upper middle alone would make a runner at 1 MiB slow; the
        // lower one would not make one at 0.75 MiB slow.
        assert_eq!(slow(&[MIB, at(2), at(6), at(6)]), None);
        assert_eq!(slow(&[3 * MIB / 4 - FRAME, at(2), at(6), at(6)]), Some(0));
    }

    /// At most one runner a decision, the slowest; a tie goes to the
    /// lowest ID.
    #[test]
    fn test_only_the_slowest_is_restarted() {
        let fast = 8 * MIB;
        assert_eq!(slow(&[fast, 2, fast, 1, fast]), Some(3));
        assert_eq!(slow(&[fast, 1, fast, 1, fast]), Some(1));
        // The slowest is near its end: the next slowest goes.
        let mut runners = runners(&[fast, 2, fast, 1, fast]);
        runners[3].remaining = MIN_CHUNK;
        assert_eq!(slow_runner(&context(runners)), Some(1));
    }

    /// No restart while a speed limit slows the runners on purpose, or
    /// while the task is congested.
    #[test]
    fn test_no_restart_under_a_speed_limit_or_a_congestion() {
        let fast = 8 * MIB;
        let limited = SlowRestartContext {
            speed_limit: Some(MIB as f64),
            ..context(runners(&[fast, fast, 0]))
        };
        assert_eq!(slow_runner(&limited), None);
        let congested = SlowRestartContext {
            congested: true,
            ..context(runners(&[fast, fast, 0]))
        };
        assert_eq!(slow_runner(&congested), None);
    }

    /// Without values the median is not defined.
    #[test]
    fn test_median() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [3.0]), Some(3.0));
        assert_eq!(median(&mut [5.0, 1.0, 3.0]), Some(3.0));
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
