//! Runner Manager - 统一管理 runner 的生命周期和通信
//!
//! 这个模块提供了一个统一的接口来管理：
//! - Runner ID 的分配和回收 (IdGenerator)
//! - Chunk 的分配和进度跟踪 (ChunkPlanner)
//! - Runner 消息的聚合接收 (RunnerNotification)
//! - 向特定 runner 发送控制消息的通道

use std::{collections::HashMap, ops::Range};

use async_channel::{Receiver, Sender, TrySendError};

use super::{ChunkPlanner, PlannerGuard, RunnerNotification, chunk_planner::ChunkState};
use crate::{
    runner::RunnerMessageConsumer,
    task::{ControlEvent, RunnerId, instance::Generator},
};

/// 控制通道的默认容量
const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 8;

/// 控制消息发送端的类型别名
pub type ControlSender = Sender<ControlEvent>;

/// 控制消息接收端的类型别名
pub type ControlReceiver = Receiver<ControlEvent>;

/// 发送消息时可能遇到的错误
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("Runner {0} not found")]
    RunnerNotFound(RunnerId),
    #[error("Control channel is full")]
    ChannelFull,
    #[error("Control channel is closed")]
    ChannelClosed,
}

trait ManagerControlMapExt {
    fn send_message(&self, runner_id: RunnerId, message: ControlEvent) -> Result<(), SendError>;
}

impl ManagerControlMapExt for HashMap<RunnerId, ControlSender> {
    fn send_message(&self, runner_id: RunnerId, message: ControlEvent) -> Result<(), SendError> {
        if let Some(tx) = self.get(&runner_id) {
            tx.try_send(message)?;
            Ok(())
        } else {
            Err(SendError::RunnerNotFound(runner_id))
        }
    }
}

impl<T> From<TrySendError<T>> for SendError {
    fn from(err: TrySendError<T>) -> Self {
        match err {
            TrySendError::Full(_) => SendError::ChannelFull,
            TrySendError::Closed(_) => SendError::ChannelClosed,
        }
    }
}

/// Runner 注册信息
pub struct RunnerRegistration {
    /// Runner ID
    pub runner_id: RunnerId,
    /// 控制消息接收端
    pub control_rx: ControlReceiver,
}

/// Runner Manager - 统一管理 runner 的生命周期和通信
pub struct RunnerManager {
    /// ID 生成器，用于管理 runner ID 的分配和回收
    id_generator: Generator,
    /// Chunk 规划器，用于管理 chunk 的分配和进度
    chunk_planner: ChunkPlanner,
    /// Runner 通知聚合器，用于接收来自 runner 的消息
    runner_notification: RunnerNotification,
    /// 控制通道映射，用于向特定 runner 发送消息
    control_channels: HashMap<RunnerId, ControlSender>,
}

impl RunnerManager {
    /// 创建一个新的 RunnerManager
    ///
    /// # Arguments
    /// * `total` - 下载文件的总大小
    /// * `max_concurrency` - 最大并发数
    pub fn new(total: u64, max_concurrency: usize) -> Self {
        Self {
            id_generator: Generator::new(max_concurrency),
            chunk_planner: ChunkPlanner::new(total),
            runner_notification: RunnerNotification::with_capacity(max_concurrency),
            control_channels: HashMap::with_capacity(max_concurrency),
        }
    }

    // ==================== ID Generator 相关方法 ====================

