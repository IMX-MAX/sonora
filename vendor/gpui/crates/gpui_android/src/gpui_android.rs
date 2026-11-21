#![cfg(target_os = "android")]

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use android_activity::input::{InputEvent, KeyAction, Keycode, MotionAction};
use android_activity::{AndroidAppWaker, InputStatus, MainEvent, PollEvent};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, Bounds,
    Capslock, ClipboardItem, CursorStyle, Decorations, DevicePixels, DispatchEventResult,
    DisplayId, DummyKeyboardMapper, Edges, ForegroundExecutor, FrameRequestSource, GpuSpecs,
    HeadlessAtlas, KeyDownEvent, KeyUpEvent, Keymap, Keystroke, LongPressEvent, Menu, MenuItem,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Platform,
    PlatformAtlas, PlatformDispatcher, PlatformDisplay, PlatformInput, PlatformInputHandler,
    PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Point,
    Priority, PriorityQueueReceiver, PriorityQueueSender, PromptButton, PromptLevel,
    RequestFrameOptions, RunnableVariant, Scene, ScrollDelta, ScrollWheelEvent, Size, Task,
    TextInputStateChange, ThermalState, TouchPhase, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, WindowControls, WindowInsets, WindowParams, WindowVisibility,
    point, px, size,
};
use gpui_wgpu::{CosmicTextSystem, GpuContext, WgpuRenderer, WgpuSurfaceConfig};
use ndk::native_window::NativeWindow;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use uuid::Uuid;

pub use android_activity::AndroidApp;

static ANDROID_APP: Mutex<Option<AndroidApp>> = Mutex::new(None);

/// Registers the [`AndroidApp`] handle passed to `android_main`. Must be called before
/// constructing [`AndroidPlatform`].
pub fn set_android_app(app: AndroidApp) {
    *ANDROID_APP.lock().unwrap_or_else(|e| e.into_inner()) = Some(app);
}

fn android_app() -> AndroidApp {
    ANDROID_APP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .expect("gpui_android::set_android_app must be called in android_main")
        .clone()
}

const TOUCH_SLOP_PX: f32 = 10.0;
const LONG_PRESS_DURATION: Duration = Duration::from_millis(450);

struct ScheduledTimer {
    at: Instant,
    seq: u64,
    runnable: RunnableVariant,
}

impl PartialEq for ScheduledTimer {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.seq == other.seq
    }
}

impl Eq for ScheduledTimer {}

impl PartialOrd for ScheduledTimer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledTimer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

struct TimerState {
    heap: BinaryHeap<Reverse<ScheduledTimer>>,
    next_seq: u64,
    shutdown: bool,
}

struct AndroidDispatcher {
    main_queue: Arc<Mutex<VecDeque<(Priority, RunnableVariant)>>>,
    waker: AndroidAppWaker,
    background_sender: PriorityQueueSender<RunnableVariant>,
    _background_threads: Vec<thread::JoinHandle<()>>,
    timer_state: Arc<(Mutex<TimerState>, Condvar)>,
    main_thread_id: thread::ThreadId,
}

impl AndroidDispatcher {
    fn new(waker: AndroidAppWaker) -> Self {
        let (background_sender, background_receiver) = PriorityQueueReceiver::new();
        let thread_count = std::thread::available_parallelism().map_or(2, |n| n.get().max(2));

        let mut background_threads = Vec::with_capacity(thread_count + 1);
        for i in 0..thread_count {
            let receiver: PriorityQueueReceiver<RunnableVariant> = background_receiver.clone();
            let handle = thread::Builder::new()
                .name(format!("Worker-{i}"))
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    for runnable in receiver.iter() {
                        runnable.run();
                    }
                })
                .expect("failed to spawn worker thread");
            background_threads.push(handle);
        }

        let timer_state = Arc::new((
            Mutex::new(TimerState {
                heap: BinaryHeap::new(),
                next_seq: 0,
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let timer_thread = {
            let timer_state = Arc::clone(&timer_state);
            thread::Builder::new()
                .name("Timer".to_owned())
                .spawn(move || {
                    let (lock, condvar) = &*timer_state;
                    let mut state = lock.lock().unwrap();
                    loop {
                        if state.shutdown {
                            break;
                        }
                        let Some(Reverse(next)) = state.heap.peek() else {
                            state = condvar.wait(state).unwrap();
                            continue;
                        };
                        let now = Instant::now();
                        if next.at <= now {
                            let Reverse(expired) = state.heap.pop().unwrap();
                            drop(state);
                            expired.runnable.run();
                            state = lock.lock().unwrap();
                        } else {
                            let wait_for = next.at - now;
                            let (guard, _) = condvar.wait_timeout(state, wait_for).unwrap();
                            state = guard;
                        }
                    }
                })
                .expect("failed to spawn timer thread")
        };
        background_threads.push(timer_thread);

        Self {
            main_queue: Arc::new(Mutex::new(VecDeque::new())),
            waker,
            background_sender,
            _background_threads: background_threads,
            timer_state,
            main_thread_id: thread::current().id(),
        }
    }

    fn drain_main(&self) {
        loop {
            let next = self.main_queue.lock().unwrap().pop_front();
            let Some((_, runnable)) = next else {
                break;
            };
            runnable.run();
        }
    }
}

impl Drop for AndroidDispatcher {
    fn drop(&mut self) {
        let (lock, condvar) = &*self.timer_state;
        lock.lock().unwrap().shutdown = true;
        condvar.notify_all();
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        self.background_sender
            .send(priority, runnable)
            .unwrap_or_else(|_| panic!("background worker queue closed"));
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.main_queue
            .lock()
            .unwrap()
            .push_back((priority, runnable));
        self.waker.wake();
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let (lock, condvar) = &*self.timer_state;
        let mut state = lock.lock().unwrap();
        let seq = state.next_seq;
        state.next_seq = seq.wrapping_add(1);
        state.heap.push(Reverse(ScheduledTimer {
            at: Instant::now() + duration,
            seq,
            runnable,
        }));
        condvar.notify_one();
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        thread::spawn(f);
    }
}

#[derive(Debug, Clone)]
struct AndroidDisplay {
    size: Size<Pixels>,
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::nil())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(Point::default(), self.size)
    }

    fn refresh_interval(&self) -> Option<Duration> {
        Some(Duration::from_nanos(16_666_667))
    }
}

