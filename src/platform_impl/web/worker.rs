//! A page-owned window with an application event loop on a dedicated worker.
//!
//! The host and worker instantiate the same atomics-enabled Wasm module and
//! memory. Only the OffscreenCanvas crosses the JavaScript message boundary;
//! events and the existing DOM command dispatcher travel through Rust memory.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;

use atomic_waker::AtomicWaker;
use concurrent_queue::ConcurrentQueue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    DedicatedWorkerGlobalScope, HtmlCanvasElement, MessageChannel, MessagePort, OffscreenCanvas,
};
use web_time::Instant;

use super::event_loop::runner::EventHandler;
use super::main_thread::MainThreadMarker;
use super::r#async::Dispatcher;
use super::{ActiveEventLoop, Window};
use crate::dpi::PhysicalSize;
use crate::event::{Event, StartCause, WindowEvent};
use crate::event_loop::{ControlFlow, DeviceEvents};
use crate::platform::web::{PollStrategy, WaitUntilStrategy, WindowAttributesExtWebSys};
use crate::window::{WindowAttributes, WindowId};

static NEXT_HOST: AtomicU32 = AtomicU32::new(1);
static HOSTS: OnceLock<Mutex<BTreeMap<u32, Arc<Shared>>>> = OnceLock::new();

thread_local! {
    static WORKER: RefCell<Option<Rc<WorkerRunner>>> = const { RefCell::new(None) };
}

struct ForwardedEvent {
    event: Event<()>,
    // Winit's normal scale-change callback owns this only until it returns.
    // Retain it through worker delivery, then apply the application's reply
    // using the existing page-side window dispatcher.
    size_reply: Option<(Arc<Mutex<PhysicalSize<u32>>>, PhysicalSize<u32>)>,
}

pub(crate) struct Shared {
    host: Dispatcher<ActiveEventLoop>,
    window: Window,
    window_id: WindowId,
    events: ConcurrentQueue<ForwardedEvent>,
    notified: AtomicBool,
    user_events: AtomicUsize,
    closed: AtomicBool,
    waker: AtomicWaker,
}

impl Shared {
    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        self.waker.wake();
    }

    pub(crate) fn wake_user(&self) {
        if !self.closed.load(Ordering::Acquire) {
            self.user_events.fetch_add(1, Ordering::Release);
            self.notify();
        }
    }

    fn forward(&self, event: Event<()>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let size_reply = match &event {
            Event::WindowEvent {
                event: WindowEvent::ScaleFactorChanged { inner_size_writer, .. },
                ..
            } => inner_size_writer.new_inner_size.upgrade().map(|size| {
                let initial = *size.lock().unwrap();
                (size, initial)
            }),
            _ => None,
        };
        match event {
            Event::NewEvents(_) | Event::AboutToWait => return,
            _ => (),
        }
        let _ = self.events.push(ForwardedEvent { event, size_reply });
        self.notify();
    }
}

/// Prepare one page canvas for an application hosted in a dedicated worker.
/// Transfer the returned canvas with `postMessage`, and call `attach_worker`
/// there before building the application's event loop. Both instances must
/// use the same Wasm module and shared memory. No rendering context may have
/// been created on the HTML canvas before this call.
pub fn prepare_worker(canvas: HtmlCanvasElement) -> Result<(u32, OffscreenCanvas), JsValue> {
    let marker =
        MainThreadMarker::new().ok_or_else(|| JsValue::from_str("worker host requires a page"))?;
    let target = ActiveEventLoop::new();
    let attributes = WindowAttributes::default().with_canvas(Some(canvas.clone()));
    let window =
        Window::new(&target, attributes).map_err(|error| JsValue::from_str(&error.to_string()))?;
    let offscreen = canvas.transfer_control_to_offscreen()?;
    let window_id = WindowId(window.inner.queue(|window| window.id()));
    let (host, dispatcher) = Dispatcher::new(marker, target.clone()).unwrap();
    let shared = Arc::new(Shared {
        host,
        window,
        window_id,
        events: ConcurrentQueue::unbounded(),
        notified: AtomicBool::new(false),
        user_events: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
        waker: AtomicWaker::new(),
    });
    let id = NEXT_HOST.fetch_add(1, Ordering::Relaxed);
    HOSTS.get_or_init(Mutex::default).lock().unwrap().insert(id, shared.clone());
    let mut initial_resume = true;
    target.run(
        Box::new(move |event| {
            let _keep_alive = &dispatcher;
            if matches!(event, Event::Resumed) && std::mem::take(&mut initial_resume) {
                return;
            }
            shared.forward(event);
        }),
        false,
    );
    Ok((id, offscreen))
}

