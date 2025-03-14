use crate::rt::{Signal, WindowHandler, WinitWindow};
use i_slint_core::window::WindowAdapterInternal;
use i_slint_core::InternalToken;
use i_slint_renderer_skia::SkiaRenderer;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use slint::platform::{PointerEventButton, Renderer, WindowAdapter, WindowEvent, WindowProperties};
use slint::{LogicalPosition, LogicalSize, PhysicalSize, PlatformError, SharedString};
use std::any::Any;
use std::cell::Cell;
use std::error::Error;
use std::rc::Rc;
use winit::event::{DeviceId, ElementState, InnerSizeWriter, MouseButton};
use winit::window::WindowId;

/// Implementation of [`WindowAdapter`].
pub struct Window {
    winit: Rc<winit::window::Window>,
    slint: slint::Window,
    renderer: SkiaRenderer,
    visible: Cell<Option<bool>>, // Wayland does not support this so we need to emulate it.
    hidden: Signal<()>,
    pointer: Cell<LogicalPosition>,
    title: Cell<SharedString>,
    minimum_size: Cell<Option<winit::dpi::PhysicalSize<u32>>>,
    maximum_size: Cell<Option<winit::dpi::PhysicalSize<u32>>>,
    preferred_size: Cell<Option<winit::dpi::PhysicalSize<u32>>>,
}

impl Window {
    pub fn new(
        winit: Rc<winit::window::Window>,
        slint: slint::Window,
        renderer: SkiaRenderer,
    ) -> Self {
        Self {
            winit,
            slint,
            renderer,
            visible: Cell::new(None),
            hidden: Signal::default(),
            pointer: Cell::default(),
            title: Cell::default(),
            minimum_size: Cell::default(),
            maximum_size: Cell::default(),
            preferred_size: Cell::default(),
        }
    }

    pub fn from_adapter(adapter: &dyn WindowAdapter) -> &Self {
        adapter
            .internal(InternalToken)
            .unwrap()
            .as_any()
            .downcast_ref::<Self>()
            .unwrap()
    }

    pub fn winit(&self) -> &winit::window::Window {
        &self.winit
    }

    pub fn hidden(&self) -> &Signal<()> {
        &self.hidden
    }
}

impl WinitWindow for Window {
    fn id(&self) -> WindowId {
        self.winit.id()
    }
}

impl WindowHandler for Window {
    fn on_resized(
        &self,
        new: winit::dpi::PhysicalSize<u32>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let size = PhysicalSize::new(new.width, new.height);
        let size = LogicalSize::from_physical(size, self.winit.scale_factor() as f32);

        self.slint.dispatch_event(WindowEvent::Resized { size });

        Ok(())
    }

    fn on_close_requested(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.slint.dispatch_event(WindowEvent::CloseRequested);
        Ok(())
    }

    fn on_focused(&self, gained: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.slint
            .dispatch_event(WindowEvent::WindowActiveChanged(gained));

        Ok(())
    }

    fn on_cursor_moved(
        &self,
        _: DeviceId,
        pos: winit::dpi::PhysicalPosition<f64>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let pos = pos.to_logical(self.winit.scale_factor());
        let position = LogicalPosition::new(pos.x, pos.y);

        self.slint
            .dispatch_event(WindowEvent::PointerMoved { position });
        self.pointer.set(position);

        Ok(())
    }

    fn on_cursor_left(&self, _: DeviceId) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.slint.dispatch_event(WindowEvent::PointerExited);
        Ok(())
    }

    fn on_mouse_input(
        &self,
        _: DeviceId,
        st: ElementState,
        btn: MouseButton,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Map button.
        let button = match btn {
            MouseButton::Left => PointerEventButton::Left,
            MouseButton::Right => PointerEventButton::Right,
            MouseButton::Middle => PointerEventButton::Middle,
            MouseButton::Back => PointerEventButton::Back,
            MouseButton::Forward => PointerEventButton::Forward,
            MouseButton::Other(_) => PointerEventButton::Other,
        };

        // Dispatch to Slint.
        let position = self.pointer.get();
        let ev = match st {
            ElementState::Pressed => WindowEvent::PointerPressed { position, button },
            ElementState::Released => WindowEvent::PointerReleased { position, button },
        };

        self.slint.dispatch_event(ev);

        Ok(())
    }

    fn on_scale_factor_changed(
        &self,
        new: f64,
        _: InnerSizeWriter,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let scale_factor = new as f32;

        self.slint
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });

        Ok(())
    }

    fn on_redraw_requested(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Wayland will show the window on the first render so we need to check visibility flag
        // here.
        if self.visible.get().is_some_and(|v| v) {
            self.renderer.render()?;

            if self.slint.has_active_animations() {
                self.winit.request_redraw();
            }
        }

        Ok(())
    }
}

impl WindowAdapter for Window {
    fn window(&self) -> &slint::Window {
        &self.slint
    }

    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {
        if visible {
            assert!(self.visible.get().is_none());

            self.winit.set_visible(true);

            // Render initial frame on macOS. Without this the modal will show a blank window until
            // show animation is complete. On Wayland there are some problems when another window is
            // showing so we need to to disable it.
            #[cfg(target_os = "macos")]
            {
                let scale_factor = self.winit.scale_factor() as f32;
                let size = self.winit.inner_size();
                let size = PhysicalSize::new(size.width, size.height);
                let size = LogicalSize::from_physical(size, scale_factor);

                self.slint
                    .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
                self.slint.dispatch_event(WindowEvent::Resized { size });

                self.renderer.render()?;
            }

            self.visible.set(Some(true));
        } else if self.visible.get().is_some_and(|v| v) {
            self.winit.set_visible(false);
            self.visible.set(Some(false));
            self.hidden.set(()).unwrap();
        }

        Ok(())
    }

    fn size(&self) -> PhysicalSize {
        let s = self.winit.inner_size();

        PhysicalSize::new(s.width, s.height)
    }

    fn request_redraw(&self) {
        self.winit.request_redraw();
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn update_window_properties(&self, properties: WindowProperties) {
        // Set window title.
        let title = properties.title();

        if self.title.replace(title.clone()) != title {
            self.winit.set_title(&title);
        }

        // Setup mapper.
        let scale = self.winit.scale_factor() as f32;
        let map = move |v: LogicalSize| {
            let v = v.to_physical(scale);

            winit::dpi::PhysicalSize::new(v.width, v.height)
        };

        // Set window size.
        let size = properties.layout_constraints();
        let min = size.min.map(&map);
        let max = size.max.map(&map);
        let pre = map(size.preferred);

        if self.minimum_size.replace(min) != min {
            self.winit.set_min_inner_size(min);
        }

        if self.maximum_size.replace(max) != max {
            self.winit.set_max_inner_size(max);
        }

        // Winit on Wayland will panic if either width or height is zero.
        // TODO: Not sure why Slint also update the preferred size when window size is changed.
        if self.preferred_size.replace(Some(pre)).is_none() && pre.width != 0 && pre.height != 0 {
            let _ = self.winit.request_inner_size(pre);

            if matches!((min, max), (Some(min), Some(max)) if min == max && pre == max) {
                self.winit.set_resizable(false);
            }
        }
    }

    fn internal(&self, _: InternalToken) -> Option<&dyn WindowAdapterInternal> {
        Some(self)
    }

    fn window_handle_06(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.winit.window_handle()
    }

    fn display_handle_06(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.winit.display_handle()
    }
}

impl WindowAdapterInternal for Window {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
