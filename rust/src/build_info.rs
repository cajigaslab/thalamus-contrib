//! This plugin's build type, version and git commit (see build.rs), the
//! counterparts of Thalamus's ThalamusAPI::thalamus_build_type etc.

/// "Debug" or "Release", like Thalamus's CMAKE_BUILD_TYPE.
pub const BUILD_TYPE: &str = if cfg!(debug_assertions) { "Debug" } else { "Release" };
/// The thalamus-contrib package version (__about__.py).
pub const VERSION: &str = env!("THALAMUS_CONTRIB_VERSION");
/// The abbreviated git commit this was built from, empty if git couldn't
/// tell.
pub const COMMIT: &str = env!("THALAMUS_CONTRIB_COMMIT");
