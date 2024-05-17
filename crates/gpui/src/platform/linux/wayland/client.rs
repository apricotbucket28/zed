use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::{Rc, Weak};
use std::{convert::TryInto, time::Duration};

use collections::HashMap;
use smithay_client_toolkit::activation::RequestData;
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    activation::{ActivationHandler, ActivationState},
    compositor::{CompositorHandler, CompositorState},
    delegate_activation, delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use util::ResultExt;
use wayland_backend::client::ObjectId;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::platform::linux::wayland::serial::{SerialKind, SerialTracker};
use crate::platform::linux::wayland::window::{WaylandWindow, WaylandWindowStatePtr};
use crate::{
    AnyWindowHandle, CursorStyle, DisplayId, ForegroundExecutor, LinuxClient, LinuxCommon,
    PlatformDisplay, PlatformWindow, WindowParams,
};

pub(crate) struct Globals {
    pub qh: QueueHandle<WaylandClientState>,
    pub registry_state: RegistryState,
    pub seat_state: SeatState,
    pub output_state: OutputState,
    pub shm: Shm,
    pub compositor: CompositorState,
    pub xdg_shell: XdgShell,
    pub xdg_activation: Option<ActivationState>,
    pub executor: ForegroundExecutor,
}

pub(crate) struct WaylandClientState {
    serial_tracker: SerialTracker,
    globals: Globals,
    windows: HashMap<ObjectId, WaylandWindowStatePtr>,
    common: LinuxCommon,
    conn: Connection,
    loop_handle: LoopHandle<'static, WaylandClientState>,
    event_loop: Option<EventLoop<'static, WaylandClientState>>,
}

/// This struct is required to conform to Rust's orphan rules, so we can dispatch on the state but hand the
/// window to GPUI.
#[derive(Clone)]
pub struct WaylandClientStatePtr(Weak<RefCell<WaylandClientState>>);

impl WaylandClientStatePtr {
    fn get_client(&self) -> Rc<RefCell<WaylandClientState>> {
        self.0
            .upgrade()
            .expect("The pointer should always be valid when dispatching in wayland")
    }

    pub fn get_serial(&self, kind: SerialKind) -> u32 {
        self.0.upgrade().unwrap().borrow().serial_tracker.get(kind)
    }

    pub fn drop_window(&self, surface_id: &ObjectId) {
        // TODO
        // let mut client = self.get_client();
        // let mut state = client.borrow_mut();
        // let closed_window = state.windows.remove(surface_id).unwrap();
        // if let Some(window) = state.mouse_focused_window.take() {
        //     if !window.ptr_eq(&closed_window) {
        //         state.mouse_focused_window = Some(window);
        //     }
        // }
        // if let Some(window) = state.keyboard_focused_window.take() {
        //     if !window.ptr_eq(&closed_window) {
        //         state.keyboard_focused_window = Some(window);
        //     }
        // }
        // if state.windows.is_empty() {
        //     state.common.signal.stop();
        // }
    }
}

#[derive(Clone)]
pub struct WaylandClient(Rc<RefCell<WaylandClientState>>);

impl Drop for WaylandClient {
    fn drop(&mut self) {
        // TODO
        // let mut state = self.0.borrow_mut();
        // state.windows.clear();

        // // Drop the clipboard to prevent a seg fault after we've closed all Wayland connections.
        // state.primary = None;
        // state.clipboard = None;
        // if let Some(wl_pointer) = &state.wl_pointer {
        //     wl_pointer.release();
        // }
        // if let Some(cursor_shape_device) = &state.cursor_shape_device {
        //     cursor_shape_device.destroy();
        // }
        // if let Some(data_device) = &state.data_device {
        //     data_device.release();
        // }
        // if let Some(text_input) = &state.text_input {
        //     text_input.destroy();
        // }
    }
}

const WL_DATA_DEVICE_MANAGER_VERSION: u32 = 3;
const WL_OUTPUT_VERSION: u32 = 2;

fn wl_seat_version(version: u32) -> u32 {
    // We rely on the wl_pointer.frame event
    const WL_SEAT_MIN_VERSION: u32 = 5;
    const WL_SEAT_MAX_VERSION: u32 = 9;

    if version < WL_SEAT_MIN_VERSION {
        panic!(
            "wl_seat below required version: {} < {}",
            version, WL_SEAT_MIN_VERSION
        );
    }

    version.clamp(WL_SEAT_MIN_VERSION, WL_SEAT_MAX_VERSION)
}