struct AndroidKeyboardLayout;

impl PlatformKeyboardLayout for AndroidKeyboardLayout {
    fn id(&self) -> &str {
        "android"
    }

    fn name(&self) -> &str {
        "Android"
    }
}

#[derive(Default)]
struct WindowCallbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    visibility_change: Option<Box<dyn FnMut(WindowVisibility)>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
    insets_changed: Option<Box<dyn FnMut(WindowInsets)>>,
    back_handler: Option<Box<dyn FnMut()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
struct FlingState {
    pos: Point<Pixels>,
    velocity: Point<f32>,
    last_tick: Instant,
}

const FLING_MIN_SPEED: f32 = 120.0;
const FLING_MAX_SPEED: f32 = 4500.0;
const FLING_STOP_SPEED: f32 = 24.0;
const FLING_FRICTION: f32 = 4.5;

struct TouchTracker {
    active_pointer: Option<i32>,
    down_pos: Point<Pixels>,
    last_pos: Point<Pixels>,
    down_at: Instant,
    last_move_at: Instant,
    velocity: Point<f32>,
    scroll_axis: Option<ScrollAxis>,
    scrolling: bool,
    long_pressed: bool,
    /// Whether a view claimed the long press, so moves and the lift go to it as a `LongPress`.
    long_press_claimed: bool,
    fling: Option<FlingState>,
}

impl Default for TouchTracker {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            active_pointer: None,
            down_pos: Point::default(),
            last_pos: Point::default(),
            down_at: now,
            last_move_at: now,
            velocity: Point::default(),
            scroll_axis: None,
            scrolling: false,
            long_pressed: false,
            long_press_claimed: false,
            fling: None,
        }
    }
}

#[derive(Clone, Debug)]
struct NativeWindowTarget(NativeWindow);

impl HasWindowHandle for NativeWindowTarget {
    fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
        self.0.window_handle()
    }
}

