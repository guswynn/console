use crate::{AttributeUpdate, WatchRequest};

use super::{
    AttributeUpdateOp, AttributeUpdateValue, Event, OpType, Readiness, WakeOp, Watch, WatchKind,
};
use console_api as proto;
use tokio::sync::{mpsc, Notify};

use futures::FutureExt;
use std::{
    collections::HashMap,
    convert::TryInto,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicBool, Ordering::*},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tracing_core::{span, Metadata};

use hdrhistogram::{
    serialization::{Serializer, V2SerializeError, V2Serializer},
    Histogram,
};

pub(crate) struct Aggregator {
    /// Channel of incoming events emitted by `TaskLayer`s.
    events: mpsc::Receiver<Event>,

    /// New incoming RPCs.
    rpcs: mpsc::Receiver<WatchKind>,

    /// The interval at which new data updates are pushed to clients.
    publish_interval: Duration,

    /// How long to keep task data after a task has completed.
    retention: Duration,

    /// Triggers a flush when the event buffer is approaching capacity.
    flush_capacity: Arc<Flush>,

    /// Currently active RPCs streaming task events.
    watchers: Vec<Watch<proto::instrument::InstrumentUpdate>>,

    /// Currently active RPCs streaming task details events, by task ID.
    details_watchers: HashMap<span::Id, Vec<Watch<proto::tasks::TaskDetails>>>,

    /// *All* metadata for task spans and user-defined spans that we care about.
    ///
    /// This is sent to new clients as part of the initial state.
    all_metadata: Vec<proto::register_metadata::NewMetadata>,

    /// *New* metadata that was registered since the last state update.
    ///
    /// This is emptied on every state update.
    new_metadata: Vec<proto::register_metadata::NewMetadata>,

    /// Map of task IDs to task static data.
    tasks: IdData<Task>,

    /// Map of task IDs to task stats.
    task_stats: IdData<TaskStats>,

    /// Map of resource IDs to resource static data.
    resources: IdData<Resource>,

    /// Map of resource IDs to resource stats.
    resource_stats: IdData<ResourceStats>,

    async_ops: IdData<AsyncOp>,

    async_op_stats: IdData<AsyncOpStats>,

    resource_ops: IdData<ResourceOp>,
}

#[derive(Debug)]
pub(crate) struct Flush {
    pub(crate) should_flush: Notify,
    pub(crate) triggered: AtomicBool,
}

// An entity that at some point in time can be closed
// and recycled
trait Closable {
    fn closed_at(&self) -> &Option<SystemTime>;
}

trait ToProto {
    type Result;
    fn to_proto(&self) -> Self::Result;
}
struct PollStats {
    current_polls: u64,
    polls: u64,
    first_poll: Option<SystemTime>,
    last_poll_started: Option<SystemTime>,
    last_poll_ended: Option<SystemTime>,
    busy_time: Duration,
}

// Represent static data for resources
struct Resource {
    id: span::Id,
    metadata: &'static Metadata<'static>,
    concrete_type: String,
    kind: String,
}

#[derive(Clone)]
enum AttrValue {
    Text(String),
    Numeric { val: u64, unit: String },
}

#[derive(Default)]
struct ResourceStats {
    created_at: Option<SystemTime>,
    closed_at: Option<SystemTime>,
    attributes: HashMap<String, AttrValue>,
}

// Represent static data for tasks
struct Task {
    id: span::Id,
    metadata: &'static Metadata<'static>,
    fields: Vec<proto::Field>,
}

struct TaskStats {
    // task stats
    created_at: Option<SystemTime>,
    closed_at: Option<SystemTime>,

    // waker stats
    wakes: u64,
    waker_clones: u64,
    waker_drops: u64,
    last_wake: Option<SystemTime>,

    poll_times_histogram: Histogram<u64>,
    poll_stats: PollStats,
}

struct AsyncOp {
    id: span::Id,
    metadata: &'static Metadata<'static>,
    source: String,
}

#[derive(Default)]
struct AsyncOpStats {
    created_at: Option<SystemTime>,
    closed_at: Option<SystemTime>,
    latest_poll_op: Option<proto::MetaId>,
    resource_id: Option<span::Id>,
    task_id: Option<span::Id>,
    poll_stats: PollStats,
}