    /// 分配一个新的 runner ID 和对应的控制通道
    ///
    /// # Returns
    /// * `Some(RunnerRegistration)` - 如果成功分配
    /// * `None` - 如果已达到最大并发数
    pub fn allocate_runner(&mut self) -> Option<RunnerRegistration> {
        let runner_id = self.id_generator.next()?;
        let (tx, rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        self.control_channels.insert(runner_id, tx);
        Some(RunnerRegistration {
            runner_id,
            control_rx: rx,
        })
    }

    /// 释放一个 runner ID 及其相关资源
    ///
    /// 这会：
    /// - 关闭并移除控制通道
    /// - 从 RunnerNotification 中移除
    /// - 回收 runner ID
    pub fn release_runner(&mut self, runner_id: RunnerId) {
        // 关闭控制通道（发送端 drop 后接收端会收到关闭信号）
        self.control_channels.remove(&runner_id);
        self.runner_notification.remove(runner_id);
        self.id_generator.release(runner_id);
    }

    /// 检查指定 ID 是否已分配
    #[inline]
    pub fn is_runner_allocated(&self, runner_id: RunnerId) -> bool {
        self.id_generator.is_allocated(runner_id)
    }

    /// 获取当前已分配的 runner 数量
    #[inline]
    pub fn allocated_runner_count(&self) -> usize {
        self.id_generator.allocated_count()
    }

    /// 检查是否已达到最大并发数
    #[inline]
    pub fn is_full(&self) -> bool {
        self.id_generator.is_full()
    }

    /// 检查是否没有任何 runner
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.id_generator.is_empty()
    }

    // ==================== 控制通道相关方法 ====================

    /// 向指定 runner 发送控制消息
    ///
    /// # Arguments
    /// * `runner_id` - 目标 runner 的 ID
    /// * `message` - 要发送的消息
    ///
    /// # Returns
    /// * `Ok(())` - 如果成功发送
    /// * `Err(SendError)` - 如果发送失败
    pub fn send_message(
        &self,
        runner_id: RunnerId,
        message: ControlEvent,
    ) -> Result<(), SendError> {
        self.control_channels.send_message(runner_id, message)
    }

    /// 获取指定 runner 的控制通道发送端（克隆）
    ///
    /// 这个方法用于需要持有发送端引用的场景
    pub fn get_control_sender(&self, runner_id: RunnerId) -> Option<ControlSender> {
        self.control_channels.get(&runner_id).cloned()
    }

    // ==================== Runner Notification 相关方法 ====================

    /// 注册一个 runner 的消息消费者
    pub fn register_notification(&mut self, runner_id: RunnerId, consumer: RunnerMessageConsumer) {
        self.runner_notification.add(runner_id, consumer);
    }

    /// 获取 RunnerNotification 的可变引用
    ///
    /// 用于在事件循环中轮询消息
    #[inline]
    pub fn notification_mut(&mut self) -> &mut RunnerNotification {
        &mut self.runner_notification
    }

    /// 关闭 RunnerNotification
    #[inline]
    pub fn close_notification(&mut self) {
        self.runner_notification.close();
    }

    /// 检查 RunnerNotification 是否已关闭
    #[inline]
    pub fn is_notification_closed(&self) -> bool {
        self.runner_notification.is_closed()
    }

    // ==================== Chunk Planner 相关方法 ====================

    /// 获取 ChunkPlanner 的不可变引用
    #[inline]
    pub fn chunk_planner(&self) -> &ChunkPlanner {
        &self.chunk_planner
    }

    /// 获取 ChunkPlanner 的可变引用
    #[inline]
    pub fn chunk_planner_mut(&mut self) -> &mut ChunkPlanner {
        &mut self.chunk_planner
    }

