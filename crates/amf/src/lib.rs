//! Minimal, safe wrappers over the AMD Advanced Media Framework (AMF)
//! runtime that ships with the AMD driver (`amfrt64.dll`), covering only
//! what a D3D11 hardware encoder needs: a context on an existing D3D11
//! device, a component, surfaces wrapping D3D11 textures (no copies through
//! the CPU) and the encoded output buffers.
//!
//! Raw declarations live in [`sys`] (transcribed from the MIT-licensed AMF
//! headers); every `unsafe` call is here, next to the invariant it relies on.

mod sys;

pub mod avc;

use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{Interface, PCWSTR, s, w};

pub use sys::{AMF_SURFACE_BGRA as SURFACE_BGRA, AMF_SURFACE_NV12 as SURFACE_NV12};

/// The oldest runtime this code accepts: every interface slot used here is
/// unchanged since AMF 1.4.0 (the headers were transcribed from 1.5.3).
pub const REQUIRED_VERSION: u64 = sys::make_version(1, 4, 0, 0);
/// AMF timestamps (`amf_pts`) count 100 ns units.
pub const PTS_PER_SECOND: i64 = 10_000_000;

/// A failed AMF call: which call and its `AMF_RESULT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmfError {
    pub call: &'static str,
    pub result: i32,
}

impl AmfError {
    fn check(call: &'static str, result: i32) -> Result<(), AmfError> {
        if result == sys::AMF_OK {
            Ok(())
        } else {
            Err(AmfError { call, result })
        }
    }
}

impl fmt::Display for AmfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} failed: {} ({})",
            self.call,
            result_name(self.result),
            self.result
        )
    }
}

impl std::error::Error for AmfError {}

/// Name of an `AMF_RESULT` value (Result.h order).
pub fn result_name(result: i32) -> &'static str {
    const NAMES: [&str; 46] = [
        "AMF_OK",
        "AMF_FAIL",
        "AMF_UNEXPECTED",
        "AMF_ACCESS_DENIED",
        "AMF_INVALID_ARG",
        "AMF_OUT_OF_RANGE",
        "AMF_OUT_OF_MEMORY",
        "AMF_INVALID_POINTER",
        "AMF_NO_INTERFACE",
        "AMF_NOT_IMPLEMENTED",
        "AMF_NOT_SUPPORTED",
        "AMF_NOT_FOUND",
        "AMF_ALREADY_INITIALIZED",
        "AMF_NOT_INITIALIZED",
        "AMF_INVALID_FORMAT",
        "AMF_WRONG_STATE",
        "AMF_FILE_NOT_OPEN",
        "AMF_NO_DEVICE",
        "AMF_DIRECTX_FAILED",
        "AMF_OPENCL_FAILED",
        "AMF_GLX_FAILED",
        "AMF_XV_FAILED",
        "AMF_ALSA_FAILED",
        "AMF_EOF",
        "AMF_REPEAT",
        "AMF_INPUT_FULL",
        "AMF_RESOLUTION_CHANGED",
        "AMF_RESOLUTION_UPDATED",
        "AMF_INVALID_DATA_TYPE",
        "AMF_INVALID_RESOLUTION",
        "AMF_CODEC_NOT_SUPPORTED",
        "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
        "AMF_SURFACE_MUST_BE_SHARED",
        "AMF_DECODER_NOT_PRESENT",
        "AMF_DECODER_SURFACE_ALLOCATION_FAILED",
        "AMF_DECODER_NO_FREE_SURFACES",
        "AMF_ENCODER_NOT_PRESENT",
        "AMF_DEM_ERROR",
        "AMF_DEM_PROPERTY_READONLY",
        "AMF_DEM_REMOTE_DISPLAY_CREATE_FAILED",
        "AMF_DEM_START_ENCODING_FAILED",
        "AMF_DEM_QUERY_OUTPUT_FAILED",
        "AMF_TAN_CLIPPING_WAS_REQUIRED",
        "AMF_TAN_UNSUPPORTED_VERSION",
        "AMF_NEED_MORE_INPUT",
        "AMF_VULKAN_FAILED",
    ];
    usize::try_from(result)
        .ok()
        .and_then(|index| NAMES.get(index).copied())
        .unwrap_or("unknown AMF_RESULT")
}