struct ResourceOp {
    id: proto::MetaId,
    metadata: &'static Metadata<'static>,
    resource_id: span::Id,
    op_name: String,
    op_type: OpType,
}

#[derive(Default)]
struct IdData<T> {
    data: HashMap<span::Id, (T, bool)>,
}

impl Closable for ResourceStats {
    fn closed_at(&self) -> &Option<SystemTime> {
        &self.closed_at
    }
}

impl Closable for TaskStats {
    fn closed_at(&self) -> &Option<SystemTime> {
        &self.closed_at
    }
}

impl Closable for AsyncOpStats {
    fn closed_at(&self) -> &Option<SystemTime> {
        &self.closed_at
    }
}

impl Default for PollStats {
    fn default() -> Self {
        PollStats {
            current_polls: 0,
            polls: 0,
            first_poll: None,
            last_poll_started: None,
            last_poll_ended: None,
            busy_time: Default::default(),
        }
    }
}

impl Default for TaskStats {
    fn default() -> Self {
        TaskStats {
            created_at: None,
            closed_at: None,
            wakes: 0,
            waker_clones: 0,
            waker_drops: 0,
            last_wake: None,
            // significant figures should be in the [0-5] range and memory usage
            // grows exponentially with higher a sigfig
            poll_times_histogram: Histogram::<u64>::new(2).unwrap(),
            poll_stats: PollStats::default(),
        }
    }
}

impl Aggregator {
    pub(crate) fn new(
        events: mpsc::Receiver<Event>,
        rpcs: mpsc::Receiver<WatchKind>,
        builder: &crate::Builder,
    ) -> Self {
        Self {
            flush_capacity: Arc::new(Flush {
                should_flush: Notify::new(),
                triggered: AtomicBool::new(false),
            }),
            rpcs,
            publish_interval: builder.publish_interval,
            retention: builder.retention,
            events,
            watchers: Vec::new(),
            details_watchers: HashMap::new(),
            all_metadata: Vec::new(),
            new_metadata: Vec::new(),
            tasks: IdData {
                data: HashMap::<span::Id, (Task, bool)>::new(),
            },
            task_stats: IdData::default(),
            resources: IdData {
                data: HashMap::<span::Id, (Resource, bool)>::new(),
            },
            resource_stats: IdData::default(),

            async_ops: IdData {
                data: HashMap::<span::Id, (AsyncOp, bool)>::new(),
            },
            async_op_stats: IdData::default(),
            resource_ops: IdData {
                data: HashMap::<span::Id, (ResourceOp, bool)>::new(),
            },
        }
    }

    pub(crate) fn flush(&self) -> &Arc<Flush> {
        &self.flush_capacity
    }

    pub(crate) async fn run(mut self) {
        let mut publish = tokio::time::interval(self.publish_interval);
        loop {
            let should_send = tokio::select! {
                // if the flush interval elapses, flush data to the client
                _ = publish.tick() => {
                    true
                }

                // triggered when the event buffer is approaching capacity
                _ = self.flush_capacity.should_flush.notified() => {
                    self.flush_capacity.triggered.store(false, Release);
                    tracing::debug!("approaching capacity; draining buffer");
                    false
                }

                // a new client has started watching!
                subscription = self.rpcs.recv() => {
                    match subscription {
                        Some(WatchKind::Instrument(subscription)) => {
                            self.add_instrument_subscription(subscription);
                        },
                        Some(WatchKind::TaskDetail(watch_request)) => {
                            self.add_task_detail_subscription(watch_request);
                        },
                        _ => {
                            tracing::debug!("rpc channel closed, terminating");
                            return;
                        }
                    };

                    false
                }

            };

            // drain and aggregate buffered events.
            //
            // Note: we *don't* want to actually await the call to `recv` --- we
            // don't want the aggregator task to be woken on every event,
            // because it will then be woken when its own `poll` calls are
            // exited. that would result in a busy-loop. instead, we only want
            // to be woken when the flush interval has elapsed, or when the
            // channel is almost full.
            while let Some(event) = self.events.recv().now_or_never() {
                match event {
                    Some(event) => self.update_state(event),
                    // The channel closed, no more events will be emitted...time
                    // to stop aggregating.
                    None => {
                        tracing::debug!("event channel closed; terminating");
                        return;
                    }
                };
            }

            // flush data to clients, if there are any currently subscribed
            // watchers and we should send a new update.
            if !self.watchers.is_empty() && should_send {
                self.publish();
            }
            self.cleanup_closed();
        }
    }

