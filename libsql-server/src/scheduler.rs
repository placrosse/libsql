use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use anyhow::Result;
use crossbeam::channel::{Sender, TrySendError};
use smallvec::SmallVec;
use tokio::sync::mpsc::{UnboundedReceiver as TokioReceiver, UnboundedSender as TokioSender};

use crate::job::Job;
use crate::messages::Responder;
use crate::statements::Statements;

pub type ClientId = usize;

#[derive(Default)]
struct ClientQueue {
    queue: VecDeque<Job>,
    /// Sender to the active transaction for this client.
    /// On ready state, jobs for this client should be sent to this channel instead of the global queue.
    active_txn: Option<Sender<Job>>,
    /// The client for this queue has disconnected
    should_close: bool,
}

#[derive(Debug)]
pub enum UpdateStateMessage {
    Ready(ClientId),
    TxnBegin(ClientId, Sender<Job>),
    TxnEnded(ClientId),
}

#[derive(Debug)]
pub enum Action {
    Disconnect,
    Execute(Statements),
}

pub struct ServerMessage {
    pub client_id: ClientId,
    pub action: Action,
    pub responder: Box<dyn Responder>,
}

impl fmt::Debug for ServerMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerMessage")
            .field("client_id", &self.client_id)
            .field("action", &self.action)
            .finish()
    }
}

pub struct Scheduler {
    worker_pool_sender: Sender<Job>,
    queues: HashMap<ClientId, ClientQueue>,
    /// The receiving end of the channel the pool uses to notify the scheduler of the state
    /// updates for its queues
    update_state_receiver: TokioReceiver<UpdateStateMessage>,
    update_state_sender: TokioSender<UpdateStateMessage>,
    /// Receiver from the server with new statements to run
    job_receiver: TokioReceiver<ServerMessage>,

    /// Set of endpoints that are ready to give some work, i.e that have no inflight work
    ready_set: HashSet<ClientId>,
    /// Set of endpoints that have some work in their queue
    has_work_set: HashSet<ClientId>,
}

