use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    ops::Range,
    sync::Arc,
};

use bytes::Bytes;
use futures::FutureExt;
use smol_cancellation_token::CancellationToken;
use statig::{Response::*, prelude::*};

use crate::{
    adapter::{AnyAdapter, UnretryableError},
    manager::{
        Progress, RunnerId,
        task::{RunningPayload, TaskError},
    },
    runner::{TaskFailedKind, TaskRunner},
};

use super::{Result, Task};

mod chunk_planner;
mod file;
mod strategy;

use chunk_planner::*;
use strategy::*;

/// Starts with 1 task, and then split the task by dynamic strategy
const INITIAL_TASK_NUM: usize = 1;

pub struct ConcurrentTask {}

pub struct RunnerChunks {
    map: BTreeMap<RunnerId, Vec<Range<u64>>>,
}

struct TaskSpeed {
    pub current: u64,
    pub avg: u64,
}

pub struct ConcurrentTaskInner {
    /// the runners of this task
    runners: Vec<TaskRunner>,
    adapter: Option<Arc<AnyAdapter>>,
    /// the strategy manager of this task
    dynamic_strategy: DynamicStrategy,
    /// the chunk planner of this task
    chunk_planner: ChunkPlanner,
    /// The downloaded chunks of this task
    /// It is None if the task is in single-thread/singleton mode
    downloaded_chunks: Vec<Range<u64>>,
    progress: Progress,
}

struct Context {
    poll: VecDeque<()>,
}

enum Event {
    Run(RunningPayload),
    Step,
}

impl ConcurrentTaskInner {
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        // TODO: use backon to retry
        let meta = self
            .adapter
            .as_ref()
            .unwrap()
            .retrieve_meta()
            .await
            .map_err(TaskError::RetrieveMetaFailed)?;
        let total = if meta.content_size == 0 {
            return Err(TaskError::RetrieveMetaFailed(
                UnretryableError::FallbackToSingleton(
                    "content size is 0; concurrent task does not support 0-size file".to_string(),
                ),
            ));
        } else {
            meta.content_size
        };
        if self.progress.total.is_some_and(|t| t != total) {
            self.progress.total = Some(total);
            self.progress.downloaded = 0;
        }
        Ok(())
    }

    async fn download(&mut self) -> Result<()> {
        let mut meter: u32 = 0;
        todo!()
    }
}

#[state_machine(initial = "State::stopped(Cell::new(None))")]
impl ConcurrentTaskInner {
    #[state]
    fn stopped(
        &mut self,
        context: &mut Context,
        reason: &mut Cell<Option<Result<()>>>,
        event: &Event,
    ) -> Response<State> {
        match event {
            Event::Run(payload) => {
                context.poll.push_back(());
                Transition(State::initializing(payload.cancel_token.clone()))
            }
            _ => Super,
        }
    }

    #[superstate]
    async fn running(event: &Event) -> Response<State> {
        Super
    }

    #[state(superstate = "running")]
    async fn initializing(
        &mut self,
        cancel_token: &mut CancellationToken,
        context: &mut Context,
        event: &Event,
    ) -> Response<State> {
        match event {
            Event::Step => {
                let task = async {
                    match self.retrieve_meta().await {
                        Ok(_) => {
                            context.poll.push_back(());
                            Transition(State::downloading(cancel_token.clone()))
                        }
                        Err(e) => Transition(State::stopped(Cell::new(Some(Err(e))))),
                    }
                }
                .fuse();
                let cancel = cancel_token.cancelled().fuse();
                futures::pin_mut!(task, cancel);
                futures::select_biased! {
                    _ = cancel => {
                        Transition(State::stopped(Cell::new(Some(Err(
                            TaskError::Failed(TaskFailedKind::Cancelled),
                        )))))
                    }
                    res = task => { res }
                }
            }
            _ => Super,
        }
    }

    #[state(superstate = "running")]
    fn downloading(
        &mut self,
        cancel_token: &mut CancellationToken,
        context: &mut Context,
        event: &Event,
    ) -> Response<State> {
        Super
    }
}