/// Attach a prepared canvas to its owning worker. This does not transfer
/// JavaScript handles through Rust memory and does not relax the page-only
/// checks on the existing DOM wrappers.
pub fn attach_worker(id: u32, canvas: OffscreenCanvas) -> Result<(), JsValue> {
    if MainThreadMarker::new().is_some()
        || !js_sys::global().is_instance_of::<DedicatedWorkerGlobalScope>()
    {
        return Err(JsValue::from_str("attach_worker requires a dedicated worker"));
    }
    WORKER.with(|slot| {
        if slot.borrow().is_some() {
            return Err(JsValue::from_str("a worker window is already attached"));
        }
        let shared = HOSTS
            .get()
            .and_then(|hosts| hosts.lock().unwrap().remove(&id))
            .ok_or_else(|| JsValue::from_str("unknown or already attached worker host"))?;
        *slot.borrow_mut() = Some(Rc::new(WorkerRunner {
            id,
            shared,
            canvas: Box::new(canvas.into()),
            handler: RefCell::new(None),
            control: Cell::new(ControlFlow::Wait),
            poll: Cell::new(PollStrategy::default()),
            wait: Cell::new(WaitUntilStrategy::default()),
            exit: Cell::new(false),
            running: Cell::new(false),
            window_claimed: Cell::new(false),
            redraw: Cell::new(false),
            redraw_strategy: Cell::new(WorkerRedrawStrategy::AnimationFrame),
            waiting_since: Cell::new(Instant::now()),
            timer: RefCell::new(None),
            frame: RefCell::new(None),
        }));
        Ok(())
    })
}

pub(crate) fn current() -> Option<Rc<WorkerRunner>> {
    WORKER.with(|slot| slot.borrow().clone())
}

/// Whether the current worker has an attached page canvas.
pub fn worker_attached() -> bool {
    current().is_some()
}

/// How an attached worker schedules requested redraws. This controls application
/// pacing, not the browser compositor's presentation mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkerRedrawStrategy {
    /// Follow dedicated-worker animation frames (the default).
    #[default]
    AnimationFrame,
    /// Yield through a message task without waiting for a display refresh.
    /// Browsers may still throttle or suspend background workers.
    Immediate,
}

/// Change redraw pacing on the owning worker. Returns false outside an attached
/// worker. A pending redraw is rescheduled, so changes also wake a paused rAF.
pub fn set_worker_redraw_strategy(strategy: WorkerRedrawStrategy) -> bool {
    let Some(runner) = current() else { return false };
    if runner.redraw_strategy.replace(strategy) != strategy {
        let pending = runner.frame.borrow_mut().take().is_some();
        if pending {
            runner.request_redraw();
        }
    }
    true
}

#[cfg(feature = "rwh_06")]
pub(crate) fn window_handle(id: u32) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
    let runner =
        current().filter(|runner| runner.id == id).ok_or(rwh_06::HandleError::Unavailable)?;
    let pointer = std::ptr::NonNull::from(runner.canvas.as_ref()).cast();
    Ok(rwh_06::WebOffscreenCanvasWindowHandle::new(pointer).into())
}

pub(crate) struct WorkerRunner {
    id: u32,
    pub(crate) shared: Arc<Shared>,
    // Kept in the owning worker's TLS for its lifetime, including after loop
    // exit: a surface retained by the application may outlive the event loop.
    canvas: Box<JsValue>,
    handler: RefCell<Option<Box<EventHandler>>>,
    control: Cell<ControlFlow>,
    poll: Cell<PollStrategy>,
    wait: Cell<WaitUntilStrategy>,
    exit: Cell<bool>,
    running: Cell<bool>,
    window_claimed: Cell<bool>,
    redraw: Cell<bool>,
    redraw_strategy: Cell<WorkerRedrawStrategy>,
    waiting_since: Cell<Instant>,
    timer: RefCell<Option<Callback>>,
    frame: RefCell<Option<Callback>>,
}

