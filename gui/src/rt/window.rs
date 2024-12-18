use std::error::Error;
use winit::dpi::PhysicalSize;

/// Encapsulates winit window with window-specific logic.
///
/// The event loop will exit immediately if any method return an error.
pub trait RuntimeWindow {
    fn update_size(&self, v: PhysicalSize<u32>) -> Result<(), Box<dyn Error + Send + Sync>>;
    fn update_scale_factor(&self, v: f64) -> Result<(), Box<dyn Error + Send + Sync>>;
    fn redraw(&self) -> Result<(), Box<dyn Error + Send + Sync>>;
}
