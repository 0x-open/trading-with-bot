use crate::core::exchanges::common::ExchangeAccountId;
use crate::core::exchanges::exchange_blocker::ProgressStatus::ProgressBlocked;
use crate::core::nothing_to_do;
use crate::core::{
    infrastructure::{spawn_future, FutureOutcome},
    lifecycle::cancellation_token::CancellationToken,
};
use futures::{
    future::{join_all, BoxFuture},
    FutureExt,
};
use itertools::Itertools;
use log::{error, trace};
use parking_lot::{Mutex, RwLock};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::iter::FromIterator;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{fmt, iter};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Duration, Instant};

#[cfg(test)]
use mockall::automock;
#[cfg(test)]
use parking_lot::MutexGuard;

const EXPECTED_EAI_SHOULD_BE_CREATED: &str =
    "Should exists because locks created for all exchange accounts in constructor";

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum ExchangeBlockerMoment {
    Blocked,
    BeforeUnblocked,
    Unblocked,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct BlockReason(&'static str);

impl BlockReason {
    pub const fn new(value: &'static str) -> Self {
        BlockReason(value)
    }
}

impl From<&'static str> for BlockReason {
    fn from(value: &'static str) -> Self {
        BlockReason(value)
    }
}

impl Display for BlockReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for BlockReason {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

#[derive(Debug, Copy, Clone)]
pub enum BlockType {
    Manual,
    Timed(Duration),
}

struct TimeoutInProgress {
    end_time: Instant,
    timer_handle: JoinHandle<FutureOutcome>,
}

enum Timeout {
    ReadyUnblock,
    InProgress { in_progress: TimeoutInProgress },
}

impl Timeout {
    fn in_progress(end_time: Instant, timer_handle: JoinHandle<FutureOutcome>) -> Timeout {
        Timeout::InProgress {
            in_progress: TimeoutInProgress {
                end_time,
                timer_handle,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd)]
enum ProgressStatus {
    WaitBlockedMove,
    ProgressBlocked,
    WaitBeforeUnblockedMove,
    WaitUnblockedMove,
}

#[derive(Debug, Clone)]
struct ProgressState {
    is_unblock_requested: bool,
    status: ProgressStatus,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct BlockerId {
    exchange_account_id: ExchangeAccountId,
    reason: BlockReason,
}

impl BlockerId {
    pub fn new(exchange_account_id: ExchangeAccountId, reason: BlockReason) -> Self {
        BlockerId {
            exchange_account_id,
            reason,
        }
    }
}

impl Display for BlockerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.exchange_account_id, self.reason)
    }
}

struct Blocker {
    id: BlockerId,
    timeout: Mutex<Timeout>,
    progress_state: Mutex<ProgressState>,
    unblocked_notify: Arc<Notify>,
}

impl Blocker {
    fn new(id: BlockerId, timeout: Timeout) -> Self {
        Blocker {
            id,
            progress_state: Mutex::new(ProgressState {
                is_unblock_requested: false,
                status: ProgressStatus::WaitBlockedMove,
            }),
            timeout: Mutex::new(timeout),
            unblocked_notify: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct ExchangeBlockerInternalEvent {
    blocker_id: BlockerId,
    event_type: ExchangeBlockerEventType,
}

impl ExchangeBlockerInternalEvent {
    fn with_type(&self, event_type: ExchangeBlockerEventType) -> ExchangeBlockerInternalEvent {
        ExchangeBlockerInternalEvent {
            blocker_id: self.blocker_id.clone(),
            event_type,
        }
    }

    fn pub_event(&self, moment: ExchangeBlockerMoment) -> Arc<ExchangeBlockerEvent> {
        Arc::new(ExchangeBlockerEvent {
            exchange_account_id: self.blocker_id.exchange_account_id.clone(),
            reason: self.blocker_id.reason,
            moment,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum ExchangeBlockerEventType {
    MoveToBlocked,
    UnblockRequested,
    MoveBlockedToBeforeUnblocked,
    MoveBeforeUnblockedToUnblocked,
}

#[derive(Debug, Clone)]
pub struct ExchangeBlockerEvent {
    pub exchange_account_id: ExchangeAccountId,
    pub reason: BlockReason,
    pub moment: ExchangeBlockerMoment,
}

type Blockers = Arc<RwLock<HashMap<ExchangeAccountId, HashMap<BlockReason, Blocker>>>>;
type BlockerEventHandler = Box<
    dyn Fn(Arc<ExchangeBlockerEvent>, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync,
>;
type BlockerEventHandlerVec = Arc<RwLock<Vec<BlockerEventHandler>>>;

#[derive(Clone)]
struct ProcessingCtx {
    blockers: Blockers,
    handlers: BlockerEventHandlerVec,
    events_sender: mpsc::Sender<ExchangeBlockerInternalEvent>,
    cancellation_token: CancellationToken,
}

struct ExchangeBlockerEventsProcessor {
    processing_handle: Mutex<Option<JoinHandle<FutureOutcome>>>,
    handlers: BlockerEventHandlerVec,
    cancellation_token: CancellationToken,
}

impl ExchangeBlockerEventsProcessor {
    fn start(blockers: Blockers) -> (Self, mpsc::Sender<ExchangeBlockerInternalEvent>) {
        let cancellation_token = CancellationToken::new();
        let handlers = BlockerEventHandlerVec::default();

        let (events_sender, events_receiver) = mpsc::channel(20_000);

        let ctx = ProcessingCtx {
            blockers,
            handlers: handlers.clone(),
            events_sender: events_sender.clone(),
            cancellation_token: cancellation_token.clone(),
        };

        let action = async move {
            Self::processing(events_receiver, ctx).await;

            Ok(())
        };
        let processing_handle =
            spawn_future("Start ExchangeBlocker processing", true, action.boxed());

        let events_processor = ExchangeBlockerEventsProcessor {
            processing_handle: Mutex::new(Some(processing_handle)),
            handlers,
            cancellation_token,
        };

        (events_processor, events_sender)
    }

    pub fn register_handler(&self, handler: BlockerEventHandler) {
        self.handlers.write().push(handler);
    }

    fn add_event(
        events_sender: &mut mpsc::Sender<ExchangeBlockerInternalEvent>,
        event: ExchangeBlockerInternalEvent,
    ) {
        if events_sender.is_closed() {
            trace!(
                "Can't send message to ExchangeBlockerEventsProcessor channel because it is closed"
            );
            return;
        }

        match events_sender.try_send(event) {
            Ok(()) => nothing_to_do(),
            Err(err) => {
                // we can't gracefully shutdown because it is part of graceful shutdown system
                panic!("Can't add event in channel in method ExchangeBlockerEventsProcessor::add_event(): {}", err)
            }
        }
    }

    async fn processing(
        mut events_receiver: mpsc::Receiver<ExchangeBlockerInternalEvent>,
        mut ctx: ProcessingCtx,
    ) {
        while !ctx.cancellation_token.is_cancellation_requested() {
            let event = events_receiver.recv().await;
            let event = match event {
                None => {
                    trace!("Finished events processing in ExchangeBlocker because event channel was closed");
                    return;
                }
                Some(event) => event,
            };

            Self::move_next_blocker_state_if_can(&event, &mut ctx);
        }

        events_receiver.close();

        trace!("ExchangeBlocker event processing is cancelled");
    }

    fn move_next_blocker_state_if_can(
        event: &ExchangeBlockerInternalEvent,
        ctx: &mut ProcessingCtx,
    ) {
        use ExchangeBlockerEventType::*;
        use ExchangeBlockerMoment::*;
        use ProgressStatus::*;

        let progress = blocker_progress_apply_fn(&ctx.blockers, &event.blocker_id, |x| x.status);

        match (progress, event.event_type) {
            (WaitBlockedMove, MoveToBlocked) => {
                let mut ctx = ctx.clone();
                let event = event.clone();

                let action = async move {
                    Self::run_handlers(&event, Blocked, &ctx).await;

                    let is_unblock_requested =
                        blocker_progress_apply_fn(&ctx.blockers, &event.blocker_id, |statuses| {
                            let is_unblock_requested = statuses.is_unblock_requested;
                            statuses.status = match is_unblock_requested {
                                true => WaitBeforeUnblockedMove,
                                false => ProgressBlocked,
                            };
                            is_unblock_requested
                        });

                    if is_unblock_requested {
                        let event = event.with_type(MoveBlockedToBeforeUnblocked);
                        Self::add_event(&mut ctx.events_sender, event)
                    }

                    Ok(())
                };
                let _ = spawn_future("Run ExchangeBlocker handlers", true, action.boxed());
            }
            (ProgressBlocked, UnblockRequested) => {
                blocker_progress_apply_fn(&ctx.blockers, &event.blocker_id, |statuses| {
                    statuses.status = WaitBeforeUnblockedMove
                });

                let event = event.with_type(MoveBlockedToBeforeUnblocked);
                Self::add_event(&mut ctx.events_sender, event)
            }
            (WaitBeforeUnblockedMove, MoveBlockedToBeforeUnblocked) => {
                let mut ctx = ctx.clone();
                let event = event.clone();
                let action = async move {
                    Self::run_handlers(&event, BeforeUnblocked, &ctx).await;

                    blocker_progress_apply_fn(&ctx.blockers, &event.blocker_id, |x| {
                        x.status = WaitUnblockedMove
                    });

                    let event = event.with_type(MoveBeforeUnblockedToUnblocked);
                    Self::add_event(&mut ctx.events_sender, event);

                    Ok(())
                };
                let _ = spawn_future(
                    "Run ExchangeBlocker handlers in case WaitBeforeUnblockedMove",
                    true,
                    action.boxed(),
                );
            }
            (WaitUnblockedMove, MoveBeforeUnblockedToUnblocked) => {
                Self::remove_blocker(event, &ctx);

                let ctx = ctx.clone();
                let event = event.clone();

                let action = async move {
                    Self::run_handlers(&event, Unblocked, &ctx).await;
                    Ok(())
                };
                let _ = spawn_future(
                    "Run ExchangeBlocker handlers in case WaitUnblockedMove",
                    true,
                    action.boxed(),
                );
            }
            _ => nothing_to_do(),
        };
    }

    async fn run_handlers(
        event: &ExchangeBlockerInternalEvent,
        moment: ExchangeBlockerMoment,
        ctx: &ProcessingCtx,
    ) {
        let pub_event = event.pub_event(moment);
        let repeat_iter = iter::repeat((pub_event.clone(), ctx.cancellation_token.clone()));
        let handlers_futures = ctx
            .handlers
            .read()
            .iter()
            .zip(repeat_iter)
            .map(|(handler, (e, ct))| handler(e, ct))
            .collect_vec();

        join_all(handlers_futures).await;
    }

    fn remove_blocker(event: &ExchangeBlockerInternalEvent, ctx: &ProcessingCtx) {
        let mut locks_write = ctx.blockers.write();
        let blockers = locks_write
            .get_mut(&event.blocker_id.exchange_account_id)
            .expect(EXPECTED_EAI_SHOULD_BE_CREATED);

        blockers
            .get(&event.blocker_id.reason)
            .map(|blocker| blocker.unblocked_notify.notify_waiters());

        let removed_blocker = blockers.remove_entry(&event.blocker_id.reason);

        match removed_blocker {
            None => {
                error!(
                    "Can't find blocker {} {} in method ExchangeBlockerEventsProcessor::remove_blocker()",
                    event.blocker_id.exchange_account_id, event.blocker_id.reason);
            }
            Some(_) => {
                trace!(
                    "Successfully unblocked {} {} in ExchangeBlocker",
                    event.blocker_id.exchange_account_id,
                    event.blocker_id.reason
                );
            }
        }
    }

    async fn stop_processing(&self) {
        self.cancellation_token.cancel();
        tokio::task::yield_now().await;

        let processing_handle = match self.processing_handle.lock().take() {
            None => {
                trace!("ExchangeBlocker::stop_processing() called more then 1 time");
                return;
            }
            Some(rx) => rx,
        };

        trace!("ExchangeBlocker::stop_processing waiting for completion of processing");
        processing_handle.abort();
        let res = processing_handle.await;
        if let Err(join_err) = res {
            if join_err.is_panic() {
                error!(
                    "We get panic in ExchangeBlockerEventsProcessor::processing(): {}",
                    join_err
                )
            }
        }
    }
}

fn blocker_progress_apply_fn<F: FnMut(&mut ProgressState) -> R, R: 'static>(
    blockers: &Blockers,
    blocker_id: &BlockerId,
    mut f: F,
) -> R {
    let read_guard = blockers.read();
    let mut lock_guard = read_guard
        .get(&blocker_id.exchange_account_id)
        .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
        .get(&blocker_id.reason)
        .expect("Blocker should be added in method ExchangeBlocker::block()")
        .progress_state
        .lock();
    let progress_state = lock_guard.deref_mut();

    f(progress_state)
}

pub struct ExchangeBlocker {
    blockers: Blockers,
    events_processor: ExchangeBlockerEventsProcessor,
    events_sender: Mutex<mpsc::Sender<ExchangeBlockerInternalEvent>>,
}

#[cfg_attr(test, automock)]
impl ExchangeBlocker {
    pub fn new(exchange_account_ids: Vec<ExchangeAccountId>) -> Arc<Self> {
        let blockers = Arc::new(RwLock::new(HashMap::from_iter(
            exchange_account_ids
                .iter()
                .map(|x| (x.clone(), HashMap::new()))
                .into_iter(),
        )));

        let (events_processor, events_sender) =
            ExchangeBlockerEventsProcessor::start(blockers.clone());

        Arc::new(ExchangeBlocker {
            blockers,
            events_processor,
            events_sender: Mutex::new(events_sender),
        })
    }

    pub fn is_blocked(&self, exchange_account_id: &ExchangeAccountId) -> bool {
        !self
            .blockers
            .read()
            .get(exchange_account_id)
            .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
            .is_empty()
    }

    pub fn is_blocked_by_reason(
        &self,
        exchange_account_id: &ExchangeAccountId,
        reason: BlockReason,
    ) -> bool {
        self.blockers
            .read()
            .get(exchange_account_id)
            .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
            .get(&reason)
            .is_some()
    }

    pub fn is_blocked_except_reason(
        &self,
        exchange_account_id: &ExchangeAccountId,
        reason: BlockReason,
    ) -> bool {
        let read_blockers_guard = self.blockers.read();
        let blockers = read_blockers_guard
            .get(exchange_account_id)
            .expect(EXPECTED_EAI_SHOULD_BE_CREATED);
        let is_blocker_exists = blockers.get(&reason).is_some();
        let blockers_count = blockers.len();

        is_blocker_exists && blockers_count > 1 || !is_blocker_exists && blockers_count > 0
    }

    pub fn block(
        self: &Arc<Self>,
        exchange_account_id: &ExchangeAccountId,
        reason: BlockReason,
        block_type: BlockType,
    ) {
        trace!(
            "ExchangeBlocker::block() started {} {}",
            exchange_account_id,
            reason
        );

        match self
            .blockers
            .write()
            .get_mut(&exchange_account_id)
            .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
            .entry(reason)
        {
            Entry::Occupied(entry) => self.timeout_reset_if_exists(entry.get(), block_type),
            Entry::Vacant(vacant_entry) => {
                let blocker_id = BlockerId::new(exchange_account_id.clone(), reason);
                let blocker = self.create_blocker(block_type, blocker_id.clone());
                vacant_entry.insert(blocker);
                let event = ExchangeBlockerInternalEvent {
                    blocker_id,
                    event_type: ExchangeBlockerEventType::MoveToBlocked,
                };
                ExchangeBlockerEventsProcessor::add_event(
                    self.events_sender.lock().deref_mut(),
                    event,
                );
            }
        }

        trace!(
            "ExchangeBlocker::block() finished {} {}",
            exchange_account_id,
            reason
        );
    }

    fn timeout_reset_if_exists(self: &Arc<Self>, blocker: &Blocker, block_type: BlockType) {
        fn rollback_to_blocked_progress(blocker: &Blocker) {
            let mut progress_guard = blocker.progress_state.lock();
            let progress_status = progress_guard.status;
            *progress_guard = ProgressState {
                is_unblock_requested: false,
                status: match progress_status >= ProgressBlocked {
                    false => progress_status,
                    true => ProgressBlocked,
                },
            };
        }

        match block_type {
            BlockType::Timed(duration) => {
                let expected_end_time = Instant::now() + duration;

                let timeout = &mut *blocker.timeout.lock();
                match timeout {
                    Timeout::InProgress { in_progress } => {
                        if expected_end_time < in_progress.end_time {
                            return;
                        }

                        in_progress.timer_handle.abort();
                    }
                    Timeout::ReadyUnblock => nothing_to_do(),
                }

                rollback_to_blocked_progress(blocker);

                *timeout = Timeout::in_progress(
                    expected_end_time,
                    self.set_unblock_by_timer(blocker.id.clone(), expected_end_time),
                );
            }
            BlockType::Manual => match &mut *blocker.timeout.lock() {
                Timeout::ReadyUnblock => rollback_to_blocked_progress(blocker),
                Timeout::InProgress { .. } => error!("Can't block exchange by reason untimely until timed blocking by reason will be unblocked")
            },
        }
    }

    fn create_blocker(self: &Arc<Self>, block_type: BlockType, blocker_id: BlockerId) -> Blocker {
        let timeout = match block_type {
            BlockType::Manual => Timeout::ReadyUnblock,
            BlockType::Timed(duration) => self.timeout_init(&blocker_id, duration),
        };
        Blocker::new(blocker_id, timeout)
    }

    fn timeout_init(self: &Arc<Self>, blocker_id: &BlockerId, duration: Duration) -> Timeout {
        let instant = Instant::now();
        let expected_end_time = instant + duration;

        Timeout::in_progress(
            expected_end_time,
            self.set_unblock_by_timer(blocker_id.clone(), expected_end_time),
        )
    }

    fn set_unblock_by_timer(
        self: &Arc<Self>,
        blocker_id: BlockerId,
        end_time: Instant,
    ) -> JoinHandle<FutureOutcome> {
        let self_wk = Arc::downgrade(&self.clone());
        let action = async move {
            sleep_until(end_time).await;

            match self_wk.upgrade() {
                None => trace!(
                    "Can't upgrade exchange blocker reference in unblock timer of ExchangeBlocker for blocker '{}'", &blocker_id
                ),
                Some(self_rc) => {
                    let exchange_account_id = &blocker_id.exchange_account_id;
                    let reason = blocker_id.reason;
                    match self_rc
                        .blockers
                        .read()
                        .get(exchange_account_id)
                        .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
                        .get(&reason)
                    {
                        None => {
                            error!("Not found blocker '{}' on timer tick. If unblock forced, timer should be stopped manually.", &blocker_id)
                        }
                        Some(blocker) => *blocker.timeout.lock() = Timeout::ReadyUnblock,
                    }
                    self_rc.unblock(exchange_account_id, reason)
                }
            }

            Ok(())
        };
        spawn_future("Run ExchangeBlocker handlers", true, action.boxed())
    }

    pub fn unblock(&self, exchange_account_id: &ExchangeAccountId, reason: BlockReason) {
        trace!("Unblock started {} {}", exchange_account_id, reason);

        let blocker_id = BlockerId::new(exchange_account_id.clone(), reason);

        {
            let read_guard = self.blockers.read();
            let blocker = match read_guard
                .get(&blocker_id.exchange_account_id)
                .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
                .get(&blocker_id.reason)
            {
                Some(blocker) => blocker,
                None => return,
            };

            let mut lock_guard = blocker.progress_state.lock();
            let progress_state = lock_guard.deref_mut();
            progress_state.is_unblock_requested = true;
        }

        let event = ExchangeBlockerInternalEvent {
            blocker_id,
            event_type: ExchangeBlockerEventType::UnblockRequested,
        };
        ExchangeBlockerEventsProcessor::add_event(self.events_sender.lock().deref_mut(), event);

        trace!("Unblock finished {} {}", exchange_account_id, reason);
    }

    pub async fn wait_unblock(
        &self,
        exchange_account_id: ExchangeAccountId,
        cancellation_token: CancellationToken,
    ) {
        trace!(
            "ExchangeBlocker::wait_unblock() started {}",
            exchange_account_id
        );

        loop {
            let unblocked_notifies = self
                .blockers
                .read()
                .get(&exchange_account_id)
                .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
                .values()
                .map(|blocker| blocker.unblocked_notify.clone())
                .collect_vec();

            if unblocked_notifies.is_empty() {
                return;
            }

            let unblocked_futures = join_all(unblocked_notifies.iter().map(|x| x.notified()));

            tokio::select! {
                _ = unblocked_futures => nothing_to_do(),
                _ = cancellation_token.when_cancelled() => return (),
            }

            // we can reblock some reasons while waiting others
            if !self.is_blocked(&exchange_account_id) {
                break;
            }
        }

        trace!(
            "ExchangeBlocker::wait_unblock() finished {}",
            exchange_account_id
        );
    }

    pub async fn wait_unblock_with_reason(
        &self,
        exchange_account_id: ExchangeAccountId,
        reason: BlockReason,
        cancellation_token: CancellationToken,
    ) {
        trace!(
            "ExchangeBlocker::wait_unblock_with_reason started {} {}",
            exchange_account_id,
            reason
        );

        let unblocked_notify = {
            let read_locks = self.blockers.read();
            let blocker = read_locks
                .get(&exchange_account_id)
                .expect(EXPECTED_EAI_SHOULD_BE_CREATED)
                .get(&reason);
            if let Some(blocker) = blocker {
                blocker.unblocked_notify.clone()
            } else {
                return;
            }
        };

        tokio::select! {
            _ = unblocked_notify.notified() => nothing_to_do(),
            _ = cancellation_token.when_cancelled() => return (),
        }

        trace!(
            "ExchangeBlocker::wait_unblock_with_reason finished {} {}",
            exchange_account_id,
            reason
        );
    }

    pub fn register_handler(&self, handler: BlockerEventHandler) {
        self.events_processor.register_handler(handler)
    }

    pub async fn stop_blocker(&self) {
        trace!("ExchangeBlocker::stop_blocker() started");
        self.events_processor.stop_processing().await;
    }
}

crate::impl_mock_initializer!(MockExchangeBlocker, EXCHANGE_BLOCKER_MOCK_LOCKER);

#[cfg(test)]
mod tests {
    use crate::core::exchanges::common::ExchangeAccountId;
    use crate::core::exchanges::exchange_blocker::BlockType::*;
    use crate::core::exchanges::exchange_blocker::{
        BlockReason, ExchangeBlocker, ExchangeBlockerMoment,
    };
    use crate::core::nothing_to_do;
    use crate::core::{
        infrastructure::spawn_future, lifecycle::cancellation_token::CancellationToken,
    };
    use futures::future::{join, join_all};
    use futures::FutureExt;
    use parking_lot::Mutex;
    use rand::Rng;
    use std::iter::repeat_with;
    use std::ops::DerefMut;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::{oneshot, Notify};
    use tokio::time::{sleep, Duration};

    type Signal<T> = Arc<Mutex<T>>;

    fn exchange_account_id() -> ExchangeAccountId {
        // TODO Make const way to create ExchangeAccountId
        //"ExchangeId0".parse().expect("test")
        ExchangeAccountId::new("ExchangeId".into(), 0)
    }

    fn exchange_blocker() -> Arc<ExchangeBlocker> {
        let exchange_account_ids = vec![exchange_account_id()];
        ExchangeBlocker::new(exchange_account_ids)
    }

    #[tokio::test]
    async fn block_unblock_manual() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = exchange_blocker();

        let reason = "test_reason".into();

        exchange_blocker.block(&exchange_account_id(), reason, Manual);
        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), true);

        exchange_blocker.unblock(&exchange_account_id(), reason);
        exchange_blocker
            .wait_unblock(exchange_account_id(), cancellation_token)
            .await;
        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), false);
    }

    #[tokio::test]
    async fn block_unblock_future() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = exchange_blocker();
        let signal = Signal::default();

        let reason = "test_reason".into();

        exchange_blocker.block(&exchange_account_id(), reason, Manual);
        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), true);

        let _ = spawn_future(
            "Run ExchangeBlocker::wait_unblock in block_unblock_future test",
            false,
            {
                let exchange_blocker = exchange_blocker.clone();
                let signal = signal.clone();
                let cancellation_token = cancellation_token.clone();
                async move {
                    exchange_blocker
                        .wait_unblock(exchange_account_id(), cancellation_token)
                        .await;

                    *signal.lock() = true;

                    Ok(())
                }
                .boxed()
            },
        );

        tokio::task::yield_now().await;
        assert_eq!(*signal.lock(), false);

        exchange_blocker.unblock(&exchange_account_id(), reason);
        exchange_blocker
            .wait_unblock(exchange_account_id(), cancellation_token)
            .await;
        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), false);

        tokio::task::yield_now().await;
        assert_eq!(*signal.lock(), true);
    }

    #[tokio::test]
    async fn block_duration() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = exchange_blocker();

        let reason = "timer_test_reason".into();
        let duration = Duration::from_millis(50);

        let timer = Instant::now();

        let action = async move {
            exchange_blocker.block(&exchange_account_id(), reason, Timed(duration));
            assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), true);
            exchange_blocker
                .wait_unblock(exchange_account_id(), cancellation_token)
                .await;

            Ok(())
        };
        let handle = spawn_future(
            "Run ExchangeBlocker::wait_unblock in block_duration test",
            false,
            action.boxed(),
        );

        let timeout_limit = duration + Duration::from_millis(30);
        tokio::select! {
            _ = handle => {
                let elapsed = timer.elapsed();
                assert!(elapsed > duration, "Exchange should be unblocked after {} ms, but was {} ms", duration.as_millis(), elapsed.as_millis())
            },
            _ = sleep(timeout_limit) => panic!("Timeout limit ({} ms) exceeded", timeout_limit.as_millis()),
        }
    }

    #[tokio::test]
    async fn reblock_before_time_is_up() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = exchange_blocker();

        let reason = "timer_test_reason".into();
        let duration = Duration::from_millis(50);
        let duration_sleep = Duration::from_millis(20);

        let timer = Instant::now();

        let action = async move {
            exchange_blocker.block(&exchange_account_id(), reason, Timed(duration));
            assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), true);

            sleep(duration_sleep).await;

            exchange_blocker.block(&exchange_account_id(), reason, Timed(duration));
            assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), true);

            exchange_blocker
                .wait_unblock(exchange_account_id(), cancellation_token)
                .await;

            Ok(())
        };
        let handle = spawn_future(
            "Run ExchangeBlocker::wait_unblock in reblock_before_time_is_up test",
            false,
            action.boxed(),
        );

        let min_timeout = duration_sleep + duration;
        let timeout_limit = min_timeout + Duration::from_millis(30);
        tokio::select! {
            _ = handle => {
                let elapsed = timer.elapsed();
                assert!(elapsed > min_timeout, "Exchange should be unblocked after {} ms, but was {} ms", min_timeout.as_millis(), elapsed.as_millis())
            },
            _ = sleep(timeout_limit) => panic!("Timeout limit ({} ms) exceeded", timeout_limit.as_millis()),
        }
    }

    #[tokio::test]
    async fn block_with_multiple() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = &exchange_blocker();

        let reason1 = "reason1".into();
        let reason2 = "reason2".into();

        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), false);

        exchange_blocker.block(&exchange_account_id(), reason1, Manual);
        assert_blocking_state(exchange_blocker, reason1, reason2, true, false, true);

        exchange_blocker.block(&exchange_account_id(), reason2, Manual);
        assert_blocking_state(exchange_blocker, reason1, reason2, true, true, true);

        exchange_blocker.unblock(&exchange_account_id(), reason1);
        exchange_blocker
            .wait_unblock_with_reason(exchange_account_id(), reason1, cancellation_token.clone())
            .await;
        assert_blocking_state(exchange_blocker, reason1, reason2, false, true, true);

        exchange_blocker.unblock(&exchange_account_id(), reason2);
        exchange_blocker
            .wait_unblock(exchange_account_id(), cancellation_token)
            .await;
        assert_blocking_state(exchange_blocker, reason1, reason2, false, false, false);
    }

    fn assert_blocking_state(
        exchange_blocker: &Arc<ExchangeBlocker>,
        reason1: BlockReason,
        reason2: BlockReason,
        expected_is_blocked_by_reason1: bool,
        expected_is_blocked_by_reason2: bool,
        expected_is_exchange_blocked: bool,
    ) {
        let is_blocked1 = exchange_blocker.is_blocked_by_reason(&exchange_account_id(), reason1);
        assert_eq!(is_blocked1, expected_is_blocked_by_reason1);
        let is_blocked2 = exchange_blocker.is_blocked_by_reason(&exchange_account_id(), reason2);
        assert_eq!(is_blocked2, expected_is_blocked_by_reason2);
        let is_exchange_blocked = exchange_blocker.is_blocked(&exchange_account_id());
        assert_eq!(is_exchange_blocked, expected_is_exchange_blocked);
    }

    #[tokio::test]
    async fn block_with_handler() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = exchange_blocker();
        let times_count = &Signal::<u8>::default();

        exchange_blocker.register_handler({
            let times_count = times_count.clone();
            Box::new(move |event, _| {
                let times_count = times_count.clone();
                async move {
                    if event.moment == ExchangeBlockerMoment::Blocked
                        && event.exchange_account_id == exchange_account_id()
                    {
                        *times_count.lock() += 1;
                    }
                }
                .boxed()
            })
        });

        let reason = "reason".into();

        exchange_blocker.block(&exchange_account_id(), reason, Manual);
        exchange_blocker.unblock(&exchange_account_id(), reason);
        exchange_blocker
            .wait_unblock(exchange_account_id(), cancellation_token)
            .await;

        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), false);
        assert_eq!(*times_count.lock(), 1);
    }

    #[tokio::test]
    async fn block_with_first_long_handler() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = exchange_blocker();
        let times_count = &Signal::<u8>::default();

        exchange_blocker.register_handler({
            let times_count = times_count.clone();
            Box::new(move |event, _| {
                let times_count = times_count.clone();
                async move {
                    match event.moment {
                        ExchangeBlockerMoment::Blocked => {
                            sleep(Duration::from_millis(40)).await;
                            *times_count.lock() += 1;
                        }
                        ExchangeBlockerMoment::BeforeUnblocked => *times_count.lock() += 1,
                        _ => nothing_to_do(),
                    }
                }
                .boxed()
            })
        });

        let reason = "reason".into();

        exchange_blocker.block(&exchange_account_id(), reason, Manual);
        exchange_blocker.unblock(&exchange_account_id(), reason);
        exchange_blocker
            .wait_unblock(exchange_account_id(), cancellation_token)
            .await;

        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), false);
        assert_eq!(*times_count.lock(), 2);
    }

    #[tokio::test]
    async fn stop_blocker() {
        let exchange_blocker = exchange_blocker();

        let max_timeout = Duration::from_millis(100);
        tokio::select! {
            _ = exchange_blocker.stop_blocker() => nothing_to_do(),
            _ = sleep(max_timeout) => panic!("Timeout was exceeded ({} ms)", max_timeout.as_millis()),
        }
    }

    #[tokio::test]
    async fn block_with_handler_after_stop() {
        let exchange_blocker = exchange_blocker();
        let times_count = &Signal::<u8>::default();

        exchange_blocker.register_handler({
            let times_count = times_count.clone();
            Box::new(move |event, _| {
                let times_count = times_count.clone();
                async move {
                    if event.moment == ExchangeBlockerMoment::Blocked
                        && event.exchange_account_id == exchange_account_id()
                    {
                        *times_count.lock() += 1;
                    }
                }
                .boxed()
            })
        });

        exchange_blocker.stop_blocker().await;

        let reason = "reason".into();
        exchange_blocker.block(&exchange_account_id(), reason, Manual);
        exchange_blocker.unblock(&exchange_account_id(), reason);
        sleep(Duration::from_millis(1)).await;

        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), true);

        // should ignore all events
        assert_eq!(*times_count.lock(), 0);
    }

    #[tokio::test]
    async fn block_many_times() {
        async fn do_action(index: u32, exchange_blocker: Arc<ExchangeBlocker>) {
            let reason = gen_reason(index);

            exchange_blocker.block(&exchange_account_id(), reason, Manual);
            tokio::task::yield_now().await;
            exchange_blocker.unblock(&exchange_account_id(), reason);
            exchange_blocker
                .wait_unblock_with_reason(
                    exchange_account_id().clone(),
                    reason,
                    CancellationToken::new(),
                )
                .await;
        }

        let cancellation_token = CancellationToken::new();
        let exchange_blocker = &exchange_blocker();
        let times_count = &Signal::<u32>::default();

        exchange_blocker.register_handler({
            let times_count = times_count.clone();
            Box::new(move |event, _| {
                let times_count = times_count.clone();
                async move {
                    if event.moment == ExchangeBlockerMoment::Blocked
                        && event.exchange_account_id == exchange_account_id()
                    {
                        *times_count.lock().deref_mut() += 1;
                    }
                }
                .boxed()
            })
        });

        const TIMES_COUNT: u32 = 200;
        const REASONS_COUNT: u32 = 20;
        for _ in 0..(TIMES_COUNT / REASONS_COUNT) {
            let jobs = (0..REASONS_COUNT)
                .zip(repeat_with(|| exchange_blocker.clone()))
                .map(|(i, b)| {
                    let action = async move {
                        do_action(i, b).await;
                        Ok(())
                    };
                    spawn_future("do_action in block_many_times test", false, action.boxed())
                });
            join_all(jobs).await;
        }

        let max_timeout = Duration::from_secs(2);
        tokio::select! {
            _ = exchange_blocker.wait_unblock(exchange_account_id(), cancellation_token) => {
                assert_eq!(*times_count.lock(), TIMES_COUNT);
            },
            _ = sleep(max_timeout) => {
                print_blocked_reasons(exchange_blocker, REASONS_COUNT);
                panic!("Timeout was exceeded ({} ms)", max_timeout.as_millis());
            }
        }
    }

    #[tokio::test]
    async fn block_many_times_with_random_reasons() {
        async fn do_action(index: u32, exchange_blocker: Arc<ExchangeBlocker>) {
            let reason = gen_reason(index);

            exchange_blocker.block(&exchange_account_id(), reason, Manual);
            tokio::task::yield_now().await;
            exchange_blocker.unblock(&exchange_account_id(), reason);
        }

        let mut rng = rand::thread_rng();
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = &exchange_blocker();
        let times_count = Signal::<usize>::default();

        exchange_blocker.register_handler({
            let times_count = times_count.clone();
            Box::new(move |event, _| {
                let times_count = times_count.clone();
                async move {
                    if event.moment == ExchangeBlockerMoment::Blocked
                        && event.exchange_account_id == exchange_account_id()
                    {
                        *times_count.lock() += 1;
                    }
                }
                .boxed()
            })
        });

        const TIMES_COUNT: usize = 200;
        let jobs = repeat_with(|| rng.gen_range(0..10u32))
            .take(TIMES_COUNT)
            .zip(repeat_with(|| exchange_blocker.clone()))
            .map(|(i, b)| {
                let action = async move {
                    do_action(i, b).await;

                    Ok(())
                };
                spawn_future("do_action in block_many_times test", false, action.boxed())
            });
        join_all(jobs).await;

        // exchange blocker should be successfully unblocked
        let max_timeout = Duration::from_secs(2);
        tokio::select! {
            _ = exchange_blocker.wait_unblock(exchange_account_id(), cancellation_token) => nothing_to_do(),
            _ = sleep(max_timeout) => panic!("Timeout was exceeded ({} ms)", max_timeout.as_millis()),
        }
    }

    #[tokio::test]
    async fn block_many_times_with_stop_exchange_blocker() {
        async fn do_action(index: u32, exchange_blocker: Arc<ExchangeBlocker>) {
            let reason = gen_reason(index);

            exchange_blocker.block(&exchange_account_id(), reason, Manual);
            tokio::task::yield_now().await;
            exchange_blocker.unblock(&exchange_account_id(), reason);
        }

        let exchange_blocker = &exchange_blocker();
        let (blocker_stop_started_tx, blocker_stop_started_rx) = oneshot::channel();
        let (blocker_stopped_tx, blocker_stopped_rx) = oneshot::channel();
        let spawn_actions_notify = Arc::new(Notify::new());

        {
            let exchange_blocker = exchange_blocker.clone();
            let action = async move {
                sleep(Duration::from_millis(1)).await;
                let _ = blocker_stop_started_tx.send(());
                exchange_blocker.stop_blocker().await;
                let _ = blocker_stopped_tx.send(());

                Ok(())
            };
            let _ = spawn_future("do_action in block_many_times test", false, action.boxed());
        }

        {
            let exchange_blocker = exchange_blocker.clone();
            let spawn_actions_notify = spawn_actions_notify.clone();
            let action = async move {
                const TIMES_COUNT: u32 = 1000;
                const REASONS_COUNT: u32 = 10;
                for i in 0..TIMES_COUNT {
                    let exchange_blocker = exchange_blocker.clone();
                    let _ = spawn_future(
                        "do_action in block_many_times_with_stop_exchange_blocker test",
                        false,
                        async move {
                            do_action(i % REASONS_COUNT, exchange_blocker.clone()).await;
                            Ok(())
                        }
                        .boxed(),
                    );
                    if i % REASONS_COUNT == 0 {
                        tokio::task::yield_now().await;
                    }
                }

                spawn_actions_notify.notify_waiters();

                Ok(())
            };
            let _ = spawn_future(
                "spawn_actions_notify in block_many_times_with_stop_exchange_blocker test",
                false,
                action.boxed(),
            );
        };

        {
            let spawn_actions_notify = spawn_actions_notify.clone();
            let action = async move {
                tokio::select! {
                    _ = spawn_actions_notify.notified() => panic!("spawn_actions finished before exchange blocker_block() started. It does not meet test case."),
                    _ = blocker_stop_started_rx => nothing_to_do(),
                }

                Ok(())
            };
            let _ = spawn_future(
                "start checking when spawn_actions finished",
                false,
                action.boxed(),
            );
        }

        let max_timeout = Duration::from_secs(2);
        tokio::select! {
            _ = join(spawn_actions_notify.notified(), blocker_stopped_rx) => nothing_to_do(),
            _ = sleep(max_timeout) => panic!("Timeout was exceeded ({} ms)", max_timeout.as_millis()),
        }
    }

    #[tokio::test]
    async fn is_blocked_except_reason_full_cycle() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = &exchange_blocker();

        let reason1 = "reason1".into();
        let reason2 = "reason2".into();

        // no blocked
        assert_is_blocking_except_reason(exchange_blocker, reason1, reason2, false, false);

        exchange_blocker.block(&exchange_account_id(), reason2, Manual);
        // blocked with reason2
        assert_is_blocking_except_reason(exchange_blocker, reason1, reason2, true, false);

        exchange_blocker.block(&exchange_account_id(), reason2, Manual);
        // blocked with reason2 again
        assert_is_blocking_except_reason(exchange_blocker, reason1, reason2, true, false);

        exchange_blocker.block(&exchange_account_id(), reason1, Manual);
        // blocked with reason1 & reason2
        assert_is_blocking_except_reason(exchange_blocker, reason1, reason2, true, true);

        exchange_blocker.unblock(&exchange_account_id(), reason2);
        exchange_blocker
            .wait_unblock_with_reason(exchange_account_id(), reason2, cancellation_token.clone())
            .await;
        // blocked with reason 1
        assert_is_blocking_except_reason(exchange_blocker, reason1, reason2, false, true);

        exchange_blocker.unblock(&exchange_account_id(), reason1);
        exchange_blocker
            .wait_unblock_with_reason(exchange_account_id(), reason1, cancellation_token)
            .await;
        // no blocked
        assert_is_blocking_except_reason(exchange_blocker, reason1, reason2, false, false);
    }

    #[tokio::test]
    async fn wait_unblock_if_not_blocked() {
        let cancellation_token = CancellationToken::new();
        let exchange_blocker = &exchange_blocker();

        // no blocked
        assert_eq!(exchange_blocker.is_blocked(&exchange_account_id()), false);

        exchange_blocker
            .wait_unblock(exchange_account_id(), cancellation_token)
            .await;
    }

    #[tokio::test]
    async fn wait_unblock_when_reblock_1_of_2_reasons() {
        let exchange_blocker = &exchange_blocker();
        let wait_completed = Signal::<bool>::default();

        let reason1 = "reason1".into();
        let reason2 = "reason2".into();

        exchange_blocker.block(&exchange_account_id(), reason1, Manual);
        exchange_blocker.block(&exchange_account_id(), reason2, Manual);

        let _ = spawn_future(
            "Run wait_unblock in wait_unblock_when_reblock_1_of_2_reasons test",
            true,
            {
                let exchange_blocker = exchange_blocker.clone();
                let wait_completed = wait_completed.clone();
                async move {
                    exchange_blocker
                        .wait_unblock(exchange_account_id(), CancellationToken::new())
                        .await;
                    *wait_completed.lock() = true;
                    Ok(())
                }
            }
            .boxed(),
        );

        tokio::task::yield_now().await;
        assert_eq!(*wait_completed.lock(), false);

        // reblock reason1
        exchange_blocker.unblock(&exchange_account_id(), reason1);
        exchange_blocker
            .wait_unblock_with_reason(exchange_account_id(), reason1, CancellationToken::new())
            .await;
        exchange_blocker.block(&exchange_account_id(), reason1, Manual);

        exchange_blocker.unblock(&exchange_account_id(), reason2);

        exchange_blocker
            .wait_unblock_with_reason(exchange_account_id(), reason2, CancellationToken::new())
            .await;
        assert_eq!(*wait_completed.lock(), false);

        exchange_blocker.unblock(&exchange_account_id(), reason1);
        exchange_blocker
            .wait_unblock(exchange_account_id(), CancellationToken::new())
            .await;
        assert_eq!(*wait_completed.lock(), true);
    }

    fn assert_is_blocking_except_reason(
        exchange_blocker: &Arc<ExchangeBlocker>,
        reason1: BlockReason,
        reason2: BlockReason,
        expected_is_blocked_by_reason1: bool,
        expected_is_blocked_by_reason2: bool,
    ) {
        assert_eq!(
            exchange_blocker.is_blocked_except_reason(&exchange_account_id(), reason1),
            expected_is_blocked_by_reason1
        );
        assert_eq!(
            exchange_blocker.is_blocked_except_reason(&exchange_account_id(), reason2),
            expected_is_blocked_by_reason2
        );
    }

    fn gen_reason(index: u32) -> BlockReason {
        // Memory leak just in tests for simple creation different reasons. In production code it should be static string
        (&*Box::leak(format!("reason{}", index).into_boxed_str())).into()
    }

    fn print_blocked_reasons(exchange_blocker: &Arc<ExchangeBlocker>, reasons_count: u32) {
        for i in 0..reasons_count {
            let reason = gen_reason(i);
            println!(
                "reason{} is blocked: {}",
                i,
                exchange_blocker.is_blocked_by_reason(&exchange_account_id(), reason),
            )
        }
    }
}
