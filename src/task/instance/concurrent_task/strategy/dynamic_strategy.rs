use crate::task::instance::concurrent_task::DEFAULT_MAX_CONCURRENCY;

use super::{Strategy, StrategyAction};

enum DynamicPlannerStage {
    QuickStart,
    Normal,
}

pub struct DynamicStrategy {
    threashold1: f64,
    threashold2: f64,
    max_concurrency: usize,
    current_stage: DynamicPlannerStage,
    previous_total_download_speed: f64,
}

impl Default for DynamicStrategy {
    fn default() -> Self {
        Self {
            threashold1: 10.0,
            // 1MB/s minimum speed threashold
            threashold2: 1024.0 * 8.0,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            current_stage: DynamicPlannerStage::QuickStart,
            previous_total_download_speed: 0.0,
        }
    }
}

impl DynamicStrategy {
    pub fn new(threashold1: f64, threashold2: f64, max_concurrency: usize) -> Self {
        Self {
            threashold1,
            threashold2,
            max_concurrency,
            ..Self::default()
        }
    }

    pub fn new_with_max_concurrency(max_concurrency: usize) -> Self {
        Self {
            max_concurrency,
            ..Self::default()
        }
    }
}

pub struct DynamicStrategyContext {
    /// the total download speed of the task
    pub speed: f64,
    /// the average download speed of the runners
    pub per_runner_speed: f64,
    /// the current concurrency of the task
    pub current_concurrency: usize,
    /// The largest remaining download bytes of a runner
    pub remaining_largest_runner: usize,
}

impl Strategy for DynamicStrategy {
    type Context = DynamicStrategyContext;

    fn step(&mut self, context: &Self::Context) -> Vec<StrategyAction> {
        let mut result = Vec::new();
        let total_download_speed = context.speed;
        while result.is_empty() {
            match self.current_stage {
                DynamicPlannerStage::QuickStart => {
                    if (total_download_speed - 2.0 * self.previous_total_download_speed).abs()
                        < self.threashold1
                    {
                        result.push(StrategyAction::SplitAllTask);
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
                        && context.current_concurrency < self.max_concurrency
                    {
                        result.push(StrategyAction::SplitGivenTask(context.remaining_largest_runner));
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