impl WorkerRunner {
    pub(crate) fn host_id(&self) -> u32 {
        self.id
    }
    pub(crate) fn create_window(
        &self,
        attributes: WindowAttributes,
    ) -> Result<Window, crate::error::OsError> {
        if self.window_claimed.replace(true) {
            return Err(os_error!(super::OsError(
                "worker mode supports one pre-created canvas".into()
            )));
        }
        self.shared.window.maybe_queue_on_main(move |window| {
            window.apply_worker_attributes(attributes);
        });
        Ok(Window { inner: self.shared.window.inner.clone(), worker_id: Some(self.id) })
    }

    pub(crate) fn run(self: &Rc<Self>, handler: Box<EventHandler>) {
        assert!(self.handler.borrow().is_none(), "worker event loop already running");
        *self.handler.borrow_mut() = Some(handler);
        self.cycle(StartCause::Init);
        let runner = Rc::downgrade(self);
        let shared = self.shared.clone();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                poll_fn(|cx| {
                    shared.waker.register(cx.waker());
                    if shared.notified.swap(false, Ordering::AcqRel)
                        || shared.closed.load(Ordering::Acquire)
                    {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                if shared.closed.load(Ordering::Acquire) {
                    break;
                }
                let Some(runner) = runner.upgrade() else { break };
                runner.cycle(runner.wake_cause());
            }
        });
    }

    fn emit(&self, event: Event<()>) {
        if let Some(handler) = self.handler.borrow_mut().as_mut() {
            handler(event);
        }
    }

    fn cycle(self: &Rc<Self>, cause: StartCause) {
        if self.shared.closed.load(Ordering::Acquire) || self.running.replace(true) {
            return;
        }
        self.timer.borrow_mut().take();
        self.emit(Event::NewEvents(cause));
        if matches!(cause, StartCause::Init) {
            self.emit(Event::Resumed);
        }
        while let Ok(forwarded) = self.shared.events.pop() {
            if matches!(forwarded.event, Event::LoopExiting) {
                self.exit.set(true);
                break;
            }
            self.emit(forwarded.event);
            if let Some((size, initial)) = forwarded.size_reply {
                let reply = *size.lock().unwrap();
                if reply != initial {
                    self.shared.window.maybe_queue_on_main(move |window| {
                        window.request_inner_size(reply.into());
                    });
                }
            }
        }
        for _ in 0..self.shared.user_events.swap(0, Ordering::AcqRel) {
            self.emit(Event::UserEvent(()));
        }
        if self.redraw.replace(false) {
            let window_id = self.shared.window_id;
            self.emit(Event::WindowEvent { window_id, event: WindowEvent::RedrawRequested });
        }
        self.emit(Event::AboutToWait);
        self.running.set(false);
        if self.exit.get() {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.notify();
            self.frame.borrow_mut().take();
            self.emit(Event::LoopExiting);
            self.handler.borrow_mut().take();
            self.shared.host.dispatch(|host| {
                host.exit();
                host.runner.poll();
            });
            return;
        }
        let start = Instant::now();
        self.waiting_since.set(start);
        let delay = match self.control.get() {
            ControlFlow::Wait => return,
            ControlFlow::Poll => 0,
            ControlFlow::WaitUntil(deadline) => {
                let micros = deadline.saturating_duration_since(start).as_micros();
                ((micros + 999) / 1000).min(i32::MAX as u128) as i32
            },
        };
        let weak = Rc::downgrade(self);
        let callback = move || {
            if let Some(runner) = weak.upgrade() {
                runner.cycle(runner.wake_cause());
            }
        };
        *self.timer.borrow_mut() = Some(if delay == 0 {
            Callback::immediate(callback)
        } else {
            Callback::timeout(delay, callback)
        });
    }

    fn wake_cause(&self) -> StartCause {
        let start = self.waiting_since.get();
        match self.control.get() {
            ControlFlow::Poll => StartCause::Poll,
            ControlFlow::Wait => StartCause::WaitCancelled { start, requested_resume: None },
            ControlFlow::WaitUntil(deadline) if deadline <= Instant::now() => {
                StartCause::ResumeTimeReached { start, requested_resume: deadline }
            },
            ControlFlow::WaitUntil(deadline) => {
                StartCause::WaitCancelled { start, requested_resume: Some(deadline) }
            },
        }
    }