    fn cleanup_closed(&mut self) {
        // drop all closed have that has completed *and* whose final data has already
        // been sent off.
        let now = SystemTime::now();
        let has_watchers = !self.watchers.is_empty();
        drop_closed(
            now,
            &mut self.tasks,
            &mut self.task_stats,
            self.retention,
            has_watchers,
        );
        drop_closed(
            now,
            &mut self.resources,
            &mut self.resource_stats,
            self.retention,
            has_watchers,
        );
        drop_closed(
            now,
            &mut self.async_ops,
            &mut self.async_op_stats,
            self.retention,
            has_watchers,
        );
    }

    /// Add the task subscription to the watchers after sending the first update
    fn add_instrument_subscription(
        &mut self,
        subscription: Watch<proto::instrument::InstrumentUpdate>,
    ) {
        tracing::debug!("new instrument subscription");
        let now = SystemTime::now();
        // Send the initial state --- if this fails, the subscription is already dead
        let update = &proto::instrument::InstrumentUpdate {
            task_update: Some(proto::tasks::TaskUpdate {
                new_tasks: self.tasks.as_proto(false).values().cloned().collect(),
                stats_update: self.task_stats.as_proto(false),
            }),
            resource_update: Some(proto::resources::ResourceUpdate {
                new_resources: self.resources.as_proto(false).values().cloned().collect(),
                stats_update: self.resource_stats.as_proto(false),
            }),
            async_op_update: Some(proto::async_ops::AsyncOpUpdate {
                new_async_ops: self.async_ops.as_proto(false).values().cloned().collect(),
                stats_update: self.async_op_stats.as_proto(false),
            }),
            resource_op_update: Some(proto::resource_ops::ResourceOpUpdate {
                new_resource_ops: self
                    .resource_ops
                    .as_proto(false)
                    .values()
                    .cloned()
                    .collect(),
            }),
            now: Some(now.into()),
            new_metadata: Some(proto::RegisterMetadata {
                metadata: self.all_metadata.clone(),
            }),
        };

        if subscription.update(update) {
            self.watchers.push(subscription)
        }
    }

    /// Add the task details subscription to the watchers after sending the first update,
    /// if the task is found.
    fn add_task_detail_subscription(
        &mut self,
        watch_request: WatchRequest<proto::tasks::TaskDetails>,
    ) {
        let WatchRequest {
            id,
            stream_sender,
            buffer,
        } = watch_request;
        tracing::debug!(id = ?id, "new task details subscription");
        let task_id: span::Id = id.into();
        if let Some(stats) = self.task_stats.get(&task_id) {
            let (tx, rx) = mpsc::channel(buffer);
            let subscription = Watch(tx);
            let now = SystemTime::now();
            // Send back the stream receiver.
            // Then send the initial state --- if this fails, the subscription is already dead.
            if stream_sender.send(rx).is_ok()
                && subscription.update(&proto::tasks::TaskDetails {
                    task_id: Some(task_id.clone().into()),
                    now: Some(now.into()),
                    poll_times_histogram: serialize_histogram(&stats.poll_times_histogram).ok(),
                })
            {
                self.details_watchers
                    .entry(task_id)
                    .or_insert_with(Vec::new)
                    .push(subscription);
            }
        }
        // If the task is not found, drop `stream_sender` which will result in a not found error
    }

