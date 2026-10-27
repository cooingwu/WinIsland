use std::cell::{Cell, RefCell};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::Win32::UI::WindowsAndMessaging::{
    KillTimer, MSG, SetCoalescableTimer, TIMERV_DEFAULT_COALESCING, USER_TIMER_MAXIMUM,
    USER_TIMER_MINIMUM, WM_DWMCOMPOSITIONCHANGED, WM_TIMER,
};
use winisland_platform::{
    AppHandler, DisplayProvider, InputState, Key, MouseButton, MouseWheelDelta, PlatformError,
    PlatformEvent, Theme, TouchPhase, WindowId, WindowPoint, WindowPosition, WindowSize,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{
    ElementState, InnerSizeWriter, MouseButton as WinitMouseButton, MouseScrollDelta,
    TouchPhase as WinitTouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Theme as WinitTheme, WindowId as WinitWindowId};

use crate::display::WindowsDisplay;

static PROXY: OnceLock<EventLoopProxy<()>> = OnceLock::new();
static WAKE_PENDING: AtomicBool = AtomicBool::new(false);
static COMPOSITION_CHANGED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static ACTIVE_EVENT_LOOP: Cell<*const ActiveEventLoop> = const { Cell::new(std::ptr::null()) };
    static DPI_WRITER: RefCell<Option<(WindowId, InnerSizeWriter)>> = const { RefCell::new(None) };
    static COALESCED_TIMER_ID: Cell<usize> = const { Cell::new(0) };
}

pub fn wake() {
    let Some(proxy) = PROXY.get() else {
        return;
    };
    if !WAKE_PENDING.swap(true, Ordering::AcqRel) && proxy.send_event(()).is_err() {
        WAKE_PENDING.store(false, Ordering::Release);
    }
}

pub(super) fn run(handler: &mut dyn AppHandler) -> Result<(), PlatformError> {
    let mut builder = EventLoop::<()>::builder();
    builder.with_msg_hook(|message| {
        if !message.is_null() {
            // SAFETY: winit supplies a valid MSG pointer for this synchronous callback.
            let message = unsafe { &*message.cast::<MSG>() };
            if message.message == WM_TIMER
                && message.hwnd.is_invalid()
                && COALESCED_TIMER_ID.with(|id| id.get() != 0 && id.get() == message.wParam.0)
            {
                return true;
            }
            if message.message == windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY {
                crate::shell::plugin_commands::pressed(message.wParam.0);
            }
            if message.message == WM_DWMCOMPOSITIONCHANGED {
                COMPOSITION_CHANGED.store(true, Ordering::Release);
                wake();
            }
        }
        false
    });
    let event_loop = builder.build().map_err(PlatformError::backend)?;
    let _ = PROXY.set(event_loop.create_proxy());
    event_loop
        .run_app(&mut EventAdapter {
            handler,
            idle_timer: CoalescedTimer::default(),
        })
        .map_err(PlatformError::backend)
}

pub(super) fn exit() {
    let _ = with_active_event_loop(ActiveEventLoop::exit);
}

pub(super) fn with_active_event_loop<T>(f: impl FnOnce(&ActiveEventLoop) -> T) -> Option<T> {
    ACTIVE_EVENT_LOOP.with(|slot| {
        let event_loop = slot.get();
        if event_loop.is_null() {
            None
        } else {
            // SAFETY: The pointer is installed only for the duration of a winit callback on this thread.
            Some(f(unsafe { &*event_loop }))
        }
    })
}

pub(super) fn request_dpi_inner_size(id: WindowId, size: WindowSize) -> Option<bool> {
    DPI_WRITER.with(|slot| {
        let mut active = slot.borrow_mut();
        let (active_id, writer) = active.as_mut()?;
        if *active_id != id {
            return None;
        }
        Some(
            writer
                .request_inner_size(PhysicalSize::new(size.width, size.height))
                .is_ok(),
        )
    })
}

fn scope_active_event_loop<T>(event_loop: &ActiveEventLoop, f: impl FnOnce() -> T) -> T {
    ACTIVE_EVENT_LOOP.with(|slot| {
        let previous = slot.replace(event_loop);
        struct Restore<'a> {
            slot: &'a Cell<*const ActiveEventLoop>,
            previous: *const ActiveEventLoop,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.slot.set(self.previous);
            }
        }
        let _restore = Restore { slot, previous };
        f()
    })
}

