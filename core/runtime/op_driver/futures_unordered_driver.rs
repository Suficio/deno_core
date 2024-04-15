// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.
use super::future_arena::FutureAllocation;
use super::future_arena::FutureArena;
use super::op_results::*;
use super::OpDriver;
use super::OpInflightStats;
use crate::OpId;
use crate::PromiseId;
use anyhow::Error;
use bit_set::BitSet;
use deno_unsync::UnsyncWaker;
use futures::future::poll_fn;
use futures::stream::FuturesUnordered;
use futures::task::noop_waker_ref;
use futures::FutureExt;
use futures::Stream;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::ready;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::ready;
use std::task::Context;
use std::task::Poll;

async fn poll_task<C: OpMappingContext>(
  mut results: SubmissionQueueResults<
    FuturesUnordered<FutureAllocation<PendingOp<C>, PendingOpInfo>>,
  >,
  tx: Rc<RefCell<VecDeque<PendingOp<C>>>>,
  tx_waker: Rc<UnsyncWaker>,
) {
  loop {
    let ready = poll_fn(|cx| results.poll_next_unpin(cx)).await;
    tx.borrow_mut().push_back(ready);
    tx_waker.wake_by_ref();
  }
}

/// [`OpDriver`] implementation built on a tokio [`JoinSet`].
pub struct FuturesUnorderedDriver<
  C: OpMappingContext + 'static = V8OpMappingContext,
> {
  len: Cell<usize>,
  queue: SubmissionQueue<
    FuturesUnordered<FutureAllocation<PendingOp<C>, PendingOpInfo>>,
  >,
  completed_ops: Rc<RefCell<VecDeque<PendingOp<C>>>>,
  completed_waker: Rc<UnsyncWaker>,
  arena: FutureArena<PendingOp<C>, PendingOpInfo>,
}

impl<C: OpMappingContext + 'static> Drop for FuturesUnorderedDriver<C> {
  fn drop(&mut self) {
    self.shutdown()
  }
}

impl<C: OpMappingContext> FuturesUnorderedDriver<C> {
  /// Spawn a polled task inside a [`FutureAllocation`], along with a function that can map it to a [`PendingOp`].
  #[inline(always)]
  fn spawn(&self, task: FutureAllocation<PendingOp<C>, PendingOpInfo>) {
    self.len.set(self.len.get() + 1);
    self.queue.spawn(task);
  }
}

impl<C: OpMappingContext> OpDriver<C> for FuturesUnorderedDriver<C> {
  fn new() -> (Self, impl Future<Output = ()> + 'static) {
    let (queue, results) = new_submission_queue();
    let completed_ops = Rc::new(RefCell::new(VecDeque::with_capacity(128)));
    let completed_waker = Rc::new(UnsyncWaker::default());
    let op_driver_poll_task =
      poll_task(results, completed_ops.clone(), completed_waker.clone());
    let op_driver = Self {
      len: Default::default(),
      completed_ops,
      queue,
      completed_waker,
      arena: Default::default(),
    };
    (op_driver, op_driver_poll_task)
  }

  fn submit_op_fallible<
    R: 'static,
    E: Into<Error> + 'static,
    const LAZY: bool,
    const DEFERRED: bool,
  >(
    &self,
    op_id: OpId,
    promise_id: i32,
    op: impl Future<Output = Result<R, E>> + 'static,
    rv_map: C::MappingFn<R>,
  ) -> Option<Result<R, E>> {
    {
      let info = PendingOpMappingInfo::<_, _, true>(
        PendingOpInfo(promise_id, op_id),
        rv_map,
      );
      let mut pinned = self.arena.allocate(info, op);

      if LAZY {
        self.spawn(pinned.erase());
        return None;
      }

      // We poll every future here because it's much faster to return a result than
      // spin the event loop to get it.
      match pinned.poll_unpin(&mut Context::from_waker(noop_waker_ref())) {
        Poll::Pending => self.spawn(pinned.erase()),
        Poll::Ready(res) => {
          if DEFERRED {
            drop(pinned);
            self.spawn(self.arena.allocate(info, ready(res)).erase())
          } else {
            return Some(res);
          }
        }
      };

      None
    }
  }