    /// Publish the current state to all active watchers.
    ///
    /// This drops any watchers which have closed the RPC, or whose update
    /// channel has filled up.
    fn publish(&mut self) {
        let new_metadata = if !self.new_metadata.is_empty() {
            Some(proto::RegisterMetadata {
                metadata: std::mem::take(&mut self.new_metadata),
            })
        } else {
            None
        };

        let now = SystemTime::now();
        let update = proto::instrument::InstrumentUpdate {
            now: Some(now.into()),
            new_metadata,
            task_update: Some(proto::tasks::TaskUpdate {
                new_tasks: self.tasks.as_proto(true).values().cloned().collect(),
                stats_update: self.task_stats.as_proto(true),
            }),
            resource_update: Some(proto::resources::ResourceUpdate {
                new_resources: self.resources.as_proto(true).values().cloned().collect(),
                stats_update: self.resource_stats.as_proto(true),
            }),
            async_op_update: Some(proto::async_ops::AsyncOpUpdate {
                new_async_ops: self.async_ops.as_proto(true).values().cloned().collect(),
                stats_update: self.async_op_stats.as_proto(true),
            }),
            resource_op_update: Some(proto::resource_ops::ResourceOpUpdate {
                new_resource_ops: self.resource_ops.as_proto(true).values().cloned().collect(),
            }),
        };

        self.watchers
            .retain(|watch: &Watch<proto::instrument::InstrumentUpdate>| watch.update(&update));

        let stats = &self.task_stats;
        // Assuming there are much fewer task details subscribers than there are
        // stats updates, iterate over `details_watchers` and compact the map.
        self.details_watchers.retain(|id, watchers| {
            if let Some(task_stats) = stats.get(id) {
                let details = proto::tasks::TaskDetails {
                    task_id: Some(id.clone().into()),
                    now: Some(now.into()),
                    poll_times_histogram: serialize_histogram(&task_stats.poll_times_histogram)
                        .ok(),
                };
                watchers.retain(|watch| watch.update(&details));
                !watchers.is_empty()
            } else {
                false
            }
        });
    }

    /// Update the current state with data from a single event.
    fn update_state(&mut self, event: Event) {
        // do state update
        match event {
            Event::Metadata(meta) => {
                self.all_metadata.push(meta.into());
                self.new_metadata.push(meta.into());
            }
            Event::Spawn {
                id,
                metadata,
                at,
                fields,
                ..
            } => {
                self.tasks.insert(
                    id.clone(),
                    Task {
                        id: id.clone(),
                        metadata,
                        fields,
                        // TODO: parents
                    },
                );
                self.task_stats.insert(
                    id,
                    TaskStats {
                        created_at: Some(at),
                        ..Default::default()
                    },
                );
            }
            Event::Enter { id, at } => {
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    if task_stats.poll_stats.current_polls == 0 {
                        task_stats.poll_stats.last_poll_started = Some(at);
                        if task_stats.poll_stats.first_poll == None {
                            task_stats.poll_stats.first_poll = Some(at);
                        }
                        task_stats.poll_stats.polls += 1;
                    }
                    task_stats.poll_stats.current_polls += 1;
                }

                if let Some(mut async_op_stats) = self.async_op_stats.update(&id) {
                    if async_op_stats.poll_stats.current_polls == 0 {
                        async_op_stats.poll_stats.last_poll_started = Some(at);
                        if async_op_stats.poll_stats.first_poll == None {
                            async_op_stats.poll_stats.first_poll = Some(at);
                        }
                        async_op_stats.poll_stats.polls += 1;
                    }
                    async_op_stats.poll_stats.current_polls += 1;
                }
            }

            Event::Exit { id, at } => {
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    task_stats.poll_stats.current_polls -= 1;
                    if task_stats.poll_stats.current_polls == 0 {
                        if let Some(last_poll_started) = task_stats.poll_stats.last_poll_started {
                            let elapsed = at.duration_since(last_poll_started).unwrap();
                            task_stats.poll_stats.last_poll_ended = Some(at);
                            task_stats.poll_stats.busy_time += elapsed;
                            task_stats
                                .poll_times_histogram
                                .record(elapsed.as_nanos().try_into().unwrap_or(u64::MAX))
                                .unwrap();
                        }
                    }
                }

                if let Some(mut async_op_stats) = self.async_op_stats.update(&id) {
                    async_op_stats.poll_stats.current_polls -= 1;
                    if async_op_stats.poll_stats.current_polls == 0 {
                        if let Some(last_poll_started) = async_op_stats.poll_stats.last_poll_started
                        {
                            let elapsed = at.duration_since(last_poll_started).unwrap();
                            async_op_stats.poll_stats.last_poll_ended = Some(at);
                            async_op_stats.poll_stats.busy_time += elapsed;
                        }
                    }
                }
            }