/// Formats a packed AMF version as `major.minor.release.build`.
pub fn format_version(packed: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        (packed >> 48) & 0xFFFF,
        (packed >> 32) & 0xFFFF,
        (packed >> 16) & 0xFFFF,
        packed & 0xFFFF
    )
}

/// The loaded runtime and its factory.
pub struct Runtime {
    factory: NonNull<sys::Object<sys::FactoryVtbl>>,
    version: u64,
}

// SAFETY: the factory is a process-wide singleton of the runtime, documented
// as usable from any thread; it is never freed (see `runtime`).
unsafe impl Send for Runtime {}
// SAFETY: as above; its methods only create new objects.
unsafe impl Sync for Runtime {}

/// Loads the AMF runtime once per process (System32 only) and initializes
/// it. The DLL is never unloaded: the driver's encoder threads may still run
/// its code after our objects are released, and unloading a DLL under live
/// threads crashes (seen while measuring WGC capture).
pub fn runtime() -> Result<&'static Runtime, AmfError> {
    static RUNTIME: OnceLock<Result<Runtime, AmfError>> = OnceLock::new();
    RUNTIME
        .get_or_init(load_runtime)
        .as_ref()
        .map_err(Clone::clone)
}

fn load_runtime() -> Result<Runtime, AmfError> {
    let not_present = |call| AmfError {
        call,
        result: sys::AMF_NOT_FOUND,
    };
    // SAFETY: static NUL-terminated name; System32 only, so no planted DLL
    // from the current or application folder is loaded. The handle is
    // intentionally leaked (never `FreeLibrary`).
    let module = unsafe { LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| not_present("LoadLibraryExW(amfrt64.dll)"))?;
    // SAFETY: valid module handle and static NUL-terminated symbol names.
    let (query, init) = unsafe {
        (
            GetProcAddress(module, s!("AMFQueryVersion")),
            GetProcAddress(module, s!("AMFInit")),
        )
    };
    let query = query.ok_or_else(|| not_present("GetProcAddress(AMFQueryVersion)"))?;
    let init = init.ok_or_else(|| not_present("GetProcAddress(AMFInit)"))?;
    // SAFETY: both exports have the signatures declared in Factory.h
    // (cdecl, which is the x64 convention).
    type Export = unsafe extern "system" fn() -> isize;
    let (query, init) = unsafe {
        (
            std::mem::transmute::<Export, sys::AmfQueryVersionFn>(query),
            std::mem::transmute::<Export, sys::AmfInitFn>(init),
        )
    };

    let mut version = 0_u64;
    // SAFETY: live out-pointer; the module stays loaded forever.
    AmfError::check("AMFQueryVersion", unsafe { query(&mut version) })?;
    if version < REQUIRED_VERSION {
        return Err(AmfError {
            call: "AMFQueryVersion (runtime older than 1.4.0)",
            result: sys::AMF_NOT_SUPPORTED,
        });
    }
    let mut factory = std::ptr::null_mut();
    // SAFETY: live out-pointer; asks for the interface version we declared.
    AmfError::check("AMFInit", unsafe { init(REQUIRED_VERSION, &mut factory) })?;
    let factory = NonNull::new(factory.cast()).ok_or(AmfError {
        call: "AMFInit (null factory)",
        result: sys::AMF_FAIL,
    })?;
    Ok(Runtime { factory, version })
}

impl Runtime {
    /// The installed runtime's version (packed; see [`format_version`]).
    pub fn version(&self) -> u64 {
        self.version
    }

    fn factory_vtbl(&self) -> &sys::FactoryVtbl {
        // SAFETY: the factory and its vtable live as long as the runtime.
        unsafe { &*self.factory.as_ref().vtbl }
    }

