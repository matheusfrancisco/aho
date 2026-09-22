//! `08` — the headless product plus the browser and terminal surfaces. Both render the same
//! tree / inventory / approval JSON the CLI prints.

#![forbid(unsafe_code)]

use aho_cap::Composition;

pub const NAME: &str = "aho-web";

pub fn web() -> Composition {
    Composition::new(NAME).include(aho_headless::headless()).surface::<surface_web::SurfaceWeb>().surface::<surface_tui::SurfaceTui>()
}
