use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use wasm_bindgen::{prelude::*, JsCast};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::web::{
        set_worker_redraw_strategy, EventLoopExtWebSys, WindowAttributesExtWebSys, WindowExtWebSys,
        WorkerRedrawStrategy,
    },
    window::{Window, WindowId},
};

static FRAMES: AtomicU32 = AtomicU32::new(0);
static KEYS: AtomicU32 = AtomicU32::new(0);
static WIDTH: AtomicU32 = AtomicU32::new(0);
static WAKES: AtomicU32 = AtomicU32::new(0);
static EXITED: AtomicU32 = AtomicU32::new(0);
static DEADLINES: AtomicU32 = AtomicU32::new(0);
static RESUMES: AtomicU32 = AtomicU32::new(0);
static SUSPENDS: AtomicU32 = AtomicU32::new(0);
static PROXY: std::sync::OnceLock<winit::event_loop::EventLoopProxy<u32>> =
    std::sync::OnceLock::new();
#[wasm_bindgen]
pub fn prepare(canvas: web_sys::HtmlCanvasElement) -> Result<js_sys::Array, JsValue> {
    console_error_panic_hook::set_once();
    let (id, canvas) = winit::platform::web::prepare_worker(canvas)?;
    Ok([JsValue::from(id), canvas.into()].into_iter().collect())
}
#[wasm_bindgen]
pub fn start(id: u32, canvas: web_sys::OffscreenCanvas) -> Result<(), JsValue> {
    winit::platform::web::attach_worker(id, canvas)?;
    let el = EventLoop::<u32>::with_user_event().build().unwrap();
    PROXY.set(el.create_proxy()).ok().unwrap();
    el.spawn_app(App {
        window: None,
        context: None,
        init: false,
        wait: false,
    });
    Ok(())
}
#[wasm_bindgen]
pub fn wake(value: u32) {
    PROXY.get().unwrap().send_event(value).unwrap();
}
#[wasm_bindgen]
pub fn stats() -> Vec<u32> {
    vec![
        FRAMES.load(Ordering::Relaxed),
        KEYS.load(Ordering::Relaxed),
        WIDTH.load(Ordering::Relaxed),
        WAKES.load(Ordering::Relaxed),
        EXITED.load(Ordering::Relaxed),
        DEADLINES.load(Ordering::Relaxed),
        RESUMES.load(Ordering::Relaxed),
        SUSPENDS.load(Ordering::Relaxed),
    ]
}
struct App {
    window: Option<Window>,
    context: Option<web_sys::OffscreenCanvasRenderingContext2d>,
    init: bool,
    wait: bool,
}
impl ApplicationHandler<u32> for App {
    fn new_events(&mut self, el: &ActiveEventLoop, cause: StartCause) {
        if !self.init {
            assert!(matches!(cause, StartCause::Init));
            self.init = true;
        }
        if let ControlFlow::WaitUntil(deadline) = el.control_flow() {
            assert!(
                !matches!(cause, StartCause::Poll),
                "redraw bypassed WaitUntil"
            );
            if matches!(cause, StartCause::ResumeTimeReached { .. }) {
                assert!(web_time::Instant::now() >= deadline, "early deadline wake");
            }
        }
        if let StartCause::ResumeTimeReached {
            start,
            requested_resume,
        } = cause
        {
            assert!(requested_resume > start);
            DEADLINES.fetch_add(1, Ordering::Relaxed);
            el.set_control_flow(ControlFlow::Wait);
        }
    }
    fn resumed(&mut self, el: &ActiveEventLoop) {
        RESUMES.fetch_add(1, Ordering::Relaxed);
        if self.window.is_some() {
            return;
        }
        let window = el
            .create_window(
                Window::default_attributes()
                    .with_title("worker fixture")
                    .with_inner_size(winit::dpi::LogicalSize::new(320, 240))
                    .with_min_inner_size(winit::dpi::LogicalSize::new(100, 100))
                    .with_max_inner_size(winit::dpi::LogicalSize::new(800, 600))
                    .with_focusable(false)
                    .with_prevent_default(false),
            )
            .unwrap();
        let handle = window.window_handle().unwrap();
        let RawWindowHandle::WebOffscreenCanvas(raw) = handle.as_raw() else {
            panic!("wrong handle")
        };
        let canvas: web_sys::OffscreenCanvas = unsafe { raw.obj.cast::<JsValue>().as_ref() }
            .clone()
            .unchecked_into();
        self.context = Some(canvas.get_context("2d").unwrap().unwrap().unchecked_into());
        window.request_redraw();
        self.window = Some(window);
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                if self.wait {
                    return;
                }
                let n = FRAMES.fetch_add(1, Ordering::Relaxed);
                let context = self.context.as_ref().unwrap();
                context.set_fill_style_str(if n % 2 == 0 { "red" } else { "blue" });
                context.fill_rect(0., 0., 100., 100.);
                self.window.as_ref().unwrap().request_redraw();
            }
            WindowEvent::Resized(size) => {
                WIDTH.store(size.width, Ordering::Relaxed);
                let canvas = self.context.as_ref().unwrap().canvas();
                canvas.set_width(size.width);
                canvas.set_height(size.height);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                KEYS.fetch_add(1, Ordering::Relaxed);
            }
            WindowEvent::ScaleFactorChanged {
                mut inner_size_writer,
                ..
            } => {
                inner_size_writer
                    .request_inner_size(winit::dpi::PhysicalSize::new(420, 240))
                    .unwrap();
            }
            _ => (),
        }
    }
    fn user_event(&mut self, el: &ActiveEventLoop, value: u32) {
        WAKES.fetch_add(1, Ordering::Relaxed);
        match value {
            0 => {
                self.wait = true;
                el.set_control_flow(ControlFlow::Wait);
            }
            1 => {
                self.wait = false;
                self.window.as_ref().unwrap().request_redraw();
            }
            2 => el.exit(),
            4 => {
                assert!(set_worker_redraw_strategy(WorkerRedrawStrategy::Immediate));
            }
            5 => {
                assert!(set_worker_redraw_strategy(
                    WorkerRedrawStrategy::AnimationFrame
                ));
            }
            6 => {
                let window = self.window.as_ref().unwrap();
                window.set_title("updated on worker");
                window.set_min_inner_size(Some(winit::dpi::LogicalSize::new(120, 120)));
                window.set_max_inner_size(Some(winit::dpi::LogicalSize::new(750, 550)));
                window.set_cursor(winit::window::CursorIcon::Crosshair);
                window.set_prevent_default(true);
            }
            7 => self.window.as_ref().unwrap().set_cursor_visible(false),
            8 => {
                let window = self.window.as_ref().unwrap();
                window.set_cursor_visible(true);
                window.set_fullscreen(None);
                window
                    .set_cursor_grab(winit::window::CursorGrabMode::None)
                    .unwrap();
            }
            9 => self
                .window
                .as_ref()
                .unwrap()
                .set_fullscreen(Some(winit::window::Fullscreen::Borderless(None))),
            10 => self
                .window
                .as_ref()
                .unwrap()
                .set_cursor_grab(winit::window::CursorGrabMode::Locked)
                .unwrap(),
            3 => {
                self.wait = true;
                el.set_control_flow(ControlFlow::wait_duration(
                    std::time::Duration::from_millis(40),
                ));
                self.window.as_ref().unwrap().request_redraw();
            }
            _ => (),
        }
    }
    fn suspended(&mut self, _: &ActiveEventLoop) {
        SUSPENDS.fetch_add(1, Ordering::Relaxed);
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        EXITED.store(1, Ordering::Relaxed);
    }
}
