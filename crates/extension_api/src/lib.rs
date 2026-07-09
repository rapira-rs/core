//! SDK for rapira WASM extensions.
//!
//! An extension is a sidecar that **drives** PHP: its async [`Extension::run`]
//! awaits [`exec`] to run requests through rapira's PHP pool, and can `join!`
//! several to run them concurrently.
//!
//!
//! use extension_api::{Extension, Request, Result, exec, register_extension};
//!
//! struct MyExt;
//! impl Extension for MyExt {
//!     fn new() -> Self { MyExt }
//!     async fn run(&mut self) -> Result<()> {
//!         let res = exec(Request::get("/")).await?;
//!         if res.status != 200 { return Err(format!("got {}", res.status)); }
//!         Ok(())
//!     }
//! }
//! register_extension!(MyExt);
//!
//!
//! Build with `cargo build --target wasm32-wasip2 --release`.

#[doc(hidden)]
pub mod wit {
    // `async: true` + the WIT `async func`s make `run`/`exec` async on the guest.
    // `pub_export_macro` lets `register_extension!` define the `Guest` impl in the
    // extension crate (where the user's type is known).
    wit_bindgen::generate!({
        path: "wit",
        world: "extension",
        async: true,
        pub_export_macro: true,
        default_bindings_module: "extension_api::wit",
    });
}

pub use wit::rapira::extension::types::{Request, Response};

/// Every fallible SDK path reports a plain string, which rapira logs; there is no
/// panicking path (a panic would trap the instance).
pub type Result<T, E = String> = core::result::Result<T, E>;

/// The host reads these six bytes back out of the component before instantiating it.
#[cfg(target_arch = "wasm32")]
#[unsafe(link_section = "rapira:api-version")]
#[doc(hidden)]
pub static RAPIRA_API_VERSION: [u8; 6] =
    *include_bytes!(concat!(env!("OUT_DIR"), "/version_bytes"));

/// Run an HTTP request through PHP. Await it; `join!` several to run concurrently.
pub async fn exec(req: Request) -> Result<Response> {
    wit::rapira::extension::host::exec(req).await
}

impl Request {
    /// A bodyless `GET` for `uri`, no headers.
    pub fn get(uri: &str) -> Self {
        Self {
            method: "GET".to_string(),
            uri: uri.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// A rapira extension. rapira constructs it and awaits [`run`](Extension::run) once.
#[allow(async_fn_in_trait)]
pub trait Extension {
    fn new() -> Self
    where
        Self: Sized;

    /// The entry rapira calls: drive PHP with [`exec`]; return `Err` to report a failure.
    async fn run(&mut self) -> Result<()>;
}

/// Registers a type as this component's extension, defining the world's single
/// async `run` export.
#[macro_export]
macro_rules! register_extension {
    ($t:ty) => {
        struct __RapiraComponent;
        impl $crate::wit::Guest for __RapiraComponent {
            async fn run() -> ::core::result::Result<(), ::std::string::String> {
                let mut ext = <$t as $crate::Extension>::new();
                $crate::Extension::run(&mut ext).await
            }
        }
        $crate::wit::export!(__RapiraComponent);
    };
}