impl HasDisplayHandle for NativeWindowTarget {
    fn display_handle(&self) -> std::result::Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

struct WindowInner {
    handle: AnyWindowHandle,
    native_window: Option<NativeWindowTarget>,
    gpu_context: GpuContext,
    renderer: Option<WgpuRenderer>,
    fallback_atlas: Arc<HeadlessAtlas>,
    bounds: Bounds<Pixels>,
    scale: f32,
    insets: WindowInsets,
    active: bool,
    visible: bool,
    title: String,
    input_handler: Option<PlatformInputHandler>,
    back_enabled: bool,
    callbacks: WindowCallbacks,
    touch: TouchTracker,
    frame_requested: Cell<bool>,
    waker: AndroidAppWaker,
}

impl WindowInner {
    fn ensure_renderer(&mut self) {
        let Some(native_window) = self.native_window.as_ref() else {
            return;
        };
        let dev_width = native_window.0.width().max(1);
        let dev_height = native_window.0.height().max(1);
        let make_config = || WgpuSurfaceConfig {
            size: size(DevicePixels(dev_width), DevicePixels(dev_height)),
            transparent: false,
            preferred_present_mode: None,
        };
        let instance = self
            .gpu_context
            .borrow()
            .as_ref()
            .map(|ctx| ctx.instance.clone());
        let replaced = if let (Some(renderer), Some(instance)) =
            (self.renderer.as_mut(), instance.as_ref())
        {
            if renderer.device_lost() {
                match renderer.recover(native_window) {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("gpui_android: failed to recover renderer: {error:#}");
                        false
                    }
                }
            } else {
                match renderer.replace_surface(native_window, make_config(), instance) {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("gpui_android: failed to replace surface: {error:#}");
                        match renderer.recover(native_window) {
                            Ok(()) => true,
                            Err(recover_error) => {
                                log::error!(
                                    "gpui_android: failed to recover after replace_surface: {recover_error:#}"
                                );
                                false
                            }
                        }
                    }
                }
            }
        } else {
            false
        };
        if !replaced {
            self.renderer = None;
            if self
                .gpu_context
                .borrow()
                .as_ref()
                .is_some_and(|ctx| ctx.device_lost())
            {
                *self.gpu_context.borrow_mut() = None;
            }
            match WgpuRenderer::new(self.gpu_context.clone(), native_window, make_config(), None) {
                Ok(renderer) => {
                    self.renderer = Some(renderer);
                }
                Err(error) => {
                    log::error!("gpui_android: failed to create WgpuRenderer: {error:#}");
                    if self.gpu_context.borrow().is_some() {
                        *self.gpu_context.borrow_mut() = None;
                        match WgpuRenderer::new(
                            self.gpu_context.clone(),
                            native_window,
                            make_config(),
                            None,
                        ) {
                            Ok(renderer) => self.renderer = Some(renderer),
                            Err(retry_error) => {
                                log::error!(
                                    "gpui_android: retry creating WgpuRenderer failed: {retry_error:#}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn unconfigure_surface(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.unconfigure_surface();
        }
        self.native_window = None;
    }

    fn update_size_from_native(&mut self, scale: f32) -> bool {
        let Some(native_window) = self.native_window.as_ref() else {
            return false;
        };
        let dev_w = native_window.0.width().max(1);
        let dev_h = native_window.0.height().max(1);
        let logical = size(px(dev_w as f32 / scale), px(dev_h as f32 / scale));
        let changed = self.bounds.size != logical || (self.scale - scale).abs() > f32::EPSILON;
        self.scale = scale;
        self.bounds = Bounds::new(Point::default(), logical);
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.update_drawable_size(size(DevicePixels(dev_w), DevicePixels(dev_h)));
        }
        changed
    }
}

#[derive(Clone)]
struct AndroidWindow(Rc<RefCell<WindowInner>>);

impl HasWindowHandle for AndroidWindow {
    fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
        let inner = self.0.borrow();
        let Some(native) = inner.native_window.as_ref() else {
            return Err(HandleError::Unavailable);
        };
        let handle = native.window_handle()?;
        // SAFETY: `NativeWindow` is backed by ANativeWindow kept alive by `WindowInner`.
        Ok(unsafe { WindowHandle::borrow_raw(handle.as_raw()) })
    }
}

impl HasDisplayHandle for AndroidWindow {
    fn display_handle(&self) -> std::result::Result<DisplayHandle<'_>, HandleError> {
        let inner = self.0.borrow();
        let Some(native) = inner.native_window.as_ref() else {
            return Err(HandleError::Unavailable);
        };
        let handle = native.display_handle()?;
        Ok(unsafe { DisplayHandle::borrow_raw(handle.as_raw()) })
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.0.borrow().scale
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(AndroidDisplay {
            size: self.content_size(),
        }))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.borrow().touch.last_pos
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        self.0.borrow().active
    }

    fn visibility(&self) -> WindowVisibility {
        match self.0.borrow().visible {
            true => WindowVisibility::Visible,
            false => WindowVisibility::Hidden,
        }
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.0.borrow_mut().title = title.to_owned();
    }

    fn get_title(&self) -> String {
        self.0.borrow().title.clone()
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {}

    fn minimize(&self) {
        call_activity_void(&android_app(), jni::jni_str!("leaveApp"));
    }

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.borrow_mut().callbacks.request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.borrow_mut().callbacks.input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.borrow_mut().callbacks.active_status_change = Some(callback);
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.0.borrow_mut().callbacks.visibility_change = Some(callback);
    }

    fn on_hover_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.borrow_mut().callbacks.resize = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_display_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.borrow_mut().callbacks.should_close = Some(callback);
    }

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.borrow_mut().callbacks.close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.borrow_mut().callbacks.appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut inner = self.0.borrow_mut();
        if !inner.visible || inner.native_window.is_none() {
            return;
        }
        if inner.renderer.is_none() {
            inner.ensure_renderer();
        }
        let native = inner.native_window.clone();
        if let Some(renderer) = inner.renderer.as_mut() {
            if renderer.device_lost() {
                if let Some(native) = native.as_ref() {
                    if let Err(error) = renderer.recover(native) {
                        log::error!("gpui_android: failed to recover renderer: {error:#}");
                        inner.renderer = None;
                        *inner.gpu_context.borrow_mut() = None;
                        inner.ensure_renderer();
                        inner.frame_requested.set(true);
                        return;
                    }
                } else {
                    return;
                }
            }
            if !renderer.draw(scene) {
                inner.frame_requested.set(true);
            }
        }
    }

    fn schedule_frame(&self) {
        let inner = self.0.borrow();
        inner.frame_requested.set(true);
        inner.waker.wake();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        let inner = self.0.borrow();
        match inner.renderer.as_ref() {
            Some(renderer) => renderer.sprite_atlas().clone(),
            None => inner.fallback_atlas.clone(),
        }
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn window_decorations(&self) -> Decorations {
        Decorations::Server
    }

    fn window_controls(&self) -> WindowControls {
        WindowControls::default()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.0
            .borrow()
            .renderer
            .as_ref()
            .and_then(|renderer| renderer.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn prepare_frame(&self) -> bool {
        let app = android_app();
        let scale = self.0.borrow().scale;
        let next = query_insets(&app, scale);
        let mut inner = self.0.borrow_mut();
        if inner.insets != next {
            inner.insets = next;
            true
        } else {
            false
        }
    }

    fn insets(&self) -> WindowInsets {
        self.0.borrow().insets.clone()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.0.borrow_mut().callbacks.insets_changed = Some(callback);
    }

    fn set_back_handler(&self, callback: Box<dyn FnMut()>) {
        self.0.borrow_mut().callbacks.back_handler = Some(callback);
    }

    fn set_back_enabled(&self, enabled: bool) {
        self.0.borrow_mut().back_enabled = enabled;
    }

    fn show_soft_keyboard(&self) {
        call_activity_void(&android_app(), jni::jni_str!("showSoftKeyboard"));
    }

    fn hide_soft_keyboard(&self) {
        call_activity_void(&android_app(), jni::jni_str!("hideSoftKeyboard"));
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        match change {
            TextInputStateChange::FocusGained => self.show_soft_keyboard(),
            TextInputStateChange::FocusLost => self.hide_soft_keyboard(),
            _ => {}
        }
    }
}

#[derive(Default)]
struct PlatformCallbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit: Option<Box<dyn FnMut() -> bool>>,
    reopen: Option<Box<dyn FnMut()>>,
    lifecycle: Option<Box<dyn FnMut(AppLifecyclePhase)>>,
}

struct PlatformState {
    app: AndroidApp,
    window: Option<AndroidWindow>,
    gpu_context: GpuContext,
    scale: f32,
    running: bool,
    launched: bool,
    callbacks: PlatformCallbacks,
}

pub struct AndroidPlatform {
    state: Rc<RefCell<PlatformState>>,
    dispatcher: Arc<AndroidDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
}

impl AndroidPlatform {
    pub fn new() -> Self {
        let app = android_app();
        let waker = app.create_waker();
        let dispatcher = Arc::new(AndroidDispatcher::new(waker));
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = Arc::new(CosmicTextSystem::new("Roboto"));
        let scale = density_scale(&app);

        Self {
            state: Rc::new(RefCell::new(PlatformState {
                app,
                window: None,
                gpu_context: Rc::new(RefCell::new(None)),
                scale,
                running: true,
                launched: false,
                callbacks: PlatformCallbacks::default(),
            })),
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
        }
    }

    fn dispatch_input(&self, input: PlatformInput) {
        self.dispatch_input_result(input);
    }

    fn dispatch_input_result(&self, input: PlatformInput) -> Option<DispatchEventResult> {
        let window = self.state.borrow().window.clone()?;
        let callback = window.0.borrow_mut().callbacks.input.take();
        let mut callback = callback?;
        let result = callback(input);
        let mut inner = window.0.borrow_mut();
        if inner.callbacks.input.is_none() {
            inner.callbacks.input = Some(callback);
        }
        Some(result)
    }

    /// Offers a long press to the views, and returns whether one claimed it.
    fn dispatch_long_press(
        &self,
        phase: TouchPhase,
        start_position: Point<Pixels>,
        position: Point<Pixels>,
    ) -> bool {
        self.dispatch_input_result(PlatformInput::LongPress(LongPressEvent {
            phase,
            start_position,
            position,
        }))
        .is_some_and(|result| result.default_prevented)
    }

    fn handle_input_events(&self) {
        let app = self.state.borrow().app.clone();
        let scale = self.state.borrow().scale;
        let Ok(mut iter) = app.input_events_iter() else {
            return;
        };
        loop {
            let handled = iter.next(|event| match event {
                InputEvent::MotionEvent(motion) => {
                    self.handle_motion_event(&motion, scale);
                    InputStatus::Handled
                }
                InputEvent::KeyEvent(key) => {
                    // The system back button and gesture reach the views as Escape, the key
                    // that already closes whatever is open, and the app decides what an Escape
                    // nothing took does. Both halves are claimed so the system never finishes
                    // the activity itself.
                    if key.key_code() == Keycode::Back {
                        if key.action() == KeyAction::Down {
                            self.dispatch_key_press("escape", None);
                        }
                        return InputStatus::Handled;
                    }
                    InputStatus::Unhandled
                }
                _ => InputStatus::Unhandled,
            });
            if !handled {
                break;
            }
        }
    }

    fn check_long_press(&self) {
        let Some(window) = self.state.borrow().window.clone() else {
            return;
        };
        let pos = {
            let mut inner = window.0.borrow_mut();
            if inner.touch.active_pointer.is_none()
                || inner.touch.scrolling
                || inner.touch.long_pressed
                || inner.touch.down_at.elapsed() < LONG_PRESS_DURATION
            {
                return;
            }
            inner.touch.long_pressed = true;
            (inner.touch.down_pos, inner.touch.last_pos)
        };
        let (start, pos) = pos;
        // A view that claims the long press (by preventing default) follows the finger until it
        // lifts; otherwise the long press opens the context menu as a right click.
        if self.dispatch_long_press(TouchPhase::Started, start, pos) {
            window.0.borrow_mut().touch.long_press_claimed = true;
            return;
        }
        let offscreen = point(px(-10_000.0), px(-10_000.0));
        self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
            position: pos,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }));
        self.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Right,
            position: pos,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }));
        self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Right,
            position: pos,
            modifiers: Modifiers::default(),
            click_count: 1,
        }));
        self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
            position: offscreen,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }));
    }

    fn drain_ime_input(&self) {
        let app = self.state.borrow().app.clone();
        let events = drain_activity_input_events(&app);
        if events.is_empty() {
            return;
        }
        let window = self.state.borrow().window.clone();
        let Some(window) = window else {
            return;
        };
        for event in events {
            if let Some(text) = event.strip_prefix("T:") {
                let handler = window.0.borrow_mut().input_handler.take();
                if let Some(mut handler) = handler {
                    handler.replace_text_in_range(None, text);
                    let mut inner = window.0.borrow_mut();
                    if inner.input_handler.is_none() {
                        inner.input_handler = Some(handler);
                    }
                    inner.frame_requested.set(true);
                } else {
                    for ch in text.chars() {
                        let s = ch.to_string();
                        self.dispatch_key_press(&s, Some(&s));
                    }
                }
            } else if let Some(count) = event.strip_prefix("B:") {
                let n = count.parse::<usize>().unwrap_or(1).min(64);
                for _ in 0..n {
                    self.dispatch_key_press("backspace", None);
                }
            } else if event == "I:HIDDEN" {
                // The keyboard went away. A touch it cut short never sends its lift, so start
                // the next one from a clean slate rather than waiting on it.
                let offscreen = point(px(-10_000.0), px(-10_000.0));
                {
                    let mut inner = window.0.borrow_mut();
                    inner.touch = TouchTracker::default();
                    inner.touch.last_pos = offscreen;
                    inner.frame_requested.set(true);
                }
                self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                    position: offscreen,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }));
            } else if let Some(key) = event.strip_prefix("K:") {
                let mapped = match key {
                    "BACKSPACE" => Some("backspace"),
                    "ENTER" => Some("enter"),
                    "TAB" => Some("tab"),
                    "LEFT" => Some("left"),
                    "RIGHT" => Some("right"),
                    "UP" => Some("up"),
                    "DOWN" => Some("down"),
                    _ => None,
                };
                if let Some(name) = mapped {
                    self.dispatch_key_press(name, None);
                }
            }
        }
    }

    fn dispatch_key_press(&self, key: &str, key_char: Option<&str>) {
        let keystroke = Keystroke {
            modifiers: Modifiers::default(),
            key: key.to_owned(),
            key_char: key_char.map(str::to_owned),
        };
        self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: key_char.is_some(),
        }));
        self.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
    }

    fn handle_motion_event(&self, motion: &android_activity::input::MotionEvent, scale: f32) {
        let Some(window) = self.state.borrow().window.clone() else {
            return;
        };
        let pointer_idx = motion.pointer_index();
        let Some(pointer) = motion.pointers().nth(pointer_idx) else {
            return;
        };
        let pointer_id = pointer.pointer_id();
        let pos = point(px(pointer.x() / scale), px(pointer.y() / scale));

        match motion.action() {
            action @ (MotionAction::Down | MotionAction::PointerDown) => {
                let cancelled_fling = {
                    let mut inner = window.0.borrow_mut();
                    if matches!(action, MotionAction::PointerDown)
                        && inner.touch.active_pointer.is_some()
                    {
                        return;
                    }
                    let fling = if matches!(action, MotionAction::Down) {
                        inner.touch.fling.take()
                    } else {
                        None
                    };
                    let now = Instant::now();
                    inner.touch.active_pointer = Some(pointer_id);
                    inner.touch.down_pos = pos;
                    inner.touch.last_pos = pos;
                    inner.touch.down_at = now;
                    inner.touch.last_move_at = now;
                    inner.touch.velocity = Point::default();
                    inner.touch.scroll_axis = None;
                    inner.touch.scrolling = false;
                    inner.touch.long_pressed = false;
                    inner.touch.long_press_claimed = false;
                    fling
                };
                if let Some(fling) = cancelled_fling {
                    self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: fling.pos,
                        delta: ScrollDelta::Pixels(Point::default()),
                        modifiers: Modifiers::default(),
                        touch_phase: TouchPhase::Ended,
                    }));
                }
            }
            MotionAction::Move => {
                // Find the active pointer in the motion event
                let active_id = window.0.borrow().touch.active_pointer;
                let Some(active_id) = active_id else {
                    return;
                };
                let Some(p) = motion.pointers().find(|p| p.pointer_id() == active_id) else {
                    return;
                };
                let pos = point(px(p.x() / scale), px(p.y() / scale));
                let (entered_scroll, delta, prev_scrolling, long_pressed) = {
                    let mut inner = window.0.borrow_mut();
                    if inner.touch.long_pressed {
                        inner.touch.last_pos = pos;
                        (false, Point::default(), false, true)
                    } else {
                        let now = Instant::now();
                        let dt = now
                            .duration_since(inner.touch.last_move_at)
                            .as_secs_f32()
                            .clamp(0.001, 0.1);
                        let dx = pos.x - inner.touch.last_pos.x;
                        let dy = pos.y - inner.touch.last_pos.y;
                        let total_dx = f32::from(pos.x) - f32::from(inner.touch.down_pos.x);
                        let total_dy = f32::from(pos.y) - f32::from(inner.touch.down_pos.y);
                        let dist = (total_dx * total_dx + total_dy * total_dy).sqrt();
                        let was_scrolling = inner.touch.scrolling;
                        if !was_scrolling && dist > TOUCH_SLOP_PX {
                            inner.touch.scrolling = true;
                            inner.touch.scroll_axis = Some(if total_dx.abs() > total_dy.abs() {
                                ScrollAxis::Horizontal
                            } else {
                                ScrollAxis::Vertical
                            });
                        }
                        let inst_vx = (f32::from(dx) / dt).clamp(-FLING_MAX_SPEED, FLING_MAX_SPEED);
                        let inst_vy = (f32::from(dy) / dt).clamp(-FLING_MAX_SPEED, FLING_MAX_SPEED);
                        inner.touch.velocity = point(
                            inner.touch.velocity.x * 0.4 + inst_vx * 0.6,
                            inner.touch.velocity.y * 0.4 + inst_vy * 0.6,
                        );
                        inner.touch.last_pos = pos;
                        inner.touch.last_move_at = now;
                        let locked = match inner.touch.scroll_axis {
                            Some(ScrollAxis::Horizontal) => point(dx, Pixels::ZERO),
                            Some(ScrollAxis::Vertical) => point(Pixels::ZERO, dy),
                            None => point(dx, dy),
                        };
                        (
                            !was_scrolling && inner.touch.scrolling,
                            locked,
                            inner.touch.scrolling,
                            false,
                        )
                    }
                };
                if long_pressed {
                    let (claimed, start) = {
                        let inner = window.0.borrow();
                        (inner.touch.long_press_claimed, inner.touch.down_pos)
                    };
                    if claimed {
                        self.dispatch_long_press(TouchPhase::Moved, start, pos);
                    }
                    return;
                }

                if prev_scrolling {
                    let phase = match entered_scroll {
                        true => TouchPhase::Started,
                        false => TouchPhase::Moved,
                    };
                    self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: pos,
                        delta: ScrollDelta::Pixels(delta),
                        modifiers: Modifiers::default(),
                        touch_phase: phase,
                    }));
                }
            }
            MotionAction::Up | MotionAction::PointerUp => {
                let offscreen = point(px(-10_000.0), px(-10_000.0));
                let (was_active, was_scrolling, was_long_pressed, started_fling, tap_pos) = {
                    let mut inner = window.0.borrow_mut();
                    if inner.touch.active_pointer != Some(pointer_id) {
                        (false, false, false, false, pos)
                    } else if inner.touch.long_press_claimed {
                        inner.touch.long_press_claimed = false;
                        inner.touch.long_pressed = false;
                        inner.touch.active_pointer = None;
                        inner.touch.last_pos = offscreen;
                        let start = inner.touch.down_pos;
                        drop(inner);
                        self.dispatch_long_press(TouchPhase::Ended, start, pos);
                        return;
                    } else {
                        let scrolling = inner.touch.scrolling;
                        let long_pressed = inner.touch.long_pressed;
                        let down_pos = inner.touch.down_pos;
                        let fresh = inner.touch.last_move_at.elapsed() < Duration::from_millis(100);
                        let mut fling_started = false;
                        if scrolling && !long_pressed && fresh {
                            let vel = match inner.touch.scroll_axis {
                                Some(ScrollAxis::Horizontal) => point(inner.touch.velocity.x, 0.0),
                                Some(ScrollAxis::Vertical) => point(0.0, inner.touch.velocity.y),
                                None => inner.touch.velocity,
                            };
                            let speed = (vel.x * vel.x + vel.y * vel.y).sqrt();
                            if speed >= FLING_MIN_SPEED {
                                inner.touch.fling = Some(FlingState {
                                    pos,
                                    velocity: vel,
                                    last_tick: Instant::now(),
                                });
                                inner.frame_requested.set(true);
                                fling_started = true;
                            }
                        }
                        inner.touch.active_pointer = None;
                        inner.touch.scrolling = false;
                        inner.touch.long_pressed = false;
                        inner.touch.scroll_axis = None;
                        inner.touch.velocity = Point::default();
                        inner.touch.last_pos = offscreen;
                        (true, scrolling, long_pressed, fling_started, down_pos)
                    }
                };
                if !was_active {
                    return;
                }
                if was_long_pressed {
                    self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                        position: offscreen,
                        pressed_button: None,
                        modifiers: Modifiers::default(),
                    }));
                    return;
                }
                if was_scrolling {
                    if !started_fling {
                        self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: pos,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Ended,
                        }));
                    }
                } else {
                    self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                        position: tap_pos,
                        pressed_button: None,
                        modifiers: Modifiers::default(),
                    }));
                    self.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: tap_pos,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }));
                    self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: tap_pos,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }));
                }
                self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                    position: offscreen,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }));
            }
            MotionAction::Cancel => {
                let offscreen = point(px(-10_000.0), px(-10_000.0));
                let (was_active, was_scrolling) = {
                    let mut inner = window.0.borrow_mut();
                    if inner.touch.active_pointer.is_some() && inner.touch.long_press_claimed {
                        inner.touch.active_pointer = None;
                        inner.touch.long_press_claimed = false;
                        inner.touch.long_pressed = false;
                        inner.touch.last_pos = offscreen;
                        let start = inner.touch.down_pos;
                        drop(inner);
                        self.dispatch_long_press(TouchPhase::Cancelled, start, pos);
                        return;
                    }
                    if inner.touch.active_pointer.take().is_some() {
                        let scrolling = inner.touch.scrolling;
                        inner.touch.scrolling = false;
                        inner.touch.long_pressed = false;
                        inner.touch.scroll_axis = None;
                        inner.touch.velocity = Point::default();
                        inner.touch.last_pos = offscreen;
                        (true, scrolling)
                    } else {
                        (false, false)
                    }
                };
                if was_active {
                    if was_scrolling {
                        self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: pos,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Cancelled,
                        }));
                    }
                    self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                        position: offscreen,
                        pressed_button: None,
                        modifiers: Modifiers::default(),
                    }));
                }
            }
            _ => {}
        }
    }

    fn tick_fling(&self) {
        let Some(window) = self.state.borrow().window.clone() else {
            return;
        };
        let event = {
            let mut inner = window.0.borrow_mut();
            let Some(mut fling) = inner.touch.fling.take() else {
                return;
            };
            let now = Instant::now();
            let dt = now
                .duration_since(fling.last_tick)
                .as_secs_f32()
                .clamp(0.001, 0.05);
            let step = point(px(fling.velocity.x * dt), px(fling.velocity.y * dt));
            let decay = (-FLING_FRICTION * dt).exp();
            fling.velocity.x *= decay;
            fling.velocity.y *= decay;
            fling.last_tick = now;
            let speed =
                (fling.velocity.x * fling.velocity.x + fling.velocity.y * fling.velocity.y).sqrt();
            if speed >= FLING_STOP_SPEED {
                inner.touch.fling = Some(fling);
                inner.frame_requested.set(true);
                Some((fling.pos, step, TouchPhase::Moved))
            } else {
                Some((fling.pos, Point::default(), TouchPhase::Ended))
            }
        };
        if let Some((position, delta, touch_phase)) = event {
            self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(delta),
                modifiers: Modifiers::default(),
                touch_phase,
            }));
        }
    }

    fn request_frame_if_needed(&self) {
        let window = self.state.borrow().window.clone();
        let Some(window) = window else {
            return;
        };
        let (should_draw, cb) = {
            let mut inner = window.0.borrow_mut();
            let ready = inner.native_window.is_some() && inner.visible;
            let requested = inner.frame_requested.replace(false);
            if !ready || !requested {
                (false, None)
            } else {
                (true, inner.callbacks.request_frame.take())
            }
        };
        if should_draw && let Some(mut cb) = cb {
            cb(RequestFrameOptions {
                require_presentation: false,
                force_render: false,
                signal_at: None,
                signal_source: FrameRequestSource::LocalSchedule,
            });
            let mut inner = window.0.borrow_mut();
            if inner.callbacks.request_frame.is_none() {
                inner.callbacks.request_frame = Some(cb);
            }
        }
    }
}

