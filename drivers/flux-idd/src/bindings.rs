//! Raw IddCx bindings generated at build time from the WDK headers.
//!
//! IddCx's API surface is FORCEINLINE functions dispatching through the
//! IddCxFunctions table (resolved by IddCxStub.lib); bindgen's
//! `wrap_static_fns` generates callable extern wrappers for them.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/iddcx_bindings.rs"));