fn scope_dpi_writer<T>(id: WindowId, writer: InnerSizeWriter, f: impl FnOnce() -> T) -> T {
    DPI_WRITER.with(|slot| {
        let previous = slot.replace(Some((id, writer)));
        struct Restore<'a> {
            slot: &'a RefCell<Option<(WindowId, InnerSizeWriter)>>,
            previous: Option<(WindowId, InnerSizeWriter)>,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = self.slot.replace(self.previous.take());
            }
        }
        let _restore = Restore { slot, previous };
        f()
    })
}

struct EventAdapter<'a> {
    handler: &'a mut dyn AppHandler,
    idle_timer: CoalescedTimer,
}

#[derive(Default)]
struct CoalescedTimer {
    id: usize,
    deadline: Option<Instant>,
    failed: bool,
}

impl CoalescedTimer {
    fn arm(&mut self, deadline: Instant) -> bool {
        if self.failed {
            return false;
        }
        if self.id != 0 && self.deadline == Some(deadline) {
            return true;
        }
        self.cancel();
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(1_000_000)
            .clamp(USER_TIMER_MINIMUM as u128, USER_TIMER_MAXIMUM as u128)
            as u32;
        // SAFETY: The timer belongs to this event-loop thread and is handled by its message hook.
        self.id = unsafe { SetCoalescableTimer(None, 0, timeout, None, TIMERV_DEFAULT_COALESCING) };
        if self.id == 0 {
            let error = windows::core::Error::from_thread();
            self.failed = true;
            log::warn!("Coalesced event-loop timer is unavailable: {error}");
            return false;
        }
        self.deadline = Some(deadline);
        COALESCED_TIMER_ID.with(|id| id.set(self.id));
        true
    }

    fn cancel(&mut self) {
        if self.id != 0 {
            COALESCED_TIMER_ID.with(|id| id.set(0));
            // SAFETY: The ID is owned by this timer and is released on its creating thread.
            let _ = unsafe { KillTimer(None, self.id) };
            self.id = 0;
        }
        self.deadline = None;
    }
}

impl Drop for CoalescedTimer {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl EventAdapter<'_> {
    fn composition_changed(&mut self) {
        if COMPOSITION_CHANGED.swap(false, Ordering::AcqRel) {
            self.handler.on_event(PlatformEvent::CompositionChanged);
        }
    }
}