    pub fn create_context(&self) -> Result<Context, AmfError> {
        let mut context = std::ptr::null_mut();
        // SAFETY: valid factory; live out-pointer receiving one reference.
        let result = unsafe {
            (self.factory_vtbl().create_context)(self.factory.as_ptr().cast(), &mut context)
        };
        AmfError::check("AMFFactory::CreateContext", result)?;
        // SAFETY: on success `context` is an owned AMFContext reference.
        unsafe { Ref::from_raw(context) }
            .map(Context)
            .ok_or(AmfError {
                call: "AMFFactory::CreateContext (null)",
                result: sys::AMF_FAIL,
            })
    }

    /// Creates a component (e.g. [`avc::ENCODER`]) on `context`.
    pub fn create_component(&self, context: &Context, id: PCWSTR) -> Result<Component, AmfError> {
        let mut component = std::ptr::null_mut();
        // SAFETY: valid factory and context; `id` is a static NUL-terminated
        // wide string; live out-pointer receiving one reference.
        let result = unsafe {
            (self.factory_vtbl().create_component)(
                self.factory.as_ptr().cast(),
                context.0.as_raw(),
                id.as_ptr(),
                &mut component,
            )
        };
        AmfError::check("AMFFactory::CreateComponent", result)?;
        // SAFETY: on success `component` is an owned AMFComponent reference.
        unsafe { Ref::from_raw(component) }
            .map(Component)
            .ok_or(AmfError {
                call: "AMFFactory::CreateComponent (null)",
                result: sys::AMF_FAIL,
            })
    }
}

/// A property value (only the plain-data variant types used here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Int64(i64),
    Size {
        width: i32,
        height: i32,
    },
    Rate {
        num: u32,
        den: u32,
    },
    /// A variant type not decoded here (its `AMF_VARIANT_TYPE`).
    Other(i32),
}

impl Value {
    fn to_variant(self) -> sys::AmfVariant {
        let (kind, value) = match self {
            Value::Bool(flag) => (
                sys::AMF_VARIANT_BOOL,
                sys::VariantValue {
                    bool_value: u8::from(flag),
                },
            ),
            Value::Int64(number) => (
                sys::AMF_VARIANT_INT64,
                sys::VariantValue {
                    int64_value: number,
                },
            ),
            Value::Size { width, height } => (
                sys::AMF_VARIANT_SIZE,
                sys::VariantValue {
                    size_value: sys::AmfSize { width, height },
                },
            ),
            Value::Rate { num, den } => (
                sys::AMF_VARIANT_RATE,
                sys::VariantValue {
                    rate_value: sys::AmfRate { num, den },
                },
            ),
            Value::Other(_) => (sys::AMF_VARIANT_EMPTY, sys::VariantValue { rect: [0; 4] }),
        };
        sys::AmfVariant { kind, value }
    }

    fn from_variant(variant: &sys::AmfVariant) -> Value {
        // SAFETY: each arm reads the union member that `kind` declares.
        unsafe {
            match variant.kind {
                sys::AMF_VARIANT_BOOL => Value::Bool(variant.value.bool_value != 0),
                sys::AMF_VARIANT_INT64 => Value::Int64(variant.value.int64_value),
                sys::AMF_VARIANT_SIZE => Value::Size {
                    width: variant.value.size_value.width,
                    height: variant.value.size_value.height,
                },
                sys::AMF_VARIANT_RATE => Value::Rate {
                    num: variant.value.rate_value.num,
                    den: variant.value.rate_value.den,
                },
                other => Value::Other(other),
            }
        }
    }
}

/// Vtables that begin with `AMFPropertyStorageVtbl` (and so with
/// `AMFInterfaceVtbl`).
///
/// # Safety
/// `storage` must return the vtable's leading `PropertyStorageVtbl`.
unsafe trait Vtable {
    fn storage(&self) -> &sys::PropertyStorageVtbl;
}

// SAFETY (all four): each struct's first field is, directly or through its
// own first field, the `PropertyStorageVtbl`, mirroring the C headers.
unsafe impl Vtable for sys::ContextVtbl {
    fn storage(&self) -> &sys::PropertyStorageVtbl {
        &self.storage
    }
}
unsafe impl Vtable for sys::ComponentVtbl {
    fn storage(&self) -> &sys::PropertyStorageVtbl {
        &self.storage.storage
    }
}
unsafe impl Vtable for sys::SurfaceVtbl {
    fn storage(&self) -> &sys::PropertyStorageVtbl {
        &self.data.storage
    }
}
unsafe impl Vtable for sys::BufferVtbl {
    fn storage(&self) -> &sys::PropertyStorageVtbl {
        &self.data.storage
    }
}
unsafe impl Vtable for sys::DataVtbl {
    fn storage(&self) -> &sys::PropertyStorageVtbl {
        &self.storage
    }
}