impl Platform for AndroidPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        let mut launch_cb = Some(on_finish_launching);
        let app = self.state.borrow().app.clone();

        while self.state.borrow().running {
            let frame_pending = self
                .state
                .borrow()
                .window
                .as_ref()
                .is_some_and(|w| w.0.borrow().frame_requested.get());
            let touch_pending = self.state.borrow().window.as_ref().is_some_and(|w| {
                let inner = w.0.borrow();
                inner.touch.active_pointer.is_some()
                    && !inner.touch.scrolling
                    && !inner.touch.long_pressed
            });
            let fling_pending = self
                .state
                .borrow()
                .window
                .as_ref()
                .is_some_and(|w| w.0.borrow().touch.fling.is_some());
            let visible = self
                .state
                .borrow()
                .window
                .as_ref()
                .is_some_and(|w| w.0.borrow().visible);
            let timeout = match (frame_pending, touch_pending, fling_pending, visible) {
                (true, _, _, true) => Some(Duration::ZERO),
                (false, _, true, true) => Some(Duration::from_millis(8)),
                (false, true, false, true) => Some(Duration::from_millis(16)),
                (false, false, false, true) => Some(Duration::from_millis(16)),
                (_, _, _, false) => None,
            };

            app.poll_events(timeout, |event| {
                if !self.state.borrow().running {
                    return;
                }
                match event {
                    PollEvent::Wake => {
                        self.dispatcher.drain_main();
                    }
                    PollEvent::Main(main_event) => match main_event {
                        MainEvent::InitWindow { .. } => {
                            let native = app.native_window().map(NativeWindowTarget);
                            let scale = density_scale(&app);
                            {
                                let mut state = self.state.borrow_mut();
                                state.scale = scale;
                                if let Some(window) = state.window.clone() {
                                    let mut inner = window.0.borrow_mut();
                                    inner.native_window = native;
                                    inner.visible = true;
                                    inner.ensure_renderer();
                                    let resized = inner.update_size_from_native(scale);
                                    let bounds_size = inner.bounds.size;
                                    let cb =
                                        resized.then(|| inner.callbacks.resize.take()).flatten();
                                    inner.frame_requested.set(true);
                                    drop(inner);
                                    drop(state);
                                    if let Some(mut cb) = cb {
                                        cb(bounds_size, scale);
                                        let mut inner = window.0.borrow_mut();
                                        if inner.callbacks.resize.is_none() {
                                            inner.callbacks.resize = Some(cb);
                                        }
                                    }
                                }
                            }
                            if !self.state.borrow().launched
                                && app.native_window().is_some()
                                && let Some(cb) = launch_cb.take()
                            {
                                self.state.borrow_mut().launched = true;
                                cb();
                            }
                        }
                        MainEvent::TerminateWindow { .. } => {
                            if let Some(window) = self.state.borrow().window.clone() {
                                let mut inner = window.0.borrow_mut();
                                inner.visible = false;
                                inner.frame_requested.set(false);
                                inner.touch = TouchTracker::default();
                                inner.unconfigure_surface();
                            }
                        }
                        MainEvent::WindowResized { .. }
                        | MainEvent::ContentRectChanged { .. }
                        | MainEvent::ConfigChanged { .. } => {
                            let scale = density_scale(&app);
                            self.state.borrow_mut().scale = scale;
                            let next_insets = query_insets(&app, scale);
                            if let Some(window) = self.state.borrow().window.clone() {
                                let (changed, bounds_size, cb, insets_changed, insets_cb) = {
                                    let mut inner = window.0.borrow_mut();
                                    let changed = inner.update_size_from_native(scale);
                                    let insets_changed = inner.insets != next_insets;
                                    if insets_changed {
                                        inner.insets = next_insets.clone();
                                    }
                                    inner.frame_requested.set(true);
                                    (
                                        changed,
                                        inner.bounds.size,
                                        changed.then(|| inner.callbacks.resize.take()).flatten(),
                                        insets_changed,
                                        insets_changed
                                            .then(|| inner.callbacks.insets_changed.take())
                                            .flatten(),
                                    )
                                };
                                if changed && let Some(mut cb) = cb {
                                    cb(bounds_size, scale);
                                    let mut inner = window.0.borrow_mut();
                                    if inner.callbacks.resize.is_none() {
                                        inner.callbacks.resize = Some(cb);
                                    }
                                }
                                if insets_changed && let Some(mut cb) = insets_cb {
                                    cb(next_insets);
                                    let mut inner = window.0.borrow_mut();
                                    if inner.callbacks.insets_changed.is_none() {
                                        inner.callbacks.insets_changed = Some(cb);
                                    }
                                }
                            }
                        }
                        MainEvent::RedrawNeeded { .. } => {
                            if let Some(window) = self.state.borrow().window.clone() {
                                window.0.borrow().frame_requested.set(true);
                            }
                        }
                        MainEvent::GainedFocus => {
                            if let Some(window) = self.state.borrow().window.clone() {
                                let cb = {
                                    let mut inner = window.0.borrow_mut();
                                    inner.active = true;
                                    inner.callbacks.active_status_change.take()
                                };
                                if let Some(mut cb) = cb {
                                    cb(true);
                                    let mut inner = window.0.borrow_mut();
                                    if inner.callbacks.active_status_change.is_none() {
                                        inner.callbacks.active_status_change = Some(cb);
                                    }
                                }
                            }
                        }
                        MainEvent::LostFocus => {
                            if let Some(window) = self.state.borrow().window.clone() {
                                let cb = {
                                    let mut inner = window.0.borrow_mut();
                                    inner.active = false;
                                    inner.callbacks.active_status_change.take()
                                };
                                if let Some(mut cb) = cb {
                                    cb(false);
                                    let mut inner = window.0.borrow_mut();
                                    if inner.callbacks.active_status_change.is_none() {
                                        inner.callbacks.active_status_change = Some(cb);
                                    }
                                }
                            }
                        }
                        MainEvent::Start | MainEvent::Resume { .. } => {
                            let cb = self.state.borrow_mut().callbacks.lifecycle.take();
                            if let Some(mut cb) = cb {
                                cb(AppLifecyclePhase::Foreground);
                                let mut state = self.state.borrow_mut();
                                if state.callbacks.lifecycle.is_none() {
                                    state.callbacks.lifecycle = Some(cb);
                                }
                            }
                        }
                        MainEvent::Pause | MainEvent::Stop => {
                            let cb = self.state.borrow_mut().callbacks.lifecycle.take();
                            if let Some(mut cb) = cb {
                                cb(AppLifecyclePhase::Background);
                                let mut state = self.state.borrow_mut();
                                if state.callbacks.lifecycle.is_none() {
                                    state.callbacks.lifecycle = Some(cb);
                                }
                            }
                        }
                        MainEvent::Destroy => {
                            let mut state = self.state.borrow_mut();
                            state.running = false;
                            if let Some(window) = state.window.take() {
                                let mut inner = window.0.borrow_mut();
                                inner.visible = false;
                                inner.unconfigure_surface();
                                inner.renderer = None;
                            }
                        }
                        MainEvent::InputAvailable => {
                            self.handle_input_events();
                        }
                        _ => {}
                    },
                    _ => {}
                }
            });

            if !self.state.borrow().running {
                break;
            }
            self.check_long_press();
            self.tick_fling();
            self.drain_ime_input();
            self.dispatcher.drain_main();
            if self.state.borrow().running {
                self.request_frame_if_needed();
            }
        }
    }

    fn quit(&self) {
        self.state.borrow_mut().running = false;
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.primary_display().unwrap()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        let state = self.state.borrow();
        let size = match state.app.native_window() {
            Some(w) => size(
                px(w.width().max(1) as f32 / state.scale),
                px(w.height().max(1) as f32 / state.scale),
            ),
            None => size(px(400.0), px(800.0)),
        };
        Some(Rc::new(AndroidDisplay { size }))
    }

    fn on_displays_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.state
            .borrow()
            .window
            .as_ref()
            .map(|w| w.0.borrow().handle)
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let mut state = self.state.borrow_mut();
        let native_window = state.app.native_window().map(NativeWindowTarget);
        let scale = state.scale;
        let bounds = match native_window.as_ref() {
            Some(native) => Bounds::new(
                Point::default(),
                size(
                    px(native.0.width().max(1) as f32 / scale),
                    px(native.0.height().max(1) as f32 / scale),
                ),
            ),
            None => options.bounds,
        };
        let mut inner = WindowInner {
            handle,
            native_window,
            gpu_context: state.gpu_context.clone(),
            renderer: None,
            fallback_atlas: Arc::new(HeadlessAtlas::default()),
            bounds,
            scale,
            insets: query_insets(&state.app, scale),
            active: true,
            visible: true,
            title: options
                .titlebar
                .and_then(|t| t.title)
                .map(|t| t.to_string())
                .unwrap_or_else(|| "Sonora".to_owned()),
            input_handler: None,
            back_enabled: false,
            callbacks: WindowCallbacks::default(),
            touch: TouchTracker::default(),
            frame_requested: Cell::new(true),
            waker: state.app.create_waker(),
        };
        inner.ensure_renderer();
        let window = AndroidWindow(Rc::new(RefCell::new(inner)));
        state.window = Some(window.clone());
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn open_url(&self, url: &str) {
        call_activity_string(&self.state.borrow().app, jni::jni_str!("openUrl"), url);
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.state.borrow_mut().callbacks.open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt_for_paths(
        &self,
        _options: gpui::PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(None));
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(None));
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.state.borrow_mut().callbacks.quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.reopen = Some(callback);
    }

    fn on_system_sleep(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.state.borrow_mut().callbacks.lifecycle = Some(callback);
    }

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Ok(ActivityGuard::new(Box::new(|| {}))))
    }

    fn compositor_name(&self) -> &'static str {
        "Android"
    }

    fn app_path(&self) -> Result<PathBuf> {
        self.state
            .borrow()
            .app
            .internal_data_path()
            .ok_or_else(|| anyhow!("missing internal data path"))
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        Err(anyhow!(
            "auxiliary executable {name} is unavailable on Android"
        ))
    }

    fn set_cursor_style(&self, _style: CursorStyle) {}

    fn hide_cursor_until_mouse_moves(&self) {}

    fn is_cursor_visible(&self) -> bool {
        false
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        read_activity_clipboard(&self.state.borrow().app).map(ClipboardItem::new_string)
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            call_activity_string(
                &self.state.borrow().app,
                jni::jni_str!("writeClipboard"),
                &text,
            );
        }
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("keychain unavailable on Android")))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Err(anyhow!("keychain unavailable on Android")))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("keychain unavailable on Android")))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(AndroidKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

fn density_scale(app: &AndroidApp) -> f32 {
    app.config()
        .density()
        .map(|dpi| (dpi as f32 / 160.0).max(1.0))
        .unwrap_or(2.0)
}

fn query_insets(app: &AndroidApp, scale: f32) -> WindowInsets {
    if let Some(insets) = query_jni_insets(app, scale) {
        return insets;
    }
    let Some(native) = app.native_window() else {
        return WindowInsets::default();
    };
    let dev_w = native.width().max(1);
    let dev_h = native.height().max(1);
    let rect = app.content_rect();
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return WindowInsets::default();
    }
    WindowInsets {
        safe_area: Edges {
            top: px(rect.top.max(0) as f32 / scale),
            right: px((dev_w - rect.right).max(0) as f32 / scale),
            bottom: px((dev_h - rect.bottom).max(0) as f32 / scale),
            left: px(rect.left.max(0) as f32 / scale),
        },
        ime: Edges::default(),
    }
}

