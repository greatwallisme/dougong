// Rust guideline compliant 2026-10-21
//! Example dougong plugin built as a wasm component.
//!
//! Depends only on `wit-bindgen` and the WIT contract at `wit/`: no kernel,
//! no async runtime crosses the boundary. All framework semantics run
//! host-side; this component declares and provides the `greeting` service
//! plus two event listeners, exercising the full contract.
//!
//! Build with:
//!
//! ```text
//! cargo build -p greeter-wasm --target wasm32-wasip2
//! ```
//!
//! The artifact lands at `target/wasm32-wasip2/debug/greeter_wasm.wasm`.
//! On non-wasm targets this crate compiles to an empty library so the
//! workspace gates stay green.

#[cfg(target_arch = "wasm32")]
#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        world: "dougong-plugin",
        path: "../../wit",
    });
}

#[cfg(target_arch = "wasm32")]
mod imp;

#[cfg(target_arch = "wasm32")]
pub use imp::Greeter;

#[cfg(target_arch = "wasm32")]
bindings::export!(Greeter with_types_in bindings);
