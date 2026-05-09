// src/registry/mod.rs
//
// Registries are the source of truth for "what real things exist on this
// system that the user might want to act on." Each registry knows how to
// scan its domain (installed apps, running apps, system settings) and how
// to resolve a fuzzy user query into a confident match.
//
// Today: AppRegistry only. Future phases add RunningAppRegistry and
// SystemSettingsRegistry following the same shape.

pub mod apps;
pub mod matcher;

pub use apps::AppRegistry;
pub use matcher::{ResolutionResult};   