/// One owned reference to an AMF object; released on drop.
struct Ref<V: Vtable> {
    object: NonNull<sys::Object<V>>,
}

impl<V: Vtable> Ref<V> {
    /// # Safety
    /// `raw` must be null or an owned reference to an object whose vtable
    /// is `V`.
    unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw.cast()).map(|object| Self { object })
    }

    fn as_raw(&self) -> *mut c_void {
        self.object.as_ptr().cast()
    }

    fn vtbl(&self) -> &V {
        // SAFETY: the object is alive while we hold a reference, and so is
        // its vtable (static data of the runtime).
        unsafe { &*self.object.as_ref().vtbl }
    }

    fn set_property(&self, name: PCWSTR, value: Value) -> Result<(), AmfError> {
        // SAFETY: valid object; `name` is a NUL-terminated wide string and
        // the variant holds plain data (nothing for the callee to free).
        let result = unsafe {
            (self.vtbl().storage().set_property)(self.as_raw(), name.as_ptr(), value.to_variant())
        };
        AmfError::check("AMFPropertyStorage::SetProperty", result)
    }

    /// Reads a property. Only used for plain-data properties: a string or
    /// interface variant would need `AMFVariantClear`, which is not
    /// declared here, and is reported as `Value::Other` without freeing it.
    fn get_property(&self, name: PCWSTR) -> Result<Value, AmfError> {
        let mut variant = Value::Other(0).to_variant();
        // SAFETY: valid object, NUL-terminated name, live out-variant.
        let result = unsafe {
            (self.vtbl().storage().get_property)(self.as_raw(), name.as_ptr(), &mut variant)
        };
        AmfError::check("AMFPropertyStorage::GetProperty", result)?;
        Ok(Value::from_variant(&variant))
    }
}

impl<V: Vtable> Drop for Ref<V> {
    fn drop(&mut self) {
        // SAFETY: we own exactly one reference, released exactly once.
        unsafe { (self.vtbl().storage().interface.release)(self.as_raw()) };
    }
}

/// An AMF context bound to a D3D11 device. Drop it after every component
/// created on it.
pub struct Context(Ref<sys::ContextVtbl>);

impl Context {
    /// Binds the context to `device` (which it references; the device must
    /// have multithread protection on if other threads use it).
    pub fn init_dx11(&self, device: &ID3D11Device) -> Result<(), AmfError> {
        // SAFETY: valid context and device; AMF adds its own reference.
        let result =
            unsafe { (self.0.vtbl().init_dx11)(self.0.as_raw(), device.as_raw(), sys::AMF_DX11_1) };
        AmfError::check("AMFContext::InitDX11", result)
    }

    /// Wraps a D3D11 texture of the context's device in an AMF surface
    /// without copying it. The texture must not be rewritten until the
    /// encoder is done with the surface.
    pub fn create_surface_from_dx11(&self, texture: &ID3D11Texture2D) -> Result<Surface, AmfError> {
        let mut surface = std::ptr::null_mut();
        // SAFETY: valid context and texture (AMF references it); no observer.
        let result = unsafe {
            (self.0.vtbl().create_surface_from_dx11_native)(
                self.0.as_raw(),
                texture.as_raw(),
                &mut surface,
                std::ptr::null_mut(),
            )
        };
        AmfError::check("AMFContext::CreateSurfaceFromDX11Native", result)?;
        // SAFETY: on success `surface` is an owned AMFSurface reference.
        unsafe { Ref::from_raw(surface) }
            .map(Surface)
            .ok_or(AmfError {
                call: "AMFContext::CreateSurfaceFromDX11Native (null)",
                result: sys::AMF_FAIL,
            })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: valid context; releases its device resources before the
        // last reference goes (in `Ref::drop`).
        let _ = unsafe { (self.0.vtbl().terminate)(self.0.as_raw()) };
    }
}