impl ApplicationHandler<()> for EventAdapter<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        scope_active_event_loop(event_loop, || {
            self.handler.on_event(PlatformEvent::Resumed);
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: WinitWindowId,
        event: WindowEvent,
    ) {
        let native_id = WindowId(u64::from(id));
        let id = if let Some(owner) = super::input_owner(native_id) {
            if !matches!(
                event,
                WindowEvent::CloseRequested
                    | WindowEvent::DroppedFile(_)
                    | WindowEvent::CursorMoved { .. }
                    | WindowEvent::CursorLeft { .. }
                    | WindowEvent::CursorEntered { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::Touch(_)
                    | WindowEvent::Focused(_)
                    | WindowEvent::KeyboardInput { .. }
                    | WindowEvent::Ime(_)
            ) {
                return;
            }
            owner
        } else {
            if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
                super::sync_input_window(native_id);
            }
            native_id
        };
        scope_active_event_loop(event_loop, || match event {
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                inner_size_writer,
            } => scope_dpi_writer(id, inner_size_writer, || {
                self.handler.on_event(PlatformEvent::ScaleFactorChanged {
                    id,
                    scale: scale_factor,
                });
            }),
            other => {
                if let Some(event) = platform_event(id, other) {
                    self.handler.on_event(event);
                }
            }
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        scope_active_event_loop(event_loop, || {
            WAKE_PENDING.store(false, Ordering::Release);
            self.composition_changed();
            self.handler.on_event(PlatformEvent::Wake);
        });
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        scope_active_event_loop(event_loop, || {
            self.composition_changed();
            let control_flow = match self.handler.on_about_to_wait() {
                Some(wake) if wake.deadline <= Instant::now() => {
                    self.idle_timer.cancel();
                    ControlFlow::Poll
                }
                Some(wake) if !wake.precise && self.idle_timer.arm(wake.deadline) => {
                    ControlFlow::Wait
                }
                Some(wake) => {
                    self.idle_timer.cancel();
                    ControlFlow::WaitUntil(wake.deadline)
                }
                None => {
                    self.idle_timer.cancel();
                    ControlFlow::Wait
                }
            };
            event_loop.set_control_flow(control_flow);
        });
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.idle_timer.cancel();
        scope_active_event_loop(event_loop, || {
            self.handler.on_event(PlatformEvent::Exiting);
            self.handler.on_exit();
        });
    }
}

fn platform_event(id: WindowId, event: WindowEvent) -> Option<PlatformEvent> {
    let event = match event {
        WindowEvent::CloseRequested => PlatformEvent::CloseRequested { id },
        WindowEvent::Destroyed => PlatformEvent::Destroyed { id },
        WindowEvent::Resized(size) => PlatformEvent::Resized {
            id,
            size: WindowSize {
                width: size.width,
                height: size.height,
            },
        },
        WindowEvent::Moved(position) => PlatformEvent::Moved {
            id,
            position: WindowPosition {
                x: position.x,
                y: position.y,
            },
        },
        WindowEvent::ThemeChanged(theme) => PlatformEvent::ThemeChanged {
            id,
            theme: match theme {
                WinitTheme::Light => Theme::Light,
                WinitTheme::Dark => Theme::Dark,
            },
        },
        WindowEvent::RedrawRequested => PlatformEvent::RedrawRequested { id },
        WindowEvent::Focused(focused) => PlatformEvent::Focused { id, focused },
        WindowEvent::CursorMoved { position, .. } => PlatformEvent::CursorMoved {
            id,
            position: WindowPoint {
                x: position.x,
                y: position.y,
            },
        },
        WindowEvent::CursorLeft { .. } => PlatformEvent::CursorLeft { id },
        WindowEvent::MouseInput { state, button, .. } => PlatformEvent::MouseInput {
            id,
            button: match button {
                WinitMouseButton::Left => MouseButton::Left,
                WinitMouseButton::Right => MouseButton::Right,
                _ => MouseButton::Other,
            },
            state: input_state(state),
            position: WindowsDisplay
                .cursor_position()
                .ok()
                .flatten()
                .unwrap_or_default(),
        },
        WindowEvent::MouseWheel { delta, .. } => PlatformEvent::MouseWheel {
            id,
            delta: match delta {
                MouseScrollDelta::LineDelta(x, y) => MouseWheelDelta::Lines { x, y },
                MouseScrollDelta::PixelDelta(position) => MouseWheelDelta::Pixels {
                    x: position.x,
                    y: position.y,
                },
            },
        },
        WindowEvent::KeyboardInput { event, .. } => PlatformEvent::KeyInput {
            id,
            key: match event.logical_key {
                WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
                WinitKey::Named(NamedKey::Enter) => Key::Enter,
                WinitKey::Named(NamedKey::Escape) => Key::Escape,
                WinitKey::Named(NamedKey::ArrowLeft) => Key::ArrowLeft,
                WinitKey::Named(NamedKey::ArrowRight) => Key::ArrowRight,
                WinitKey::Named(NamedKey::ArrowUp) => Key::ArrowUp,
                WinitKey::Named(NamedKey::ArrowDown) => Key::ArrowDown,
                WinitKey::Named(NamedKey::Tab) => Key::Tab,
                WinitKey::Named(NamedKey::Delete) => Key::Delete,
                WinitKey::Named(NamedKey::Home) => Key::Home,
                WinitKey::Named(NamedKey::End) => Key::End,
                WinitKey::Character(value) => Key::Character(value.to_string()),
                _ => Key::Other,
            },
            state: input_state(event.state),
            text: event.text.map(|text| text.to_string()),
            repeat: event.repeat,
        },
        WindowEvent::ModifiersChanged(modifiers) => {
            let state = modifiers.state();
            PlatformEvent::ModifiersChanged {
                id,
                modifiers: u32::from(state.control_key())
                    | (u32::from(state.alt_key()) << 1)
                    | (u32::from(state.shift_key()) << 2)
                    | (u32::from(state.super_key()) << 3),
            }
        }
        WindowEvent::Ime(winit::event::Ime::Commit(text)) => PlatformEvent::ImeCommit { id, text },
        WindowEvent::Ime(winit::event::Ime::Preedit(text, cursor)) => {
            PlatformEvent::ImeComposition { id, text, cursor }
        }
        WindowEvent::Touch(touch) => PlatformEvent::Touch {
            id,
            touch_id: touch.id,
            phase: match touch.phase {
                WinitTouchPhase::Started => TouchPhase::Started,
                WinitTouchPhase::Moved => TouchPhase::Moved,
                WinitTouchPhase::Ended => TouchPhase::Ended,
                WinitTouchPhase::Cancelled => TouchPhase::Cancelled,
            },
            position: WindowPoint {
                x: touch.location.x,
                y: touch.location.y,
            },
        },
        WindowEvent::DroppedFile(path) => PlatformEvent::DroppedFile { id, path },
        _ => return None,
    };
    Some(event)
}

fn input_state(state: ElementState) -> InputState {
    match state {
        ElementState::Pressed => InputState::Pressed,
        ElementState::Released => InputState::Released,
    }
}