            Event::Close { id, at } => {
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    task_stats.closed_at = Some(at);
                }

                // TODO: When resources and async ops are closed we need to also mark
                // the corresponding resource ops as closed, so they can be dropped later
                if let Some(mut resource_stats) = self.resource_stats.update(&id) {
                    resource_stats.closed_at = Some(at);
                }
                if let Some(mut async_op_stats) = self.async_op_stats.update(&id) {
                    async_op_stats.closed_at = Some(at);
                }
            }

            Event::Waker { id, op, at } => {
                // It's possible for wakers to exist long after a task has
                // finished. We don't want those cases to create a "new"
                // task that isn't closed, just to insert some waker stats.
                //
                // It may be useful to eventually be able to report about
                // "wasted" waker ops, but we'll leave that for another time.
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    match op {
                        WakeOp::Wake | WakeOp::WakeByRef => {
                            task_stats.wakes += 1;
                            task_stats.last_wake = Some(at);

                            // Note: `Waker::wake` does *not* call the `drop`
                            // implementation, so waking by value doesn't
                            // trigger a drop event. so, count this as a `drop`
                            // to ensure the task's number of wakers can be
                            // calculated as `clones` - `drops`.
                            //
                            // see
                            // https://github.com/rust-lang/rust/blob/673d0db5e393e9c64897005b470bfeb6d5aec61b/library/core/src/task/wake.rs#L211-L212
                            if let WakeOp::Wake = op {
                                task_stats.waker_drops += 1;
                            }
                        }
                        WakeOp::Clone => {
                            task_stats.waker_clones += 1;
                        }
                        WakeOp::Drop => {
                            task_stats.waker_drops += 1;
                        }
                    }
                }
            }

            Event::Resource {
                at,
                id,
                metadata,
                kind,
                concrete_type,
                ..
            } => {
                self.resources.insert(
                    id.clone(),
                    Resource {
                        id: id.clone(),
                        kind,
                        metadata,
                        concrete_type,
                    },
                );

                self.resource_stats.insert(
                    id,
                    ResourceStats {
                        created_at: Some(at),
                        ..Default::default()
                    },
                );
            }

            Event::ResourceOp {
                metadata,
                at,
                resource_id,
                op_name,
                op_type,
                ..
            } => {
                match op_type.clone() {
                    OpType::StateUpdate(attr_updates) => {
                        if let Some(mut stats) = self.resource_stats.update(&resource_id) {
                            for upd in attr_updates {
                                match stats.attributes.get_mut(&upd.name) {
                                    Some(attr) => attr.update(&upd.val),
                                    None => {
                                        stats.attributes.insert(upd.name.clone(), upd.val.into());
                                    }
                                }
                            }
                        }
                    }
                    OpType::Poll {
                        async_op_id,
                        task_id,
                        readiness,
                    } => {
                        // TODO: make it less hacky. Ideally we want to store
                        // these in a vec that gets cleaned periodically and allows
                        // us to send only the new ones much like the IdData map.
                        let mut async_op_stats = self.async_op_stats.update_or_default(async_op_id);
                        async_op_stats.poll_stats.polls += 1;
                        async_op_stats.latest_poll_op = Some(metadata.into());
                        if async_op_stats.task_id.is_none() {
                            async_op_stats.task_id = Some(task_id);
                        }
                        if async_op_stats.resource_id.is_none() {
                            async_op_stats.resource_id = Some(resource_id.clone());
                        }

                        if let Readiness::Pending = readiness {
                            if async_op_stats.poll_stats.first_poll.is_none() {
                                async_op_stats.poll_stats.first_poll = Some(at);
                            }
                        }
                    }
                }
                let id = span::Id::from_u64(metadata as *const _ as u64);
                self.resource_ops.insert(
                    id,
                    ResourceOp {
                        id: metadata.into(),
                        metadata,
                        resource_id,
                        op_name,
                        op_type,
                    },
                );
            }

            Event::AsyncResourceOp {
                at,
                id,
                source,
                metadata,
                ..
            } => {
                self.async_ops.insert(
                    id.clone(),
                    AsyncOp {
                        id: id.clone(),
                        metadata,
                        source,
                    },
                );

                self.async_op_stats.insert(
                    id,
                    AsyncOpStats {
                        created_at: Some(at),
                        ..Default::default()
                    },
                );
            }
        }
    }
}