impl WaylandClient {
    pub(crate) fn new() -> Self {
        let conn = Connection::connect_to_env().unwrap();

        let (globals, event_queue) = registry_queue_init(&conn).unwrap();
        let qh = event_queue.handle();
        let event_loop = EventLoop::<WaylandClientState>::try_new().unwrap();
        let loop_handle = event_loop.handle();

        let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
        let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
        let shm = Shm::bind(&globals, &qh).expect("wl shm is not available.");
        // If the compositor supports xdg-activation it probably wants us to use it to get focus
        let xdg_activation = ActivationState::bind(&globals, &qh).ok();

        let display = conn.backend().display_ptr() as *mut std::ffi::c_void;

        let event_loop = EventLoop::<WaylandClientState>::try_new().unwrap();

        let (common, main_receiver) = LinuxCommon::new(event_loop.get_signal());

        let handle = event_loop.handle();
        handle.insert_source(main_receiver, |event, _, _: &mut WaylandClientState| {
            if let calloop::channel::Event::Msg(runnable) = event {
                runnable.run();
            }
        });

        WaylandSource::new(conn.clone(), event_queue)
            .insert(loop_handle)
            .unwrap();

        let globals = Globals {
            qh: qh.clone(),
            executor: common.foreground_executor.clone(),
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state: OutputState::new(&globals, &qh),
            shm,
            compositor,
            xdg_shell,
            xdg_activation,
        };

        let mut state = Rc::new(RefCell::new(WaylandClientState {
            serial_tracker: SerialTracker::new(),
            globals,
            // TODO: output_scales
            windows: HashMap::default(),
            common,
            conn,
            loop_handle: handle.clone(),
            event_loop: Some(event_loop),
        }));

        Self(state)
    }
}

impl LinuxClient for WaylandClient {
    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        Vec::new()
    }

    fn display(&self, id: DisplayId) -> Option<Rc<dyn PlatformDisplay>> {
        unimplemented!()
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        None
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> Box<dyn PlatformWindow> {
        let mut state = self.0.borrow_mut();

        let (window, surface_id) = WaylandWindow::new(
            &state.globals,
            state.conn.backend().display_ptr().cast::<c_void>(),
            WaylandClientStatePtr(Rc::downgrade(&self.0)),
            params,
        );
        state.windows.insert(surface_id, window.0.clone());

        Box::new(window)
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        // let mut state = self.0.borrow_mut();

        // let need_update = state
        //     .cursor_style
        //     .map_or(true, |current_style| current_style != style);

        // if need_update {
        //     let serial = state.serial_tracker.get(SerialKind::MouseEnter);
        //     state.cursor_style = Some(style);

        //     if let Some(cursor_shape_device) = &state.cursor_shape_device {
        //         cursor_shape_device.set_shape(serial, style.to_shape());
        //     } else if state.mouse_focused_window.is_some() {
        //         // cursor-shape-v1 isn't supported, set the cursor using a surface.
        //         let wl_pointer = state
        //             .wl_pointer
        //             .clone()
        //             .expect("window is focused by pointer");
        //         state
        //             .cursor
        //             .set_icon(&wl_pointer, serial, &style.to_icon_name());
        //     }
        // }
    }

    fn open_uri(&self, uri: &str) {
        let mut state = self.0.borrow_mut();
        // if let (Some(activation), Some(window)) = (
        //     state.globals.activation.clone(),
        //     state.mouse_focused_window.clone(),
        // ) {
        //     state.pending_open_uri = Some(uri.to_owned());
        //     let token = activation.get_activation_token(&state.globals.qh, ());
        //     let serial = state.serial_tracker.get(SerialKind::MousePress);
        //     token.set_serial(serial, &state.wl_seat);
        //     token.set_surface(&window.surface());
        //     token.commit();
        // } else {
        //     open_uri_internal(uri, None);
        // }
    }

    fn with_common<R>(&self, f: impl FnOnce(&mut LinuxCommon) -> R) -> R {
        f(&mut self.0.borrow_mut().common)
    }

    fn run(&self) {
        let mut event_loop = self
            .0
            .borrow_mut()
            .event_loop
            .take()
            .expect("App is already running");

        event_loop
            .run(
                None,
                // &mut WaylandClientStatePtr(Rc::downgrade(&self.0)),
                &mut self.0.borrow_mut(),
                |_| {},
            )
            .log_err();
    }

    fn write_to_primary(&self, item: crate::ClipboardItem) {
        // self.0
        //     .borrow_mut()
        //     .primary
        //     .as_mut()
        //     .unwrap()
        //     .set_contents(item.text);
    }

    fn write_to_clipboard(&self, item: crate::ClipboardItem) {
        // self.0
        //     .borrow_mut()
        //     .clipboard
        //     .as_mut()
        //     .unwrap()
        //     .set_contents(item.text);
    }

    fn read_from_primary(&self) -> Option<crate::ClipboardItem> {
        None
        // self.0
        //     .borrow_mut()
        //     .primary
        //     .as_mut()
        //     .unwrap()
        //     .get_contents()
        //     .ok()
        //     .map(|s| crate::ClipboardItem {
        //         text: s,
        //         metadata: None,
        //     })
    }

    fn read_from_clipboard(&self) -> Option<crate::ClipboardItem> {
        None
        // self.0
        //     .borrow_mut()
        //     .clipboard
        //     .as_mut()
        //     .unwrap()
        //     .get_contents()
        //     .ok()
        //     .map(|s| crate::ClipboardItem {
        //         text: s,
        //         metadata: None,
        //     })
    }
}

