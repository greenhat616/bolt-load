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
        tracing::trace!(
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
                        && context.current_concurrency.saturating_mul(2) < context.max_concurrency
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
                        && context.current_concurrency.saturating_add(1) < context.max_concurrency
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