// ==== impl Flush ===

impl Flush {
    pub(crate) fn trigger(&self) {
        if self
            .triggered
            .compare_exchange(false, true, AcqRel, Acquire)
            .is_ok()
        {
            self.should_flush.notify_one();
            tracing::trace!("flush triggered");
        } else {
            // someone else already did it, that's fine...
            tracing::trace!("flush already triggered");
        }
    }
}

impl<T> IdData<T> {
    fn update_or_default(&mut self, id: span::Id) -> Updating<'_, T>
    where
        T: Default,
    {
        Updating(self.data.entry(id).or_default())
    }

    fn update(&mut self, id: &span::Id) -> Option<Updating<'_, T>> {
        self.data.get_mut(id).map(Updating)
    }

    fn insert(&mut self, id: span::Id, data: T) {
        self.data.insert(id, (data, true));
    }

    fn since_last_update(&mut self) -> impl Iterator<Item = (&span::Id, &mut T)> {
        self.data.iter_mut().filter_map(|(id, (data, dirty))| {
            if *dirty {
                *dirty = false;
                Some((id, data))
            } else {
                None
            }
        })
    }

    fn all(&self) -> impl Iterator<Item = (&span::Id, &T)> {
        self.data.iter().map(|(id, (data, _))| (id, data))
    }

    fn get(&self, id: &span::Id) -> Option<&T> {
        self.data.get(id).map(|(data, _)| data)
    }

    fn as_proto(&mut self, updated_only: bool) -> HashMap<u64, T::Result>
    where
        T: ToProto,
    {
        if updated_only {
            return self
                .since_last_update()
                .map(|(id, d)| (id.into_u64(), d.to_proto()))
                .collect();
        }
        self.all()
            .map(|(id, d)| (id.into_u64(), d.to_proto()))
            .collect()
    }
}

struct Updating<'a, T>(&'a mut (T, bool));

impl<'a, T> Deref for Updating<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0 .0
    }
}

impl<'a, T> DerefMut for Updating<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0 .0
    }
}

impl<'a, T> Drop for Updating<'a, T> {
    fn drop(&mut self) {
        self.0 .1 = true;
    }
}

impl<T: Clone> Watch<T> {
    fn update(&self, update: &T) -> bool {
        if let Ok(reserve) = self.0.try_reserve() {
            reserve.send(Ok(update.clone()));
            true
        } else {
            false
        }
    }
}

impl ToProto for PollStats {
    type Result = proto::PollStats;

    fn to_proto(&self) -> Self::Result {
        proto::PollStats {
            polls: self.polls,
            first_poll: self.first_poll.map(Into::into),
            last_poll_started: self.last_poll_started.map(Into::into),
            last_poll_ended: self.last_poll_ended.map(Into::into),
            busy_time: Some(self.busy_time.into()),
        }
    }
}

impl ToProto for Task {
    type Result = proto::tasks::Task;

    fn to_proto(&self) -> Self::Result {
        proto::tasks::Task {
            id: Some(self.id.clone().into()),
            // TODO: more kinds of tasks...
            kind: proto::tasks::task::Kind::Spawn as i32,
            metadata: Some(self.metadata.into()),
            parents: Vec::new(), // TODO: implement parents nicely
            fields: self.fields.clone(),
        }
    }
}

impl ToProto for TaskStats {
    type Result = proto::tasks::Stats;

    fn to_proto(&self) -> Self::Result {
        proto::tasks::Stats {
            poll_stats: Some(self.poll_stats.to_proto()),
            created_at: self.created_at.map(Into::into),
            total_time: total_time(self.created_at.as_ref(), self.closed_at.as_ref())
                .map(Into::into),
            wakes: self.wakes,
            waker_clones: self.waker_clones,
            waker_drops: self.waker_drops,
            last_wake: self.last_wake.map(Into::into),
        }
    }
}

impl ToProto for Resource {
    type Result = proto::resources::Resource;

    fn to_proto(&self) -> Self::Result {
        proto::resources::Resource {
            id: Some(self.id.clone().into()),
            kind: self.kind.clone(),
            metadata: Some(self.metadata.into()),
            concrete_type: self.concrete_type.clone(),
        }
    }
}

