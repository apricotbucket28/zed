use std::cell::{Ref, RefCell, RefMut};
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::{convert::TryInto, time::Duration};

use blade_graphics as gpu;
use collections::HashMap;
use collections::HashSet;
use futures::channel::oneshot::Receiver;
use raw_window_handle as rwh;
use smithay_client_toolkit::activation::RequestData;
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::shell::xdg::XdgSurface;
use smithay_client_toolkit::{
    activation::{ActivationHandler, ActivationState},
    compositor::{CompositorHandler, CompositorState},
    delegate_activation, delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym},
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
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_backend::client::ObjectId;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::platform::blade::{BladeRenderer, BladeSurfaceConfig};
use crate::platform::linux::wayland::display::WaylandDisplay;
use crate::platform::linux::wayland::serial::SerialKind;
use crate::platform::{PlatformAtlas, PlatformInputHandler, PlatformWindow};
use crate::scene::Scene;
use crate::{
    px, size, Bounds, DevicePixels, Globals, Modifiers, Pixels, PlatformDisplay, PlatformInput,
    Point, PromptLevel, Size, WaylandClientStatePtr, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowParams,
};

#[derive(Default)]
pub(crate) struct Callbacks {
    request_frame: Option<Box<dyn FnMut()>>,
    input: Option<Box<dyn FnMut(crate::PlatformInput) -> crate::DispatchEventResult>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
}

struct RawWindow {
    window: *mut c_void,
    display: *mut c_void,
}

impl rwh::HasWindowHandle for RawWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let window = NonNull::new(self.window).unwrap();
        let handle = rwh::WaylandWindowHandle::new(window);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}
impl rwh::HasDisplayHandle for RawWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let display = NonNull::new(self.display).unwrap();
        let handle = rwh::WaylandDisplayHandle::new(display);
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(handle.into()) })
    }
}

struct WaylandWindowState {
    exit: bool,
    first_configure: bool,
    window: Window,
    renderer: BladeRenderer,
    bounds: Bounds<u32>,
    scale: f32,
    input_handler: Option<PlatformInputHandler>,
    fullscreen: bool,
    restore_bounds: Bounds<DevicePixels>,
    maximized: bool,
    client: WaylandClientStatePtr,
    callbacks: Callbacks,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: bool, // TODO: remove this?
    pointer: Option<wl_pointer::WlPointer>,
}

#[derive(Clone)]
pub struct WaylandWindowStatePtr {
    state: Rc<RefCell<WaylandWindowState>>,
    callbacks: Rc<RefCell<Callbacks>>,
}

impl WaylandWindowState {
    pub(crate) fn new(
        window: Window,
        display_ptr: *mut std::ffi::c_void,
        client: WaylandClientStatePtr,
        globals: &Globals,
        options: WindowParams,
    ) -> Self {
        let bounds = options.bounds.map(|p| p.0 as u32);

        let raw = RawWindow {
            window: window.wl_surface().id().as_ptr().cast::<c_void>(),
            display: display_ptr,
        };
        let gpu = Arc::new(
            unsafe {
                gpu::Context::init_windowed(
                    &raw,
                    gpu::ContextDesc {
                        validation: false,
                        capture: false,
                        overlay: false,
                    },
                )
            }
            .unwrap(),
        );
        let config = BladeSurfaceConfig {
            size: gpu::Extent {
                width: bounds.size.width,
                height: bounds.size.height,
                depth: 1,
            },
            transparent: options.window_background != WindowBackgroundAppearance::Opaque,
        };

        // Kick things off
        window.commit();

        Self {
            exit: false,
            first_configure: true,
            window,
            renderer: BladeRenderer::new(gpu, config),
            bounds,
            scale: 1.0, // TODO
            input_handler: None,
            fullscreen: false,
            restore_bounds: Bounds::default(),
            maximized: false,
            callbacks: Callbacks::default(),
            client,
            keyboard: None,
            keyboard_focus: false, // TODO: remove this?
            pointer: None,
        }
    }
}