  fn submit_op_infallible<
    R: 'static,
    const LAZY: bool,
    const DEFERRED: bool,
  >(
    &self,
    op_id: OpId,
    promise_id: i32,
    op: impl Future<Output = R> + 'static,
    rv_map: C::MappingFn<R>,
  ) -> Option<R> {
    {
      let info = PendingOpMappingInfo::<_, _, false>(
        PendingOpInfo(promise_id, op_id),
        rv_map,
      );
      let mut pinned = self.arena.allocate(info, op);

      if LAZY {
        self.spawn(pinned.erase());
        return None;
      }

      // We poll every future here because it's much faster to return a result than
      // spin the event loop to get it.
      match Pin::new(&mut pinned)
        .poll(&mut Context::from_waker(noop_waker_ref()))
      {
        Poll::Pending => self.spawn(pinned.erase()),
        Poll::Ready(res) => {
          if DEFERRED {
            drop(pinned);
            self.spawn(self.arena.allocate(info, ready(res)).erase())
          } else {
            return Some(res);
          }
        }
      };

      None
    }
  }

  #[inline(always)]
  fn poll_ready(
    &self,
    cx: &mut Context,
  ) -> Poll<(PromiseId, OpId, OpResult<C>)> {
    let mut ops = self.completed_ops.borrow_mut();
    if ops.is_empty() {
      self.completed_waker.register(cx.waker());
      return Poll::Pending;
    }
    let item = ops.pop_front().unwrap();
    let PendingOp(PendingOpInfo(promise_id, op_id), resp) = item;
    self.len.set(self.len.get() - 1);
    Poll::Ready((promise_id, op_id, resp))
  }

  #[inline(always)]
  fn len(&self) -> usize {
    self.len.get()
  }

  fn shutdown(&self) {
    self.completed_ops.borrow_mut().clear();
    self.queue.queue.queue.borrow_mut().clear();
  }

  fn stats(&self, op_exclusions: &BitSet) -> OpInflightStats {
    let q = self.queue.queue.queue.borrow();
    let mut v: Vec<PendingOpInfo> = Vec::with_capacity(self.len.get());
    for f in q.iter() {
      let context = f.context();
      if !op_exclusions.contains(context.1 as _) {
        v.push(context);
      }
    }
    OpInflightStats {
      ops: v.into_boxed_slice(),
    }
  }
}

impl<F: Future<Output = R>, R> SubmissionQueueFutures for FuturesUnordered<F> {
  type Future = F;
  type Output = F::Output;

  fn len(&self) -> usize {
    self.len()
  }

  fn spawn(&mut self, f: Self::Future) {
    self.push(f)
  }

  fn poll_next_unpin(&mut self, cx: &mut Context) -> Poll<Self::Output> {
    Poll::Ready(ready!(Pin::new(self).poll_next(cx)).unwrap())
  }
}

#[derive(Default)]
struct Queue<F: SubmissionQueueFutures> {
  queue: RefCell<F>,
  item_waker: UnsyncWaker,
}

pub trait SubmissionQueueFutures: Default {
  type Future: Future<Output = Self::Output>;
  type Output;

  fn len(&self) -> usize;
  fn spawn(&mut self, f: Self::Future);
  fn poll_next_unpin(&mut self, cx: &mut Context) -> Poll<Self::Output>;
}

pub struct SubmissionQueueResults<F: SubmissionQueueFutures> {
  queue: Rc<Queue<F>>,
}

impl<F: SubmissionQueueFutures> SubmissionQueueResults<F> {
  pub fn poll_next_unpin(&mut self, cx: &mut Context) -> Poll<F::Output> {
    let mut queue = self.queue.queue.borrow_mut();
    self.queue.item_waker.register(cx.waker());
    if queue.len() == 0 {
      return Poll::Pending;
    }
    queue.poll_next_unpin(cx)
  }
}

pub struct SubmissionQueue<F: SubmissionQueueFutures> {
  queue: Rc<Queue<F>>,
}

impl<F: SubmissionQueueFutures> SubmissionQueue<F> {
  pub fn spawn(&self, f: F::Future) {
    self.queue.queue.borrow_mut().spawn(f);
    self.queue.item_waker.wake_by_ref();
  }
}

/// Create a [`SubmissionQueue`] and [`SubmissionQueueResults`] that allow for submission of tasks
/// and reception of task results. We may add work to the [`SubmissionQueue`] from any task, and the
/// [`SubmissionQueueResults`] will be polled from a single location.
pub fn new_submission_queue<F: SubmissionQueueFutures>(
) -> (SubmissionQueue<F>, SubmissionQueueResults<F>) {
  let queue: Rc<Queue<F>> = Default::default();
  (
    SubmissionQueue {
      queue: queue.clone(),
    },
    SubmissionQueueResults { queue },
  )
}
