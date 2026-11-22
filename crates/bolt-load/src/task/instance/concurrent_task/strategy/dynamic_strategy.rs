use super::{Strategy, StrategyAction};
use crate::task::RunnerId;

#[derive(Debug)]
enum DynamicPlannerStage {
    QuickStart,
    Normal,
}

pub struct DynamicStrategy {
    threashold1: f64,
    threashold2: f64,
    current_stage: DynamicPlannerStage,
    previous_total_download_speed: f64,
}

impl Default for DynamicStrategy {
    fn default() -> Self {
        Self {
            threashold1: 1024.0 * 1024.0,
            // 1MB/s minimum speed threashold
            threashold2: 1024.0 * 1024.0,
            current_stage: DynamicPlannerStage::QuickStart,
            previous_total_download_speed: 0.0,
        }
    }
}

impl DynamicStrategy {
    #[expect(dead_code, reason = "the strategy is built with its defaults")]
    pub fn new(threashold1: f64, threashold2: f64) -> Self {
        Self {
            threashold1,
            threashold2,
            ..Self::default()
        }
    }
}

#[derive(Debug)]
pub struct DynamicStrategyContext {
    /// the total download speed of the task
    pub speed: f64,
    /// the average download speed of the runners
    pub per_runner_speed: f64,
    /// the current concurrency of the task
    pub current_concurrency: usize,
    /// the maximum concurrency of the task
    pub max_concurrency: usize,
    /// the planned chunk size of the task
    pub planned_chunk_size: u64,
    /// The largest remaining download bytes of a runner
    pub remaining_largest_runner_id: RunnerId,
}

impl Strategy for DynamicStrategy {
    type Context<'a> = DynamicStrategyContext;

    fn name() -> &'static str {
        "dynamic"
    }

    fn step(&mut self, context: &mut Self::Context<'static>) -> Vec<StrategyAction> {
        bolt_load_utils::telemetry::trace!(
            "Strategy context {:?}, current stage {:?}",
            context,
            self.current_stage
        );

        let mut result = Vec::new();
        let total_download_speed = context.speed;
        while result.is_empty() {
            match self.current_stage {
                DynamicPlannerStage::QuickStart => {
                    if (total_download_speed - 2.0 * self.previous_total_download_speed).abs()
                        > self.threashold1
                        // Every running runner is split: room for twice as many.
                        && context.current_concurrency.saturating_mul(2) <= context.max_concurrency
                    {
                        result.push(StrategyAction::SplitAllTask(context.planned_chunk_size));
                        self.previous_total_download_speed = total_download_speed;
                    } else {
                        self.current_stage = DynamicPlannerStage::Normal;
                    }
                }
                DynamicPlannerStage::Normal => {
                    if (total_download_speed
                        - (self.previous_total_download_speed + context.per_runner_speed))
                        .abs()
                        > self.threashold2
                        // Room for one more runner.
                        && context.current_concurrency < context.max_concurrency
                    {
                        // If the current thread download speed is faster than threashold2, split it into two task
                        result.push(StrategyAction::SplitGivenTask(
                            context.remaining_largest_runner_id,
                        ));
                        self.previous_total_download_speed = total_download_speed;
                    } else {
                        self.previous_total_download_speed = total_download_speed;
                        break;
                    }
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEED: f64 = 2.0 * 1024.0 * 1024.0;

    fn step(current_concurrency: usize, max_concurrency: usize) -> Vec<StrategyAction> {
        DynamicStrategy::default().step(&mut DynamicStrategyContext {
            speed: SPEED,
            per_runner_speed: 0.0,
            current_concurrency,
            max_concurrency,
            planned_chunk_size: 1,
            remaining_largest_runner_id: 0,
        })
    }

    /// A quick start round may double the runners up to the limit, and a
    /// split in the normal stage may take the last slot.
    #[test]
    fn test_every_slot_up_to_the_limit_is_used() {
        assert!(matches!(step(2, 4)[..], [StrategyAction::SplitAllTask(1)]));
        assert!(matches!(
            step(3, 4)[..],
            [StrategyAction::SplitGivenTask(0)]
        ));
        assert!(matches!(step(1, 2)[..], [StrategyAction::SplitAllTask(1)]));
        assert!(step(4, 4).is_empty());
        assert!(step(1, 1).is_empty());
    }
}