    pub(crate) fn request_redraw(self: &Rc<Self>) {
        if self.frame.borrow().is_some() || self.exit.get() {
            return;
        }
        let weak = Rc::downgrade(self);
        let callback = move || {
            if let Some(runner) = weak.upgrade() {
                runner.frame.borrow_mut().take();
                runner.redraw.set(true);
                runner.cycle(runner.wake_cause());
            }
        };
        *self.frame.borrow_mut() = Some(match self.redraw_strategy.get() {
            WorkerRedrawStrategy::AnimationFrame => Callback::frame(callback),
            WorkerRedrawStrategy::Immediate => Callback::immediate(callback),
        });
    }

    pub(crate) fn control_flow(&self) -> ControlFlow {
        self.control.get()
    }
    pub(crate) fn set_control_flow(&self, flow: ControlFlow) {
        self.control.set(flow);
    }
    pub(crate) fn exit(&self) {
        self.exit.set(true);
        self.shared.notify();
    }
    pub(crate) fn exiting(&self) -> bool {
        self.exit.get()
    }
    pub(crate) fn poll_strategy(&self) -> PollStrategy {
        self.poll.get()
    }
    pub(crate) fn set_poll_strategy(&self, strategy: PollStrategy) {
        self.poll.set(strategy);
    }
    pub(crate) fn wait_until_strategy(&self) -> WaitUntilStrategy {
        self.wait.get()
    }
    pub(crate) fn set_wait_until_strategy(&self, strategy: WaitUntilStrategy) {
        self.wait.set(strategy);
    }
    pub(crate) fn listen_device_events(&self, allowed: DeviceEvents) {
        self.shared.host.dispatch(move |host| host.listen_device_events(allowed));
    }
    pub(crate) fn system_theme(&self) -> Option<crate::window::Theme> {
        self.shared.host.queue(|host| host.system_theme())
    }
    pub(crate) fn create_custom_cursor(
        &self,
        source: crate::window::CustomCursorSource,
    ) -> crate::window::CustomCursor {
        self.shared.host.queue(move |host| host.create_custom_cursor(source))
    }
    pub(crate) fn create_custom_cursor_async(
        &self,
        source: crate::window::CustomCursorSource,
    ) -> crate::platform::web::CustomCursorFuture {
        self.shared.host.queue(move |host| host.create_custom_cursor_async(source))
    }
}

struct Callback {
    global: DedicatedWorkerGlobalScope,
    handle: i32,
    frame: bool,
    port: Option<MessagePort>,
    _closure: Closure<dyn FnMut()>,
}

impl Callback {
    fn immediate(callback: impl FnMut() + 'static) -> Self {
        let global = js_sys::global().unchecked_into();
        let channel = MessageChannel::new().expect("worker message channel");
        let port = channel.port1();
        let closure = Closure::new(callback);
        port.set_onmessage(Some(closure.as_ref().unchecked_ref()));
        channel.port2().post_message(&JsValue::UNDEFINED).expect("worker message task");
        Self { global, handle: 0, frame: false, port: Some(port), _closure: closure }
    }

    fn timeout(delay: i32, callback: impl FnMut() + 'static) -> Self {
        let global: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        let closure = Closure::new(callback);
        let handle = global
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                closure.as_ref().unchecked_ref(),
                delay,
            )
            .expect("worker timer");
        Self { global, handle, frame: false, port: None, _closure: closure }
    }

    fn frame(callback: impl FnMut() + 'static) -> Self {
        let global: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        let closure = Closure::new(callback);
        match global.request_animation_frame(closure.as_ref().unchecked_ref()) {
            Ok(handle) => Self { global, handle, frame: true, port: None, _closure: closure },
            Err(_) => {
                let handle = global
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        closure.as_ref().unchecked_ref(),
                        16,
                    )
                    .expect("worker frame timer");
                Self { global, handle, frame: false, port: None, _closure: closure }
            },
        }
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(port) = &self.port {
            port.set_onmessage(None);
            port.close();
        } else if self.frame {
            let _ = self.global.cancel_animation_frame(self.handle);
        } else {
            self.global.clear_timeout_with_handle(self.handle);
        }
    }
}