/// What `Component::submit_input` did with the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    Accepted,
    /// The input queue is full: retrieve output and submit again.
    InputFull,
}

/// What `Component::query_output` returned.
pub enum Output {
    Buffer(Buffer),
    /// Nothing ready yet.
    Pending,
    /// Drained: no more output until new input after `flush`.
    EndOfStream,
}

pub struct Component(Ref<sys::ComponentVtbl>);

impl Component {
    pub fn set_property(&self, name: PCWSTR, value: Value) -> Result<(), AmfError> {
        self.0.set_property(name, value)
    }

    pub fn get_property(&self, name: PCWSTR) -> Result<Value, AmfError> {
        self.0.get_property(name)
    }

    pub fn init(&self, format: i32, width: i32, height: i32) -> Result<(), AmfError> {
        // SAFETY: valid component; plain arguments.
        let result = unsafe { (self.0.vtbl().init)(self.0.as_raw(), format, width, height) };
        AmfError::check("AMFComponent::Init", result)
    }

    pub fn submit_input(&self, surface: &Surface) -> Result<Submitted, AmfError> {
        // SAFETY: valid component and surface; the component takes its own
        // reference when it accepts the input.
        let result = unsafe { (self.0.vtbl().submit_input)(self.0.as_raw(), surface.0.as_raw()) };
        match result {
            sys::AMF_INPUT_FULL => Ok(Submitted::InputFull),
            other => {
                AmfError::check("AMFComponent::SubmitInput", other).map(|()| Submitted::Accepted)
            }
        }
    }

    /// Takes the next output without waiting (with the default
    /// `QueryTimeout` of 0).
    pub fn query_output(&self) -> Result<Output, AmfError> {
        let mut data = std::ptr::null_mut();
        // SAFETY: valid component; live out-pointer receiving one reference.
        let result = unsafe { (self.0.vtbl().query_output)(self.0.as_raw(), &mut data) };
        // SAFETY: when non-null, `data` is an owned AMFData reference
        // (released when `data` drops).
        let data: Option<Ref<sys::DataVtbl>> = unsafe { Ref::from_raw(data) };
        match (result, data) {
            (sys::AMF_EOF, _) => Ok(Output::EndOfStream),
            (sys::AMF_OK | sys::AMF_REPEAT | sys::AMF_NEED_MORE_INPUT, None) => Ok(Output::Pending),
            (sys::AMF_OK, Some(data)) => {
                let mut buffer = std::ptr::null_mut();
                // SAFETY: valid data object; static IID; live out-pointer
                // receiving one reference on success.
                let result = unsafe {
                    (data.vtbl().storage.interface.query_interface)(
                        data.as_raw(),
                        &sys::IID_AMF_BUFFER,
                        &mut buffer,
                    )
                };
                AmfError::check("AMFData::QueryInterface(AMFBuffer)", result)?;
                // SAFETY: on success `buffer` is an owned AMFBuffer reference.
                unsafe { Ref::from_raw(buffer) }
                    .map(|buffer| Output::Buffer(Buffer(buffer)))
                    .ok_or(AmfError {
                        call: "AMFData::QueryInterface(AMFBuffer) (null)",
                        result: sys::AMF_FAIL,
                    })
            }
            (other, _) => Err(AmfError {
                call: "AMFComponent::QueryOutput",
                result: other,
            }),
        }
    }

    /// Asks for every pending output; `query_output` ends with
    /// `EndOfStream`. Returns `InputFull` if it must be retried after
    /// taking output.
    pub fn drain(&self) -> Result<Submitted, AmfError> {
        // SAFETY: valid component.
        let result = unsafe { (self.0.vtbl().drain)(self.0.as_raw()) };
        match result {
            sys::AMF_INPUT_FULL => Ok(Submitted::InputFull),
            other => AmfError::check("AMFComponent::Drain", other).map(|()| Submitted::Accepted),
        }
    }