fn query_jni_insets(app: &AndroidApp, scale: f32) -> Option<WindowInsets> {
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_ptr = app.activity_as_ptr() as jni::sys::jobject;
    if activity_ptr.is_null() {
        return None;
    }
    vm.attach_current_thread(|env| -> jni::errors::Result<Option<WindowInsets>> {
        let activity = unsafe { jni::objects::JObject::from_raw(env, activity_ptr) };
        let cls = env.get_object_class(&activity)?;
        let array_obj = env
            .call_static_method(
                &cls,
                jni::jni_str!("getSafeInsets"),
                jni::jni_sig!(() -> int[]),
                &[],
            )?
            .l()?;
        if array_obj.is_null() {
            return Ok(None);
        }
        let int_array = env.cast_local::<jni::objects::JIntArray>(array_obj)?;
        let mut buf = [0i32; 5];
        int_array.get_region(env, 0, &mut buf)?;
        let [top, bottom, left, right, ime_bottom] = buf;
        Ok(Some(WindowInsets {
            safe_area: Edges {
                top: px(top.max(0) as f32 / scale),
                right: px(right.max(0) as f32 / scale),
                bottom: px(bottom.max(0) as f32 / scale),
                left: px(left.max(0) as f32 / scale),
            },
            ime: Edges {
                top: Pixels::ZERO,
                right: Pixels::ZERO,
                bottom: px(ime_bottom.max(0) as f32 / scale),
                left: Pixels::ZERO,
            },
        }))
    })
    .ok()
    .flatten()
}