    /// 创建一个 PlannerGuard 用于事务性操作
    #[inline]
    pub fn planner_guard(&mut self) -> PlannerGuard<'_> {
        PlannerGuard::new(&mut self.chunk_planner)
    }

    /// 获取文件总大小
    #[inline]
    pub fn total(&self) -> u64 {
        self.chunk_planner.total
    }

    /// 分配一个 chunk 给指定的 runner
    #[inline]
    pub fn allocate_chunk(&mut self, range: Range<u64>, runner_id: RunnerId) -> bool {
        self.chunk_planner.allocate_chunk(range, Some(runner_id))
    }

    /// 更新 runner 的下载进度
    #[inline]
    pub fn update_progress(
        &mut self,
        runner_id: RunnerId,
        bytes_downloaded: u64,
    ) -> Result<Range<u64>, String> {
        self.chunk_planner
            .update_progress(runner_id, bytes_downloaded)
    }

    /// 标记 runner 已完成
    #[inline]
    pub fn mark_finished(
        &mut self,
        runner_id: RunnerId,
    ) -> Result<(), super::chunk_planner::Error> {
        self.chunk_planner.mark_finished(runner_id)
    }

    /// 标记 runner 已失败
    #[inline]
    pub fn mark_failed(
        &mut self,
        runner_id: RunnerId,
    ) -> Result<Range<u64>, super::chunk_planner::Error> {
        self.chunk_planner.mark_failed(runner_id)
    }

    /// 检查下载是否完成
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.chunk_planner.is_complete()
    }

    /// 获取总下载量
    #[inline]
    pub fn get_total_downloaded(&self) -> u64 {
        self.chunk_planner.get_total_downloaded()
    }

    /// 获取已下载的范围
    #[inline]
    pub fn get_downloaded_ranges(&self) -> Vec<Range<u64>> {
        self.chunk_planner.get_downloaded_ranges()
    }

    /// 获取可用的范围
    #[inline]
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        self.chunk_planner.get_available_ranges()
    }

    /// 获取活跃 runner 数量
    #[inline]
    pub fn get_active_runners_count(&self) -> usize {
        self.chunk_planner.get_active_runners_count()
    }

    /// 获取指定 runner 的状态
    #[inline]
    pub fn get_runner_state(&self, runner_id: RunnerId) -> Option<&ChunkState> {
        self.chunk_planner.get_runner_state(runner_id)
    }

    /// 查找可以拆分的 chunk
    #[inline]
    pub fn find_chunk_to_split(
        &self,
        required_size: u64,
    ) -> Option<(Option<RunnerId>, Range<u64>)> {
        self.chunk_planner.find_chunk_to_split(required_size)
    }

    /// 尝试按长度安排一个 chunk
    #[inline]
    pub fn try_arrange_chunk_by_length(&self, length: u64) -> Option<Range<u64>> {
        self.chunk_planner.try_arrange_chunk_by_length(length)
    }

    /// 获取未完成的状态
    #[inline]
    pub fn get_incomplete_states(
        &self,
        min_chunk_size: Option<u64>,
    ) -> Vec<(RunnerId, Range<u64>)> {
        self.chunk_planner.get_incomplete_states(min_chunk_size)
    }

    /// 拆分 chunk
    #[inline]
    pub fn split_chunk(
        &mut self,
        runner_id: RunnerId,
        split_pos: u64,
        new_runner_id: RunnerId,
    ) -> Result<Range<u64>, super::chunk_planner::Error> {
        self.chunk_planner
            .split_chunk(runner_id, split_pos, new_runner_id)
    }

    /// 调整 runner 状态的大小
    #[inline]
    pub fn resize_runner_state(
        &mut self,
        runner_id: RunnerId,
        new_size: u64,
    ) -> Result<u64, super::chunk_planner::Error> {
        self.chunk_planner.resize_runner_state(runner_id, new_size)
    }

    // ==================== 组合操作 ====================

    /// 分配一个新的 runner 并分配 chunk
    ///
    /// 这是一个便捷方法，组合了 `allocate_runner` 和 `allocate_chunk`
    ///
    /// # Arguments
    /// * `range` - 要分配的 chunk 范围
    ///
    /// # Returns
    /// * `Some((RunnerRegistration, bool))` - runner 注册信息和 chunk 是否成功分配
    /// * `None` - 如果无法分配 runner ID
    pub fn allocate_runner_with_chunk(&mut self, range: Range<u64>) -> Option<RunnerRegistration> {
        let registration = self.allocate_runner()?;
        if !self.allocate_chunk(range, registration.runner_id) {
            self.release_runner(registration.runner_id);
            return None;
        }
        Some(registration)
    }

    /// 完全释放一个 runner（包括 chunk 标记为完成）
    ///
    /// # Arguments
    /// * `runner_id` - 要释放的 runner ID
    /// * `finished` - runner 是否成功完成
    ///
    /// # Returns
    /// * `Ok(Option<Range<u64>>)` - 如果 runner 失败，返回未完成的范围
    /// * `Err` - 如果操作失败
    pub fn release_runner_full(
        &mut self,
        runner_id: RunnerId,
        finished: bool,
    ) -> Result<Option<Range<u64>>, super::chunk_planner::Error> {
        let unfinished_range = if finished {
            self.mark_finished(runner_id)?;
            None
        } else {
            Some(self.mark_failed(runner_id)?)
        };

        self.release_runner(runner_id);
        Ok(unfinished_range)
    }

    #[inline]
    pub fn resize_runner(&mut self, runner_id: RunnerId, new_size: u64) {
        let Self {
            chunk_planner,
            control_channels,
            ..
        } = self;
        let mut guard = PlannerGuard::new(chunk_planner);
        // Limit the total size of the runner
        let downloaded_size = guard
            .planner()
            .resize_runner_state(runner_id, new_size)
            .expect("chunk planner should not fail");

        control_channels
            .send_message(
                runner_id,
                ControlEvent::LimitTotal(downloaded_size + new_size),
            )
            .expect("Manager control channel should never full or closed");

        guard.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_manager_creation() {
        let manager = RunnerManager::new(1000, 4);
        assert_eq!(manager.total(), 1000);
        assert_eq!(manager.allocated_runner_count(), 0);
        assert!(!manager.is_full());
        assert!(manager.is_empty());
    }

    #[test]
    fn test_allocate_and_release_runner() {
        let mut manager = RunnerManager::new(1000, 2);

        // 分配第一个 runner
        let reg1 = manager.allocate_runner().unwrap();
        assert_eq!(reg1.runner_id, 0);
        assert_eq!(manager.allocated_runner_count(), 1);

        // 分配第二个 runner
        let reg2 = manager.allocate_runner().unwrap();
        assert_eq!(reg2.runner_id, 1);
        assert_eq!(manager.allocated_runner_count(), 2);
        assert!(manager.is_full());

        // 无法分配更多
        assert!(manager.allocate_runner().is_none());

        // 释放第一个 runner
        manager.release_runner(reg1.runner_id);
        assert_eq!(manager.allocated_runner_count(), 1);
        assert!(!manager.is_full());

        // 可以再次分配
        let reg3 = manager.allocate_runner().unwrap();
        assert_eq!(reg3.runner_id, 0); // 复用释放的 ID
    }

    #[test]
    fn test_send_message() {
        let mut manager = RunnerManager::new(1000, 2);

        // 分配 runner
        let reg = manager.allocate_runner().unwrap();

        // 发送消息
        let result = manager.send_message(reg.runner_id, ControlEvent::LimitTotal(500));
        assert!(result.is_ok());

        // 接收消息
        let msg = reg.control_rx.try_recv().unwrap();
        assert!(matches!(msg, ControlEvent::LimitTotal(500)));

        // 向不存在的 runner 发送消息
        let result = manager.send_message(999, ControlEvent::LimitTotal(500));
        assert!(matches!(result, Err(SendError::RunnerNotFound(999))));
    }

    #[test]
    fn test_allocate_runner_with_chunk() {
        let mut manager = RunnerManager::new(1000, 2);

        // 分配 runner 和 chunk
        let reg = manager.allocate_runner_with_chunk(0..500).unwrap();

        assert_eq!(reg.runner_id, 0);

        // 验证 chunk 已分配
        let state = manager.get_runner_state(reg.runner_id).unwrap();
        assert_eq!(state.allocated, 0..500);
    }

    #[test]
    fn test_release_runner_full() {
        let mut manager = RunnerManager::new(1000, 2);

        // 分配 runner 和 chunk
        let reg = manager.allocate_runner_with_chunk(0..500).unwrap();

        // 更新进度
        manager.update_progress(reg.runner_id, 200).unwrap();

        // 释放（失败场景）
        let result = manager.release_runner_full(reg.runner_id, false).unwrap();
        assert_eq!(result, Some(200..500));

        // runner 应该已被释放
        assert!(manager.is_empty());
    }
}