impl ToProto for ResourceStats {
    type Result = proto::resources::Stats;

    fn to_proto(&self) -> Self::Result {
        let attributes = self
            .attributes
            .values()
            .cloned()
            .map(|attr| attr.to_proto())
            .collect();
        proto::resources::Stats {
            created_at: self.created_at.map(Into::into),
            total_time: total_time(self.created_at.as_ref(), self.closed_at.as_ref())
                .map(Into::into),
            attributes,
        }
    }
}

impl ToProto for AsyncOp {
    type Result = proto::async_ops::AsyncOp;

    fn to_proto(&self) -> Self::Result {
        proto::async_ops::AsyncOp {
            id: Some(self.id.clone().into()),
            metadata: Some(self.metadata.into()),
            source: self.source.clone(),
        }
    }
}

impl ToProto for AsyncOpStats {
    type Result = proto::async_ops::Stats;

    fn to_proto(&self) -> Self::Result {
        proto::async_ops::Stats {
            poll_stats: Some(self.poll_stats.to_proto()),
            created_at: self.created_at.map(Into::into),
            total_time: total_time(self.created_at.as_ref(), self.closed_at.as_ref())
                .map(Into::into),

            latest_poll_op: self.latest_poll_op.clone(),
            resource_id: self.resource_id.clone().map(Into::into),
            task_id: self.task_id.clone().map(Into::into),
        }
    }
}

impl ToProto for ResourceOp {
    type Result = proto::resource_ops::ResourceOp;

    fn to_proto(&self) -> Self::Result {
        proto::resource_ops::ResourceOp {
            id: Some(self.id.clone()),
            metadata: Some(self.metadata.into()),
            op_type: Some(self.op_type.to_proto()),
            resource_id: Some(self.resource_id.clone().into()),
            name: self.op_name.clone(),
        }
    }
}

impl ToProto for OpType {
    type Result = proto::resource_ops::OpType;

    fn to_proto(&self) -> Self::Result {
        use console_api::resource_ops::op_type::OpType as PbOpType;
        use proto::resource_ops::op_type::poll::Readiness as PbReadiness;
        use proto::resource_ops::op_type::Poll as PbPoll;
        use proto::resource_ops::op_type::StateUpdate as PbStateUpdate;

        match self {
            OpType::Poll {
                async_op_id,
                task_id,
                readiness,
            } => proto::resource_ops::OpType {
                op_type: Some(PbOpType::Poll(PbPoll {
                    task_id: Some(task_id.clone().into()),
                    async_op_id: Some(async_op_id.clone().into()),
                    readiness: match readiness {
                        Readiness::Pending => PbReadiness::Pending,
                        Readiness::Ready => PbReadiness::Ready,
                    } as i32,
                })),
            },
            OpType::StateUpdate(attrs) => proto::resource_ops::OpType {
                op_type: Some(PbOpType::StateUpdate(PbStateUpdate {
                    updates: attrs.iter().map(ToProto::to_proto).collect(),
                })),
            },
        }
    }
}

impl ToProto for AttributeUpdate {
    type Result = proto::resource_ops::op_type::state_update::AttributeUpdate;
    fn to_proto(&self) -> Self::Result {
        use console_api::resource_ops::op_type::state_update::attribute_update::Update as PbUpdate;
        use proto::resource_ops::op_type::state_update::attribute_update::numeric;
        use proto::resource_ops::op_type::state_update::attribute_update::Numeric as PbNumeric;
        use proto::resource_ops::op_type::state_update::AttributeUpdate as PbAttrUpdate;

        match &self.val {
            AttributeUpdateValue::Text(val) => PbAttrUpdate {
                name: self.name.clone(),
                update: Some(PbUpdate::Text(val.clone())),
            },

            AttributeUpdateValue::Numeric { val, op, unit } => {
                let val = *val;
                let unit = unit.clone();
                let op = match op {
                    AttributeUpdateOp::Add => numeric::Op::Add,
                    AttributeUpdateOp::Sub => numeric::Op::Sub,
                    AttributeUpdateOp::Ovr => numeric::Op::Ovr,
                } as i32;
                PbAttrUpdate {
                    name: self.name.clone(),
                    update: Some(PbUpdate::Numeric(PbNumeric { val, op, unit })),
                }
            }
        }
    }
}