fn call_activity_void(app: &AndroidApp, method: &jni::strings::JNIStr) {
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_ptr = app.activity_as_ptr() as jni::sys::jobject;
    if activity_ptr.is_null() {
        return;
    }
    let _ = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity = unsafe { jni::objects::JObject::from_raw(env, activity_ptr) };
        let cls = env.get_object_class(&activity)?;
        env.call_static_method(&cls, method, jni::jni_sig!(() -> void), &[])?;
        Ok(())
    });
}

fn call_activity_string(app: &AndroidApp, method: &jni::strings::JNIStr, value: &str) {
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_ptr = app.activity_as_ptr() as jni::sys::jobject;
    if activity_ptr.is_null() {
        return;
    }
    let _ = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity = unsafe { jni::objects::JObject::from_raw(env, activity_ptr) };
        let cls = env.get_object_class(&activity)?;
        let jstr = env.new_string(value)?;
        env.call_static_method(
            &cls,
            method,
            jni::jni_sig!((java.lang.String) -> void),
            &[jni::objects::JValue::from(&jstr)],
        )?;
        Ok(())
    });
}

fn read_activity_clipboard(app: &AndroidApp) -> Option<String> {
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_ptr = app.activity_as_ptr() as jni::sys::jobject;
    if activity_ptr.is_null() {
        return None;
    }
    vm.attach_current_thread(|env| -> jni::errors::Result<Option<String>> {
        let activity = unsafe { jni::objects::JObject::from_raw(env, activity_ptr) };
        let cls = env.get_object_class(&activity)?;
        let obj = env
            .call_static_method(
                &cls,
                jni::jni_str!("readClipboard"),
                jni::jni_sig!(() -> java.lang.String),
                &[],
            )?
            .l()?;
        if obj.is_null() {
            return Ok(None);
        }
        let jstr = env.cast_local::<jni::objects::JString>(obj)?;
        let text = jstr.try_to_string(env)?;
        Ok(Some(text))
    })
    .ok()
    .flatten()
}

fn drain_activity_input_events(app: &AndroidApp) -> Vec<String> {
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_ptr = app.activity_as_ptr() as jni::sys::jobject;
    if activity_ptr.is_null() {
        return Vec::new();
    }
    vm.attach_current_thread(|env| -> jni::errors::Result<Vec<String>> {
        let activity = unsafe { jni::objects::JObject::from_raw(env, activity_ptr) };
        let cls = env.get_object_class(&activity)?;
        let arr_obj = env
            .call_static_method(
                &cls,
                jni::jni_str!("drainInputEvents"),
                jni::jni_sig!(() -> java.lang.String[]),
                &[],
            )?
            .l()?;
        if arr_obj.is_null() {
            return Ok(Vec::new());
        }
        let arr = env.cast_local::<jni::objects::JObjectArray>(arr_obj)?;
        let len = arr.len(env)?;
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let elem = arr.get_element(env, i)?;
            if !elem.is_null() {
                let jstr = env.cast_local::<jni::objects::JString>(elem)?;
                out.push(jstr.try_to_string(env)?);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}