pub(crate) struct WaylandWindow(pub WaylandWindowStatePtr);

impl Drop for WaylandWindow {
    fn drop(&mut self) {
        // TODO
    }
}

impl WaylandWindowStatePtr {
    pub fn frame(&self, request_frame_callback: bool) {
        // if request_frame_callback {
        //     let state = self.state.borrow_mut();
        //     state
        //         .window
        //         .wl_surface()
        //         .frame(&state.globals.qh, state.surface.id());
        //     drop(state);
        // }
        let mut cb = self.callbacks.borrow_mut();
        if let Some(fun) = cb.request_frame.as_mut() {
            fun();
        }
    }

    pub fn set_size_and_scale(
        &self,
        width: Option<NonZeroU32>,
        height: Option<NonZeroU32>,
        scale: Option<f32>,
    ) {
        let (width, height, scale) = {
            let mut state = self.state.borrow_mut();
            if width.map_or(true, |width| width.get() == state.bounds.size.width)
                && height.map_or(true, |height| height.get() == state.bounds.size.height)
                && scale.map_or(true, |scale| scale == state.scale)
            {
                return;
            }
            if let Some(width) = width {
                state.bounds.size.width = width.get();
            }
            if let Some(height) = height {
                state.bounds.size.height = height.get();
            }
            if let Some(scale) = scale {
                state.scale = scale;
            }
            let width = state.bounds.size.width;
            let height = state.bounds.size.height;
            let scale = state.scale;
            state.renderer.update_drawable_size(size(
                width as f64 * scale as f64,
                height as f64 * scale as f64,
            ));
            (width, height, scale)
        };

        if let Some(ref mut fun) = self.callbacks.borrow_mut().resize {
            fun(
                Size {
                    width: px(width as f32),
                    height: px(height as f32),
                },
                scale,
            );
        }

        // {
        //     let state = self.state.borrow();
        //     if let Some(viewport) = &state.viewport {
        //         viewport.set_destination(width as i32, height as i32);
        //     }
        // }
    }
}

impl WaylandWindow {
    fn borrow(&self) -> Ref<WaylandWindowState> {
        self.0.state.borrow()
    }

    fn borrow_mut(&self) -> RefMut<WaylandWindowState> {
        self.0.state.borrow_mut()
    }

    pub fn new(
        globals: &Globals,
        display_ptr: *mut std::ffi::c_void,
        client: WaylandClientStatePtr,
        params: WindowParams,
    ) -> (Self, ObjectId) {
        let surface = globals.compositor.create_surface(&globals.qh);
        let surface_id = surface.id();
        let window =
            globals
                .xdg_shell
                .create_window(surface, WindowDecorations::RequestClient, &globals.qh);

        let this = Self(WaylandWindowStatePtr {
            state: Rc::new(RefCell::new(WaylandWindowState::new(
                window,
                display_ptr,
                client,
                globals,
                params,
            ))),
            callbacks: Rc::new(RefCell::new(Callbacks::default())),
        });
        (this, surface_id)
    }
}

impl rwh::HasWindowHandle for WaylandWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        unimplemented!()
    }
}
impl rwh::HasDisplayHandle for WaylandWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        unimplemented!()
    }
}

impl PlatformWindow for WaylandWindow {
    fn bounds(&self) -> Bounds<DevicePixels> {
        self.borrow().bounds.map(|p| DevicePixels(p as i32))
    }

    fn is_maximized(&self) -> bool {
        self.borrow().maximized
    }

    fn window_bounds(&self) -> WindowBounds {
        let state = self.borrow();
        if state.fullscreen {
            WindowBounds::Fullscreen(state.restore_bounds)
        } else if state.maximized {
            WindowBounds::Maximized(state.restore_bounds)
        } else {
            WindowBounds::Windowed(state.bounds.map(|p| DevicePixels(p as i32)))
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        let state = self.borrow();
        Size {
            width: Pixels(state.bounds.size.width as f32),
            height: Pixels(state.bounds.size.height as f32),
        }
    }

    fn scale_factor(&self) -> f32 {
        self.borrow().scale
    }

    // todo(linux)
    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }

    // todo(linux)
    fn display(&self) -> Rc<dyn PlatformDisplay> {
        Rc::new(WaylandDisplay {})
    }

    // todo(linux)
    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    // todo(linux)
    fn modifiers(&self) -> Modifiers {
        crate::Modifiers::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[&str],
    ) -> Option<Receiver<usize>> {
        None
    }

    fn activate(&self) {
        // todo(linux)
    }

    // todo(linux)
    fn is_active(&self) -> bool {
        false
    }

    fn set_title(&mut self, title: &str) {
        self.borrow().window.set_title(title);
    }

    fn set_app_id(&mut self, app_id: &str) {
        self.borrow().window.set_app_id(app_id);
    }

    fn set_background_appearance(&mut self, background_appearance: WindowBackgroundAppearance) {
        // TODO
        // let opaque = background_appearance == WindowBackgroundAppearance::Opaque;
        // let mut state = self.borrow_mut();
        // state.renderer.update_transparency(!opaque);

        // let region = state
        //     .globals
        //     .compositor
        //     .create_region(&state.globals.qh, ());
        // region.add(0, 0, i32::MAX, i32::MAX);

        // if opaque {
        //     // Promise the compositor that this region of the window surface
        //     // contains no transparent pixels. This allows the compositor to
        //     // do skip whatever is behind the surface for better performance.
        //     state.surface.set_opaque_region(Some(&region));
        // } else {
        //     state.surface.set_opaque_region(None);
        // }

        // if let Some(ref blur_manager) = state.globals.blur_manager {
        //     if (background_appearance == WindowBackgroundAppearance::Blurred) {
        //         if (state.blur.is_none()) {
        //             let blur = blur_manager.create(&state.surface, &state.globals.qh, ());
        //             blur.set_region(Some(&region));
        //             state.blur = Some(blur);
        //         }
        //         state.blur.as_ref().unwrap().commit();
        //     } else {
        //         // It probably doesn't hurt to clear the blur for opaque windows
        //         blur_manager.unset(&state.surface);
        //         if let Some(b) = state.blur.take() {
        //             b.release()
        //         }
        //     }
        // }

        // region.destroy();
    }

    fn set_edited(&mut self, edited: bool) {
        // todo(linux)
    }

    fn show_character_palette(&self) {
        // todo(linux)
    }

    fn minimize(&self) {
        self.borrow().window.set_minimized();
    }

    fn zoom(&self) {
        let state = self.borrow();
        if !state.maximized {
            state.window.set_maximized();
        } else {
            state.window.unset_maximized();
        }
    }

    fn toggle_fullscreen(&self) {
        let mut state = self.borrow_mut();
        state.restore_bounds = state.bounds.map(|p| DevicePixels(p as i32));
        if !state.fullscreen {
            state.window.set_fullscreen(None);
        } else {
            state.window.unset_fullscreen();
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.borrow().fullscreen
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut()>) {
        println!("on_request_frame");
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        // todo(linux)
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.borrow_mut();
        state.renderer.draw(scene);
    }

    fn completed_frame(&self) {
        let mut state = self.borrow_mut();
        state.window.wl_surface().commit();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        let state = self.borrow();
        state.renderer.sprite_atlas().clone()
    }

    fn show_window_menu(&self, position: Point<Pixels>) {
        let state = self.borrow();
        let serial = state.client.get_serial(SerialKind::MousePress);
        // state.window.show_window_menu(
        //     &state.globals.seat,
        //     serial,
        //     (position.x.0 as i32, position.y.0 as i32),
        // );
    }

    fn start_system_move(&self) {
        let state = self.borrow();
        let serial = state.client.get_serial(SerialKind::MousePress);
        // state.window.move_(state.globals.seat, serial);
    }

    fn should_render_window_controls(&self) -> bool {
        // self.borrow().decoration_state == WaylandDecorationState::Client
        false
    }
}