impl CompositorHandler for WaylandClientState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Not needed for this example.
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // Not needed for this example.
    }

    fn frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(conn, qh);
    }
}

impl OutputHandler for WaylandClientState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.globals.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for WaylandClientState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        // self.exit = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        println!("Window configured to: {:?}", configure);

        // self.width = configure.new_size.0.map(|v| v.get()).unwrap_or(256);
        // self.height = configure.new_size.1.map(|v| v.get()).unwrap_or(256);

        // // Initiate the first draw.
        // if self.first_configure {
        //     self.first_configure = false;
        //     self.draw(conn, qh);
        // }
    }
}

impl ActivationHandler for WaylandClientState {
    type RequestData = RequestData;

    fn new_token(&mut self, token: String, _data: &Self::RequestData) {
        // self.xdg_activation
        //     .as_ref()
        //     .unwrap()
        //     .activate::<WaylandClientState>(self.window.wl_surface(), token);
    }
}

impl SeatHandler for WaylandClientState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.globals.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // if capability == Capability::Keyboard && self.keyboard.is_none() {
        //     println!("Set keyboard capability");
        //     let keyboard = self
        //         .globals
        //         .seat_state
        //         .get_keyboard_with_repeat(
        //             qh,
        //             &seat,
        //             None,
        //             state.loop_handle.clone(),
        //             Box::new(|_state, _wl_kbd, event| {
        //                 println!("Repeat: {:?} ", event);
        //             }),
        //         )
        //         .expect("Failed to create keyboard");

        //     // self.keyboard = Some(keyboard);
        // }

        // if capability == Capability::Pointer && self.pointer.is_none() {
        //     println!("Set pointer capability");
        //     let pointer = self
        //         .seat_state
        //         .get_pointer(qh, &seat)
        //         .expect("Failed to create pointer");
        //     // self.pointer = Some(pointer);
        // }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // if capability == Capability::Keyboard && self.keyboard.is_some() {
        //     println!("Unset keyboard capability");
        //     self.keyboard.take().unwrap().release();
        // }

        // if capability == Capability::Pointer && self.pointer.is_some() {
        //     println!("Unset pointer capability");
        //     self.pointer.take().unwrap().release();
        // }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for WaylandClientState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        keysyms: &[Keysym],
    ) {
        // if self.window.wl_surface() == surface {
        //     println!("Keyboard focus on window with pressed syms: {keysyms:?}");
        //     self.keyboard_focus = true;
        // }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        // if self.window.wl_surface() == surface {
        //     println!("Release keyboard focus on window");
        //     self.keyboard_focus = false;
        // }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key press: {event:?}");
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key release: {event:?}");
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
    ) {
        println!("Update modifiers: {modifiers:?}");
    }
}

impl PointerHandler for WaylandClientState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        use PointerEventKind::*;
        for event in events {
            // Ignore events for other surfaces
            // if &event.surface != self.window.wl_surface() {
            //     continue;
            // }

            match event.kind {
                Enter { .. } => {
                    println!("Pointer entered @{:?}", event.position);
                }
                Leave { .. } => {
                    println!("Pointer left");
                }
                Motion { .. } => {}
                Press { button, .. } => {
                    println!("Press {:x} @ {:?}", button, event.position);
                    // self.shift = self.shift.xor(Some(0));
                }
                Release { button, .. } => {
                    println!("Release {:x} @ {:?}", button, event.position);
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    println!("Scroll H:{horizontal:?}, V:{vertical:?}");
                }
            }
        }
    }
}

impl ShmHandler for WaylandClientState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.globals.shm
    }
}

impl WaylandClientState {
    pub fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        // TODO
    }
}

delegate_compositor!(WaylandClientState);
delegate_output!(WaylandClientState);
delegate_shm!(WaylandClientState);

delegate_seat!(WaylandClientState);
delegate_keyboard!(WaylandClientState);
delegate_pointer!(WaylandClientState);

delegate_xdg_shell!(WaylandClientState);
delegate_xdg_window!(WaylandClientState);
delegate_activation!(WaylandClientState);

delegate_registry!(WaylandClientState);

impl ProvidesRegistryState for WaylandClientState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.globals.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
