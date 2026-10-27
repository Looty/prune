use crate::model::Provider;

pub mod bun;
pub mod cargo;
pub mod go;
pub mod npm;
pub mod nuget;
pub mod pip;
pub mod pnpm;
pub mod util;
pub mod uv;
pub mod yarn;

/// Every supported package manager, in display order.
pub fn all() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(npm::Npm),
        Box::new(pnpm::Pnpm),
        Box::new(yarn::Yarn),
        Box::new(bun::Bun),
        Box::new(pip::Pip),
        Box::new(uv::Uv),
        Box::new(cargo::Cargo),
        Box::new(go::Go),
        Box::new(nuget::Nuget),
    ]
}
