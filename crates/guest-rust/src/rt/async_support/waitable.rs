//! Generic support for "any waitable" and performing asynchronous operations on
//! that waitable.

use super::cabi;
use std::boxed::Box;
use std::ffi::c_void;
use std::future::{Future, poll_fn};
use std::mem;
use std::pin::Pin;
use std::ptr;
use std::ptr::NonNull;
use std::task::{Context, Poll, Waker};

/// Generic future-based operation on any "waitable" in the component model.
///
/// This is used right now to power futures and streams for both read/write
/// halves. This structure is driven by `S`, an implementation of
/// [`WaitableOp`], which codifies the various state transitions and what to do
/// on each state transition.
pub struct WaitableOperation<S: WaitableOp> {
    inner: NonNull<OperationInner<S>>,
}

struct OperationInner<S: WaitableOp> {
    op: S,
    state: WaitableOperationState<S>,
    /// Storage for the final result of this asynchronous operation, if it's
    /// completed asynchronously.
    completion_status: CompletionStatus,
    detached: bool,
    cancelling: bool,
}

/// Structure used to store the `u32` return code from the canonical ABI about
/// an asynchronous operation.
struct CompletionStatus {
    /// Where the async operation's code is filled in, and `None` until that
    /// happens.
    code: Option<u32>,
    waker: Option<Waker>,
}

/// Helper trait to be used with `WaitableOperation` to assist with machinery
/// necessary to track in-flight reads/writes on futures.
///
/// # Unsafety
///
/// This trait is `unsafe` as it has various guarantees that must be upheld by
/// implementors such as:
///
/// * `S::in_progress_waitable` must always return the same value for the state
///   given.
pub unsafe trait WaitableOp {
    /// Initial state of this operation, used to kick off the actual component
    /// model operation and transition to `InProgress`.
    type Start;

    /// Intermediate state of this operation when the component model is
    /// involved but it hasn't resolved just yet.
    type InProgress;

    /// Result type of this operation.
    type Result;

    /// Result of when this operation is cancelled.
    type Cancel;

    /// Starts the async operation.
    ///
    /// This method will actually call `{future,stream}.{read,write}` with
    /// `state` provided. The return code of the intrinsic is returned here
    /// along with the `InProgress` state.
    fn start(&mut self, state: Self::Start) -> (u32, Self::InProgress);

    /// Optionally complete the async operation.
    ///
    /// This method will transition from the `InProgress` state, with some
    /// status code that was received, to either a completed result or a new
    /// `InProgress` state. This is invoked when:
    ///
    /// * a new status code has been received by an async export's `callback`
    /// * cancellation returned a code to be processed here
    fn in_progress_update(
        &mut self,
        state: Self::InProgress,
        code: u32,
    ) -> Result<Self::Result, Self::InProgress>;

    /// Conversion from the "start" state to the "cancel" result, needed when an
    /// operation is cancelled before it's started.
    fn start_cancelled(&mut self, state: Self::Start) -> Self::Cancel;

    /// Acquires the component-model `waitable` index that the `InProgress`
    /// state is waiting on.
    fn in_progress_waitable(&mut self, state: &Self::InProgress) -> u32;

    /// Initiates a request for cancellation of this operation. Returns the
    /// status code returned by the `{future,stream}.cancel-{read,write}`
    /// intrinsic.
    ///
    /// Note that this must synchronously complete the operation somehow. This
    /// cannot return a status code indicating that an operation is pending,
    /// instead the operation must be complete with the returned code. That may
    /// mean that this intrinsic can block while figuring things out in the
    /// component model ABI, for example.
    fn in_progress_cancel(&mut self, state: &mut Self::InProgress) -> u32;

    /// Converts a "completion result" into a "cancel result". This is necessary
    /// when an in-progress operation is cancelled so the in-progress result is
    /// first acquired and then transitioned to a cancel request.
    fn result_into_cancel(&mut self, result: Self::Result) -> Self::Cancel;

    /// Whether dropping this operation is allowed to detach it from the Rust
    /// future and let the component-model operation continue in the background.
    fn detach_on_drop() -> bool
    where
        Self: Sized,
    {
        false
    }
}

enum WaitableOperationState<S: WaitableOp> {
    Start(S::Start),
    InProgress(S::InProgress),
    Done,
}

