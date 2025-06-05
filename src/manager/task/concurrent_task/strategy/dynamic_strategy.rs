use super::{Strategy, StrategyAction};

enum DynamicPlannerStage {
    QuickStart,
    Normal,
}

pub struct DynamicStrategy {
    threashold1: f64,
    threashold2: f64,
    max_thread: usize,
    current_stage: DynamicPlannerStage,
    previous_total_download_speed: f64,
}

impl DynamicStrategy {
    pub fn new(threashold1: Option<f64>, threashold2: Option<f64>, max_thread: Option<usize>) -> Self {
        Self {
            threashold1: threashold1.unwrap_or(10.0),
            // 1MB/s minimum speed threashold
            threashold2: threashold2.unwrap_or(1024.0 * 8.0),
            max_thread: max_thread.unwrap_or(16),
            current_stage: DynamicPlannerStage::QuickStart,
            previous_total_download_speed: 0.0,
        }
    }
}

struct DynamicStrategyContext {
    total_download_speed: f64,
    average_download_speed: f64,
    runner_count: usize,
    longest_runner: usize,
}

impl Strategy for DynamicStrategy {
    type Context = DynamicStrategyContext;

    fn step(&mut self, context: &Self::Context) -> Vec<StrategyAction> {
        let mut result = Vec::new();
        let total_download_speed = context.total_download_speed;
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
                        - (self.previous_total_download_speed + context.average_download_speed))
                        .abs()
                        > self.threashold2
                        && context.runner_count < self.max_thread
                    {
                        result.push(StrategyAction::SplitGivenTask(context.longest_runner));
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