impl ToProto for AttrValue {
    type Result = proto::resources::stats::AttrValue;

    fn to_proto(&self) -> Self::Result {
        use proto::resources::stats::attr_value::Numeric;
        use proto::resources::stats::attr_value::Val;
        use proto::resources::stats::AttrValue as PbAttrValue;

        match self {
            AttrValue::Text(t) => PbAttrValue {
                val: Some(Val::Text(t.clone())),
            },
            AttrValue::Numeric { val, unit } => PbAttrValue {
                val: Some(Val::Numeric(Numeric {
                    val: *val,
                    unit: unit.clone(),
                })),
            },
        }
    }
}

impl AttrValue {
    fn update(&mut self, update: &AttributeUpdateValue) {
        match (self, update) {
            (AttrValue::Text(t), AttributeUpdateValue::Text(upd)) => {
                *t = upd.clone();
            }
            (
                AttrValue::Numeric { ref mut val, .. },
                AttributeUpdateValue::Numeric { val: upd, op, .. },
            ) => match op {
                AttributeUpdateOp::Add => {
                    let new_val = *val + upd;
                    *val = new_val;
                }
                AttributeUpdateOp::Ovr => {
                    *val = *upd;
                }
                AttributeUpdateOp::Sub => {
                    let new_val = *val - upd;
                    *val = new_val;
                }
            },
            _ => panic!("trying to update attributes of different type"),
        }
    }
}

impl From<AttributeUpdateValue> for AttrValue {
    fn from(upd: AttributeUpdateValue) -> Self {
        match upd {
            AttributeUpdateValue::Text(t) => AttrValue::Text(t),
            AttributeUpdateValue::Numeric { val, unit, .. } => AttrValue::Numeric { val, unit },
        }
    }
}

fn serialize_histogram(histogram: &Histogram<u64>) -> Result<Vec<u8>, V2SerializeError> {
    let mut serializer = V2Serializer::new();
    let mut buf = Vec::new();
    serializer.serialize(histogram, &mut buf)?;
    Ok(buf)
}

fn total_time(created_at: Option<&SystemTime>, closed_at: Option<&SystemTime>) -> Option<Duration> {
    closed_at.and_then(|end| created_at.and_then(|start| end.duration_since(*start).ok()))
}

/// Drops all tasks, resources and ops that are not alive anymore
fn drop_closed<T, R: Closable>(
    now: SystemTime,
    entities: &mut IdData<T>,
    stats: &mut IdData<R>,
    retention: Duration,
    has_watchers: bool,
) {
    // drop stats for closed tasks if they have been updated
    tracing::trace!(?retention, has_watchers, "dropping closed entities...");

    let stats_len_0 = stats.data.len();
    stats.data.retain(|id, (stats, dirty)| {
        if let Some(closed) = stats.closed_at() {
            let closed_for = now.duration_since(*closed).unwrap_or_default();
            let should_drop =
                    // if there are any clients watching, retain all dirty tasks regardless of age
                    (*dirty && has_watchers)
                    || closed_for > retention;
            tracing::trace!(
                stats.id = ?id,
                stats.closed_at = ?closed,
                stats.closed_for = ?closed_for,
                stats.dirty = *dirty,
                should_drop,
            );
            return !should_drop;
        }

        true
    });

    let stats_len_1 = stats.data.len();

    // drop closed entities which no longer have stats.
    let entities_len_0 = entities.data.len();
    entities
        .data
        .retain(|id, (_, _)| stats.data.contains_key(id));
    let entities_len_1 = entities.data.len();
    let dropped_stats = stats_len_0 - stats_len_1;

    let stats_len_1 = stats.data.len();
    if dropped_stats > 0 {
        tracing::debug!(
            tasks.dropped = entities_len_0 - entities_len_1,
            tasks.len = entities_len_1,
            stats.dropped = dropped_stats,
            stats.tasks = stats_len_1,
            "dropped closed entities"
        );
    } else {
        tracing::trace!(
            entities.len = entities_len_1,
            stats.len = stats_len_1,
            "no closed entities were droppable"
        );
    }
}