impl<S> WaitableOperation<S>
where
    S: WaitableOp,
{
    /// Creates a new operation in the initial state.
    pub fn new(op: S, state: S::Start) -> WaitableOperation<S> {
        let inner = Box::new(OperationInner {
            op,
            state: WaitableOperationState::Start(state),
            completion_status: CompletionStatus {
                code: None,
                waker: None,
            },
            detached: false,
            cancelling: false,
        });
        WaitableOperation {
            inner: NonNull::from(Box::leak(inner)),
        }
    }

    fn inner(&self) -> &OperationInner<S> {
        unsafe { self.inner.as_ref() }
    }

    fn inner_mut(&mut self) -> &mut OperationInner<S> {
        unsafe { self.inner.as_mut() }
    }

    unsafe fn drop_inner(&mut self) {
        drop(unsafe { Box::from_raw(self.inner.as_ptr()) });
    }

    unsafe fn register_raw(ptr: *mut OperationInner<S>, waitable: u32) {
        let task = unsafe { cabi::wasip3_task_set(ptr::null_mut()) };
        assert!(!task.is_null());
        assert!(unsafe { (*task).version } >= cabi::WASIP3_TASK_V1);
        let prev = unsafe {
            ((*task).waitable_register)((*task).ptr, waitable, cabi_wake::<S>, ptr.cast())
        };
        if !prev.is_null() {
            assert_eq!(ptr, prev.cast());
        }
        unsafe {
            cabi::wasip3_task_set(task);
        }
    }

    unsafe fn unregister_raw(&mut self, waitable: u32) {
        let ptr = self.inner.as_ptr();
        let task = unsafe { cabi::wasip3_task_set(ptr::null_mut()) };
        assert!(!task.is_null());
        assert!(unsafe { (*task).version } >= cabi::WASIP3_TASK_V1);
        let prev = unsafe { ((*task).waitable_unregister)((*task).ptr, waitable) };
        if !prev.is_null() {
            assert_eq!(ptr, prev.cast());
        }
        unsafe {
            cabi::wasip3_task_set(task);
        }
    }

    /// Registers a completion of `waitable` within the current task's future to:
    ///
    /// * Fill in `completion_status` with the result of a completion event.
    /// * Call `cx.waker().wake()`.
    pub fn register_waker(&mut self, waitable: u32, cx: &mut Context) {
        let inner = self.inner_mut();
        debug_assert!(inner.completion_status.code.is_none());
        inner.completion_status.waker = Some(cx.waker().clone());
        unsafe {
            Self::register_raw(self.inner.as_ptr(), waitable);
        }
    }

    /// Deregisters the corresponding `register_waker` within the current task
    /// for the `waitable` passed here.
    pub fn unregister_waker(&mut self, waitable: u32) {
        unsafe {
            self.unregister_raw(waitable);
        }
    }

    fn apply_code(&mut self, code: u32) -> Poll<S::Result> {
        let inner = self.inner_mut();
        let WaitableOperationState::InProgress(in_progress) =
            mem::replace(&mut inner.state, WaitableOperationState::Done)
        else {
            unreachable!()
        };
        match inner.op.in_progress_update(in_progress, code) {
            Ok(result) => Poll::Ready(result),
            Err(in_progress) => {
                inner.state = WaitableOperationState::InProgress(in_progress);
                Poll::Pending
            }
        }
    }

    /// Polls this operation to see if it has completed yet.
    ///
    /// This is intended to be used within `Future::poll`.
    pub fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<S::Result> {
        let me = unsafe { self.as_mut().get_unchecked_mut() };
        assert!(
            !me.inner().cancelling,
            "cannot poll operation after requesting async cancellation"
        );
        let optional_code = match &mut me.inner_mut().state {
            WaitableOperationState::Start(_) => {
                let WaitableOperationState::Start(s) =
                    mem::replace(&mut me.inner_mut().state, WaitableOperationState::Done)
                else {
                    unreachable!()
                };
                let (code, s) = me.inner_mut().op.start(s);
                me.inner_mut().state = WaitableOperationState::InProgress(s);
                Some(code)
            }
            WaitableOperationState::InProgress(_) => me.inner_mut().completion_status.code.take(),
            WaitableOperationState::Done => panic!("cannot re-poll after operation completes"),
        };

        if let Some(code) = optional_code {
            if let Poll::Ready(result) = me.apply_code(code) {
                return Poll::Ready(result);
            }
        }

        let handle = me.inner_mut().waitable();
        me.register_waker(handle, cx);
        Poll::Pending
    }

    /// Cancels the in-flight operation, if it's still in-flight, and sees what
    /// happened.
    pub fn cancel(mut self: Pin<&mut Self>) -> S::Cancel {
        let me = unsafe { self.as_mut().get_unchecked_mut() };
        match &mut me.inner_mut().state {
            WaitableOperationState::Start(_) => {
                let WaitableOperationState::Start(s) =
                    mem::replace(&mut me.inner_mut().state, WaitableOperationState::Done)
                else {
                    unreachable!()
                };
                return me.inner_mut().op.start_cancelled(s);
            }
            WaitableOperationState::Done => {
                panic!("cannot cancel operation after completing it");
            }
            WaitableOperationState::InProgress(_) => {}
        }

        if let Some(code) = me.inner_mut().completion_status.code.take() {
            if let Poll::Ready(result) = me.apply_code(code) {
                return me.inner_mut().op.result_into_cancel(result);
            }
        } else {
            let waitable = me.inner_mut().waitable();
            me.unregister_waker(waitable);
        }

        let code = me.inner_mut().cancel_code();
        match me.apply_code(code) {
            Poll::Ready(result) => me.inner_mut().op.result_into_cancel(result),
            Poll::Pending => unreachable!(),
        }
    }

    fn poll_cancel(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<S::Cancel> {
        let me = unsafe { self.as_mut().get_unchecked_mut() };
        match &mut me.inner_mut().state {
            WaitableOperationState::Start(_) => {
                let WaitableOperationState::Start(s) =
                    mem::replace(&mut me.inner_mut().state, WaitableOperationState::Done)
                else {
                    unreachable!()
                };
                return Poll::Ready(me.inner_mut().op.start_cancelled(s));
            }
            WaitableOperationState::Done => {
                panic!("cannot cancel operation after completing it");
            }
            WaitableOperationState::InProgress(_) => {}
        }

        if let Some(code) = me.inner_mut().completion_status.code.take() {
            if let Poll::Ready(result) = me.apply_code(code) {
                return Poll::Ready(me.inner_mut().op.result_into_cancel(result));
            }
        }

        if !me.inner().cancelling {
            me.inner_mut().cancelling = true;
            let code = me.inner_mut().cancel_code();
            if let Poll::Ready(result) = me.apply_code(code) {
                return Poll::Ready(me.inner_mut().op.result_into_cancel(result));
            }
        }

        let waitable = me.inner_mut().waitable();
        me.register_waker(waitable, cx);
        Poll::Pending
    }

    pub async fn cancel_async(mut self: Pin<&mut Self>) -> S::Cancel {
        poll_fn(|cx| self.as_mut().poll_cancel(cx)).await
    }

    fn detach(&mut self) {
        let inner = self.inner_mut();
        inner.detached = true;
        inner.completion_status.waker = None;

        match inner.state {
            WaitableOperationState::Start(_) => unsafe {
                self.drop_inner();
            },
            WaitableOperationState::Done => unsafe {
                self.drop_inner();
            },
            WaitableOperationState::InProgress(_) => {
                if let Some(code) = inner.completion_status.code.take() {
                    unsafe {
                        OperationInner::<S>::on_detached_code(self.inner.as_ptr(), code);
                    }
                }
            }
        }
    }

    /// Returns whether or not this operation has completed.
    pub fn is_done(&self) -> bool {
        matches!(self.inner().state, WaitableOperationState::Done)
    }
}

impl<S: WaitableOp> Future for WaitableOperation<S> {
    type Output = S::Result;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<S::Result> {
        self.as_mut().poll_complete(cx)
    }
}

impl<S: WaitableOp> Drop for WaitableOperation<S> {
    fn drop(&mut self) {
        if self.is_done() {
            unsafe {
                self.drop_inner();
            }
            return;
        }

        if S::detach_on_drop() {
            self.detach();
            return;
        }

        let pin = unsafe { Pin::new_unchecked(&mut *self) };
        pin.cancel();
        unsafe {
            self.drop_inner();
        }
    }
}

impl<S: WaitableOp> OperationInner<S> {
    fn waitable(&mut self) -> u32 {
        let op = &mut self.op;
        let in_progress = match &self.state {
            WaitableOperationState::InProgress(in_progress) => in_progress,
            _ => unreachable!(),
        };
        op.in_progress_waitable(in_progress)
    }

    fn cancel_code(&mut self) -> u32 {
        let op = &mut self.op;
        let in_progress = match &mut self.state {
            WaitableOperationState::InProgress(in_progress) => in_progress,
            _ => unreachable!(),
        };
        op.in_progress_cancel(in_progress)
    }

    unsafe fn on_detached_code(ptr: *mut Self, code: u32) {
        let me = unsafe { &mut *ptr };
        let WaitableOperationState::InProgress(in_progress) =
            mem::replace(&mut me.state, WaitableOperationState::Done)
        else {
            unreachable!()
        };
        match me.op.in_progress_update(in_progress, code) {
            Ok(result) => {
                drop(result);
                unsafe {
                    drop(Box::from_raw(ptr));
                }
            }
            Err(in_progress) => {
                me.state = WaitableOperationState::InProgress(in_progress);
                let waitable = me.waitable();
                unsafe {
                    WaitableOperation::<S>::register_raw(ptr, waitable);
                }
            }
        }
    }
}

unsafe extern "C" fn cabi_wake<S: WaitableOp>(ptr: *mut c_void, code: u32) {
    let me = unsafe { &mut *ptr.cast::<OperationInner<S>>() };
    if me.detached {
        unsafe {
            OperationInner::<S>::on_detached_code(ptr.cast(), code);
        }
    } else {
        me.completion_status.code = Some(code);
        if let Some(waker) = me.completion_status.waker.take() {
            waker.wake();
        }
    }
}