impl Scheduler {
    pub fn new(
        worker_pool_sender: Sender<Job>,
        job_receiver: TokioReceiver<ServerMessage>,
    ) -> Result<Self> {
        let (update_state_sender, update_state_receiver) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            worker_pool_sender,
            queues: Default::default(),
            update_state_receiver,
            update_state_sender,
            job_receiver,
            ready_set: Default::default(),
            has_work_set: Default::default(),
        })
    }

    /// push some work to the gobal queue
    fn schedule_work(&mut self) {
        let mut not_waiting = SmallVec::<[ClientId; 16]>::new();
        let mut not_ready = SmallVec::<[ClientId; 16]>::new();

        for client_id in self.ready_set.intersection(&self.has_work_set).copied() {
            let Some(queue) = self.queues.get_mut(&client_id) else {
                not_ready.push(client_id);
                not_waiting.push(client_id);
                continue
            };

            let Some(mut job) = queue.queue.pop_front() else {
                not_waiting.push(client_id);
                continue
            };

            not_ready.push(client_id);

            // there is an active transaction, so we should send it there
            if let Some(ref sender) = queue.active_txn {
                job = match sender.try_send(job) {
                    Ok(_) => {
                        continue;
                    }
                    // the transaction channel was closed before we were notified, we'll send
                    // to the main queue instead
                    Err(TrySendError::Disconnected(job)) => {
                        queue.active_txn.take();
                        job
                    }
                    Err(TrySendError::Full(_)) => {
                        unreachable!("txn channel should never be full")
                    }
                };
            }

            // submit job to the main queue:
            self.worker_pool_sender
                .send(job)
                .expect("worker pool crashed");

            if queue.queue.is_empty() {
                not_waiting.push(client_id);
                if queue.should_close {
                    self.queues.remove(&client_id);
                }
            }
        }

        for e in &not_ready {
            self.ready_set.remove(e);
        }

        for e in &not_waiting {
            self.has_work_set.remove(e);
        }
    }

    /// Update the queue with new status, and return whether there might be more work ready to do;
    fn update_queue_status(&mut self, update: UpdateStateMessage) {
        match update {
            UpdateStateMessage::Ready(e) => {
                self.ready_set.insert(e);
            }
            UpdateStateMessage::TxnBegin(e, sender) => {
                if let Some(queue) = self.queues.get_mut(&e) {
                    assert!(queue.active_txn.is_none());
                    queue.active_txn.replace(sender);
                }
            }
            UpdateStateMessage::TxnEnded(e) => {
                if let Some(queue) = self.queues.get_mut(&e) {
                    // it's ok if the txn was already removed
                    queue.active_txn.take();
                    self.ready_set.insert(e);
                }
            }
        }
    }

    /// Update queues with new incoming tasks from server.
    fn update_queues(&mut self, msg: ServerMessage) {
        log::debug!("got server message: {msg:?}");
        match msg.action {
            Action::Disconnect => {
                self.queues
                    .get_mut(&msg.client_id)
                    .map(|q| q.should_close = true);
            }
            Action::Execute(statements) => {
                let job = Job {
                    scheduler_sender: self.update_state_sender.clone(),
                    statements,
                    client_id: msg.client_id,
                    responder: msg.responder,
                };

                self.queues
                    .entry(msg.client_id)
                    .or_insert_with(|| {
                        // This is the first time we see this client, so it's ready by default
                        self.ready_set.insert(msg.client_id);
                        Default::default()
                    })
                    .queue
                    .push_back(job);

                self.has_work_set.insert(msg.client_id);
            }
        }
    }

    pub async fn start(mut self) {
        let mut should_exit = false;
        loop {
            tokio::select! {
                msg = self.update_state_receiver.recv() => {
                    match msg {
                        Some(msg) => {
                            self.update_queue_status(msg);
                        }
                        None => unreachable!("Scheduler still owns a sender"),
                    }
                },
                msg = self.job_receiver.recv(), if !should_exit => {
                    match msg {
                        Some(msg) => self.update_queues(msg),
                        None => should_exit = true,
                    }
                }
            }

            self.schedule_work();

            if should_exit
                // no queue has work left
                && self.has_work_set.is_empty()
                // no queue has inflight work
                && self.ready_set.len() == self.queues.len()
            {
                break;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use crossbeam::channel::TryRecvError;
    use proptest::prelude::*;
    use rand::{thread_rng, Rng};
    use std::collections::hash_map::Entry;

    use crate::messages::Message;

    use super::*;

    struct MockResponder;

    impl Responder for MockResponder {
        fn respond(&self, _: Message) {}
    }

    #[tokio::test]
    async fn client_jobs_are_sequential() {
        let (job_sender, job_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (pool_sender, pool_receiver) = crossbeam::channel::unbounded();
        let scheduler = Scheduler::new(pool_sender, job_receiver).unwrap();

        tokio::spawn(scheduler.start());

        job_sender
            .send(ServerMessage {
                client_id: 0,
                action: Action::Execute(Statements::parse("SELECT * FROM test;".into()).unwrap()),
                responder: Box::new(MockResponder),
            })
            .unwrap();
        job_sender
            .send(ServerMessage {
                client_id: 0,
                action: Action::Execute(Statements::parse("SELECT * FROM test2;".into()).unwrap()),
                responder: Box::new(MockResponder),
            })
            .unwrap();

        // sleep a bit to make sure scheduler had time to schedule something.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let job = pool_receiver.try_recv().unwrap();

        // the second job was not enqueued
        assert_eq!(pool_receiver.try_recv().unwrap_err(), TryRecvError::Empty);

        assert_eq!(job.statements.stmts, "SELECT * FROM test;");
        // signal ready
        job.scheduler_sender
            .send(UpdateStateMessage::Ready(0))
            .unwrap();

        // sleep a bit more to the next job be scheduled
        tokio::time::sleep(Duration::from_millis(10)).await;

        let job = pool_receiver.try_recv().unwrap();
        assert_eq!(pool_receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(job.statements.stmts, "SELECT * FROM test2;");
    }

    #[tokio::test]
    async fn different_clients_processed_concurrently() {
        let (job_sender, job_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (pool_sender, pool_receiver) = crossbeam::channel::unbounded();
        let scheduler = Scheduler::new(pool_sender, job_receiver).unwrap();

        tokio::spawn(scheduler.start());

        job_sender
            .send(ServerMessage {
                client_id: 0,
                action: Action::Execute(Statements::parse("SELECT * FROM test;".into()).unwrap()),
                responder: Box::new(MockResponder),
            })
            .unwrap();
        job_sender
            .send(ServerMessage {
                client_id: 1,
                action: Action::Execute(Statements::parse("SELECT * FROM test2;".into()).unwrap()),
                responder: Box::new(MockResponder),
            })
            .unwrap();

        tokio::time::sleep(Duration::from_millis(5)).await;

        let job1 = pool_receiver.try_recv().unwrap();
        let job2 = pool_receiver.try_recv().unwrap();

        assert_eq!(pool_receiver.try_recv().unwrap_err(), TryRecvError::Empty);

        assert_eq!(job1.statements.stmts, "SELECT * FROM test;");
        assert_eq!(job2.statements.stmts, "SELECT * FROM test2;");

        job1.scheduler_sender
            .send(UpdateStateMessage::Ready(0))
            .unwrap();
        job1.scheduler_sender
            .send(UpdateStateMessage::Ready(1))
            .unwrap();

        // queue is empty
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(pool_receiver.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    proptest! {
        /// This test's goal is to schedule random jobs and make sure that:
        /// - all jobs get processed
        /// - Jobs for each client are processed sequentially
        /// - No two jobs for an enpoint get processed in the same batch.
        ///
        /// /!\ This test takes some time to run!
        #[test]
        fn test_random_scheduling(
            num_tasks in 20..100usize,
            num_clients in 1..20usize,
        ) {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let (job_sender, job_receiver) = tokio::sync::mpsc::unbounded_channel();
                let (pool_sender, pool_receiver) = crossbeam::channel::unbounded();
                let scheduler = Scheduler::new(pool_sender, job_receiver).unwrap();

                tokio::spawn(scheduler.start());

                let mut rng = thread_rng();
                for i in 0..num_tasks {
                    let client_id = rng.gen_range(0..num_clients);
                    let msg = ServerMessage {
                        client_id,
                        // this is a hack here to pass a sequence number.
                        action: Action::Execute(Statements::parse(format!("SELECT * FROM \"{i}\"")).unwrap()),
                        responder: Box::new(MockResponder),
                    };

                    job_sender.send(msg).unwrap();
                }

                drop(job_sender);

                let mut seen_tasks = 0;
                let mut client_last_task_id = HashMap::new();
                'outer: loop {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let mut batch = Vec::new();
                    loop {
                        match pool_receiver.try_recv() {
                            Ok(job) => {
                                batch.push(job);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break 'outer,
                        }
                    }

                    seen_tasks += batch.len();

                    let mut distinct_clients = HashSet::new();
                    for j in &batch {
                        distinct_clients.insert(j.client_id);
                        j.scheduler_sender.send(UpdateStateMessage::Ready(j.client_id)).unwrap();
                        let new_task_idx = j.statements.stmts
                            .split_whitespace()
                            .last()
                            .unwrap()
                            .trim_matches('"')
                            .parse::<usize>()
                            .unwrap();

                        match client_last_task_id.entry(j.client_id) {
                            Entry::Occupied(mut old) => {
                                assert!(*old.get() < new_task_idx);
                                old.insert(new_task_idx);
                            },
                            Entry::Vacant(e) => {
                                e.insert(new_task_idx);
                            },
                        }
                    }

                    // only a task per client is scheduled at a time.
                    assert_eq!(batch.len(), distinct_clients.len());
                }

                // all tasks have been processed
                assert_eq!(seen_tasks, num_tasks);
            })
        }
    }
}