    /// Drops queued input and output (after a drain, to accept input again).
    pub fn flush(&self) -> Result<(), AmfError> {
        // SAFETY: valid component.
        let result = unsafe { (self.0.vtbl().flush)(self.0.as_raw()) };
        AmfError::check("AMFComponent::Flush", result)
    }
}

impl Drop for Component {
    fn drop(&mut self) {
        // SAFETY: valid component; stops it and frees its resources before
        // the last reference goes (in `Ref::drop`).
        let _ = unsafe { (self.0.vtbl().terminate)(self.0.as_raw()) };
    }
}

/// An input surface wrapping a D3D11 texture.
pub struct Surface(Ref<sys::SurfaceVtbl>);

impl Surface {
    pub fn set_property(&self, name: PCWSTR, value: Value) -> Result<(), AmfError> {
        self.0.set_property(name, value)
    }

    pub fn set_pts(&self, pts: i64) {
        // SAFETY: valid surface; plain argument.
        unsafe { (self.0.vtbl().data.set_pts)(self.0.as_raw(), pts) }
    }

    pub fn set_duration(&self, duration: i64) {
        // SAFETY: valid surface; plain argument.
        unsafe { (self.0.vtbl().data.set_duration)(self.0.as_raw(), duration) }
    }

    /// Restricts the surface to its top-left `width` x `height`.
    pub fn set_crop(&self, width: i32, height: i32) -> Result<(), AmfError> {
        // SAFETY: valid surface; plain arguments.
        let result = unsafe { (self.0.vtbl().set_crop)(self.0.as_raw(), 0, 0, width, height) };
        AmfError::check("AMFSurface::SetCrop", result)
    }
}

/// An output buffer (host memory owned by AMF).
pub struct Buffer(Ref<sys::BufferVtbl>);

impl Buffer {
    pub fn get_property(&self, name: PCWSTR) -> Result<Value, AmfError> {
        self.0.get_property(name)
    }

    pub fn pts(&self) -> i64 {
        // SAFETY: valid buffer.
        unsafe { (self.0.vtbl().data.get_pts)(self.0.as_raw()) }
    }

    /// The buffer's bytes, valid while the buffer is alive.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: valid buffer; `GetNative` of a host buffer points to
        // `GetSize` readable bytes owned by the buffer, which outlives the
        // returned slice (it borrows `self`).
        unsafe {
            let size = (self.0.vtbl().get_size)(self.0.as_raw());
            let data = (self.0.vtbl().get_native)(self.0.as_raw());
            if data.is_null() || size == 0 {
                return &[];
            }
            std::slice::from_raw_parts(data.cast::<u8>(), size)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_names_follow_the_header_order() {
        assert_eq!(result_name(0), "AMF_OK");
        assert_eq!(result_name(25), "AMF_INPUT_FULL");
        assert_eq!(result_name(44), "AMF_NEED_MORE_INPUT");
        assert_eq!(result_name(45), "AMF_VULKAN_FAILED");
        assert_eq!(result_name(46), "unknown AMF_RESULT");
        assert_eq!(result_name(-1), "unknown AMF_RESULT");
    }

    #[test]
    fn values_round_trip_through_variants() {
        for value in [
            Value::Bool(true),
            Value::Bool(false),
            Value::Int64(-7),
            Value::Int64(20_000_000),
            Value::Size {
                width: 1920,
                height: 1080,
            },
            Value::Rate { num: 60, den: 1 },
        ] {
            assert_eq!(Value::from_variant(&value.to_variant()), value);
        }
        assert_eq!(
            Value::from_variant(&Value::Other(3).to_variant()),
            Value::Other(sys::AMF_VARIANT_EMPTY)
        );
    }

    #[test]
    fn version_formats_four_fields() {
        assert_eq!(format_version(sys::make_version(1, 5, 2, 0)), "1.5.2.0");
    }

    #[test]
    #[ignore = "requires the AMD driver"]
    fn runtime_loads_and_creates_a_context() {
        let runtime = runtime().unwrap();
        assert!(runtime.version() >= REQUIRED_VERSION);
        let _context = runtime.create_context().unwrap();
    }
}
