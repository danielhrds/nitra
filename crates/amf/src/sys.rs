//! Raw declarations transcribed from the C interface of the AMF public
//! headers (`amf/public/include`, AMF 1.5.3, GPUOpen-LibrariesAndSDKs/AMF):
//! `core/Platform.h`, `Version.h`, `Result.h`, `Variant.h`, `Interface.h`,
//! `PropertyStorage.h`, `PropertyStorageEx.h`, `Data.h`, `Buffer.h`,
//! `Surface.h`, `Context.h`, `Factory.h` and `components/Component.h`.
//! Only what the encoder needs is declared; vtable slots that are never
//! called are kept as untyped placeholders so the order stays exact.
//!
//! The transcribed declarations are covered by the headers' license:
//!
//! Copyright (c) 2017-2026 Advanced Micro Devices, Inc. All rights reserved.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to permit
//! persons to whom the Software is furnished to do so, subject to the
//! following conditions:
//!
//! The above copyright notice and this permission notice shall be included
//! in all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
//! OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
//! NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
//! DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
//! OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
//! USE OR OTHER DEALINGS IN THE SOFTWARE.
//!
//! ABI notes (Windows x64): `AMF_STD_CALL` is `__stdcall` and `AMF_CDECL_CALL`
//! is `__cdecl`, both the single x64 convention (`extern "system"`/`"C"`).
//! `amf_long` is a 32-bit C `long`, `amf_bool` is `amf_uint8` in C, `wchar_t`
//! is UTF-16, enums are `int`.

#![allow(dead_code)] // Constants are listed with their neighbors for clarity.

use std::ffi::c_void;

pub type AmfResult = i32;
pub type AmfBool = u8;
pub type AmfLong = i32;
pub type Wide = *const u16;
/// A vtable slot that is never called (its signature is irrelevant).
pub type Unused = *const c_void;

// Version.h: AMF_MAKE_FULL_VERSION packs four 16-bit fields.
pub const fn make_version(major: u64, minor: u64, release: u64, build: u64) -> u64 {
    (major << 48) | (minor << 32) | (release << 16) | build
}

// Result.h (sequential enum from 0).
pub const AMF_OK: AmfResult = 0;
pub const AMF_FAIL: AmfResult = 1;
pub const AMF_INVALID_ARG: AmfResult = 4;
pub const AMF_NO_INTERFACE: AmfResult = 8;
pub const AMF_NOT_SUPPORTED: AmfResult = 10;
pub const AMF_NOT_FOUND: AmfResult = 11;
pub const AMF_NOT_INITIALIZED: AmfResult = 13;
pub const AMF_INVALID_FORMAT: AmfResult = 14;
pub const AMF_WRONG_STATE: AmfResult = 15;
pub const AMF_EOF: AmfResult = 23;
pub const AMF_REPEAT: AmfResult = 24;
pub const AMF_INPUT_FULL: AmfResult = 25;
pub const AMF_ENCODER_NOT_PRESENT: AmfResult = 36;
pub const AMF_NEED_MORE_INPUT: AmfResult = 44;

// Variant.h: AMF_VARIANT_TYPE.
pub const AMF_VARIANT_EMPTY: i32 = 0;
pub const AMF_VARIANT_BOOL: i32 = 1;
pub const AMF_VARIANT_INT64: i32 = 2;
pub const AMF_VARIANT_DOUBLE: i32 = 3;
pub const AMF_VARIANT_SIZE: i32 = 5;
pub const AMF_VARIANT_RATE: i32 = 7;
pub const AMF_VARIANT_RATIO: i32 = 8;

// Data.h.
pub const AMF_DATA_BUFFER: i32 = 0;
pub const AMF_DATA_SURFACE: i32 = 1;
pub const AMF_MEMORY_DX11: i32 = 3;
pub const AMF_DX11_0: i32 = 110;
pub const AMF_DX11_1: i32 = 111;

// Surface.h: AMF_SURFACE_FORMAT.
pub const AMF_SURFACE_NV12: i32 = 1;
pub const AMF_SURFACE_BGRA: i32 = 3;

/// Platform.h `AMFGuid` (16 bytes, no padding).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmfGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// `AMF_DECLARE_IID(AMFBuffer, ...)` in Buffer.h.
pub const IID_AMF_BUFFER: AmfGuid = AmfGuid {
    data1: 0xb04b_7248,
    data2: 0xb6f0,
    data3: 0x4321,
    data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmfSize {
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmfRate {
    pub num: u32,
    pub den: u32,
}

/// The `AMFVariantStruct` union. Only plain-data members are declared;
/// `rect` stands for the largest members (`AMFRect`, `AMFFloatVector4D`,
/// 16 bytes) and `pointer` for the string/interface members.
#[repr(C)]
#[derive(Clone, Copy)]
pub union VariantValue {
    pub bool_value: AmfBool,
    pub int64_value: i64,
    pub double_value: f64,
    pub pointer: *mut c_void,
    pub size_value: AmfSize,
    pub rate_value: AmfRate,
    pub rect: [i32; 4],
}

/// `AMFVariantStruct`: `AMF_VARIANT_TYPE type` followed by the union.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AmfVariant {
    pub kind: i32,
    pub value: VariantValue,
}

/// Every AMF object (except the factory) starts with a vtable pointer.
#[repr(C)]
pub struct Object<V> {
    pub vtbl: *const V,
}

/// Interface.h `AMFInterfaceVtbl` (slots 1-3).
#[repr(C)]
pub struct InterfaceVtbl {
    pub acquire: unsafe extern "system" fn(this: *mut c_void) -> AmfLong,
    pub release: unsafe extern "system" fn(this: *mut c_void) -> AmfLong,
    pub query_interface: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const AmfGuid,
        out: *mut *mut c_void,
    ) -> AmfResult,
}

/// PropertyStorage.h `AMFPropertyStorageVtbl` (slots 1-13).
#[repr(C)]
pub struct PropertyStorageVtbl {
    pub interface: InterfaceVtbl,
    pub set_property:
        unsafe extern "system" fn(this: *mut c_void, name: Wide, value: AmfVariant) -> AmfResult,
    pub get_property: unsafe extern "system" fn(
        this: *mut c_void,
        name: Wide,
        value: *mut AmfVariant,
    ) -> AmfResult,
    pub has_property: Unused,
    pub get_property_count: Unused,
    pub get_property_at: Unused,
    pub clear: Unused,
    pub add_to: Unused,
    pub copy_to: Unused,
    pub add_observer: Unused,
    pub remove_observer: Unused,
}

/// PropertyStorageEx.h `AMFPropertyStorageExVtbl` (slots 1-17). Slots 15
/// and 16 are `GetPropertyInfo`, `GetPropertyInfoAt` on Windows.
#[repr(C)]
pub struct PropertyStorageExVtbl {
    pub storage: PropertyStorageVtbl,
    pub get_properties_info_count: Unused,
    pub get_property_info: Unused,
    pub get_property_info_at: Unused,
    pub validate_property: Unused,
}

/// Data.h `AMFDataVtbl` (slots 1-23).
#[repr(C)]
pub struct DataVtbl {
    pub storage: PropertyStorageVtbl,
    pub get_memory_type: Unused,
    pub duplicate: Unused,
    pub convert: Unused,
    pub interop: Unused,
    pub get_data_type: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub is_reusable: Unused,
    pub set_pts: unsafe extern "system" fn(this: *mut c_void, pts: i64),
    pub get_pts: unsafe extern "system" fn(this: *mut c_void) -> i64,
    pub set_duration: unsafe extern "system" fn(this: *mut c_void, duration: i64),
    pub get_duration: Unused,
}

/// Buffer.h `AMFBufferVtbl` (slots 1-28).
#[repr(C)]
pub struct BufferVtbl {
    pub data: DataVtbl,
    pub set_size: Unused,
    pub get_size: unsafe extern "system" fn(this: *mut c_void) -> usize,
    pub get_native: unsafe extern "system" fn(this: *mut c_void) -> *mut c_void,
    pub add_observer_buffer: Unused,
    pub remove_observer_buffer: Unused,
}

/// Surface.h `AMFSurfaceVtbl` (slots 1-33).
#[repr(C)]
pub struct SurfaceVtbl {
    pub data: DataVtbl,
    pub get_format: Unused,
    pub get_planes_count: Unused,
    pub get_plane_at: Unused,
    pub get_plane: Unused,
    pub get_frame_type: Unused,
    pub set_frame_type: Unused,
    pub set_crop: unsafe extern "system" fn(
        this: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> AmfResult,
    pub copy_surface_region: Unused,
    pub add_observer_surface: Unused,
    pub remove_observer_surface: Unused,
}

/// Context.h `AMFContextVtbl`, slots 1-50 (slots 51-55 are never reached
/// through this declaration).
#[repr(C)]
pub struct ContextVtbl {
    pub storage: PropertyStorageVtbl,
    pub terminate: unsafe extern "system" fn(this: *mut c_void) -> AmfResult,
    pub init_dx9: Unused,
    pub get_dx9_device: Unused,
    pub lock_dx9: Unused,
    pub unlock_dx9: Unused,
    pub init_dx11: unsafe extern "system" fn(
        this: *mut c_void,
        device: *mut c_void,
        version: i32,
    ) -> AmfResult,
    pub get_dx11_device: Unused,
    pub lock_dx11: Unused,
    pub unlock_dx11: Unused,
    pub init_opencl: Unused,
    pub get_opencl_context: Unused,
    pub get_opencl_command_queue: Unused,
    pub get_opencl_device_id: Unused,
    pub get_opencl_compute_factory: Unused,
    pub init_opencl_ex: Unused,
    pub lock_opencl: Unused,
    pub unlock_opencl: Unused,
    pub init_opengl: Unused,
    pub get_opengl_context: Unused,
    pub get_opengl_drawable: Unused,
    pub lock_opengl: Unused,
    pub unlock_opengl: Unused,
    pub init_xv: Unused,
    pub get_xv_device: Unused,
    pub lock_xv: Unused,
    pub unlock_xv: Unused,
    pub init_gralloc: Unused,
    pub get_gralloc_device: Unused,
    pub lock_gralloc: Unused,
    pub unlock_gralloc: Unused,
    pub alloc_buffer: Unused,
    pub alloc_surface: Unused,
    pub alloc_audio_buffer: Unused,
    pub create_buffer_from_host_native: Unused,
    pub create_surface_from_host_native: Unused,
    pub create_surface_from_dx9_native: Unused,
    pub create_surface_from_dx11_native: unsafe extern "system" fn(
        this: *mut c_void,
        texture: *mut c_void,
        surface: *mut *mut c_void,
        observer: *mut c_void,
    ) -> AmfResult,
}

/// Component.h `AMFComponentVtbl` (slots 1-28).
#[repr(C)]
pub struct ComponentVtbl {
    pub storage: PropertyStorageExVtbl,
    pub init: unsafe extern "system" fn(
        this: *mut c_void,
        format: i32,
        width: i32,
        height: i32,
    ) -> AmfResult,
    pub re_init: Unused,
    pub terminate: unsafe extern "system" fn(this: *mut c_void) -> AmfResult,
    pub drain: unsafe extern "system" fn(this: *mut c_void) -> AmfResult,
    pub flush: unsafe extern "system" fn(this: *mut c_void) -> AmfResult,
    pub submit_input: unsafe extern "system" fn(this: *mut c_void, data: *mut c_void) -> AmfResult,
    pub query_output:
        unsafe extern "system" fn(this: *mut c_void, data: *mut *mut c_void) -> AmfResult,
    pub get_context: Unused,
    pub set_output_data_allocator_cb: Unused,
    pub get_caps: Unused,
    pub optimize: Unused,
}

/// Factory.h `AMFFactoryVtbl`. The factory is not an `AMFInterface`: it
/// has no reference counting and lives as long as the runtime.
#[repr(C)]
pub struct FactoryVtbl {
    pub create_context:
        unsafe extern "system" fn(this: *mut c_void, context: *mut *mut c_void) -> AmfResult,
    pub create_component: unsafe extern "system" fn(
        this: *mut c_void,
        context: *mut c_void,
        id: Wide,
        component: *mut *mut c_void,
    ) -> AmfResult,
    pub set_cache_folder: Unused,
    pub get_cache_folder: Unused,
    pub get_debug: Unused,
    pub get_trace: Unused,
    pub get_programs: Unused,
}

/// `AMF_RESULT AMF_CDECL_CALL AMFInit(amf_uint64 version, AMFFactory** ppFactory)`.
pub type AmfInitFn = unsafe extern "C" fn(version: u64, factory: *mut *mut c_void) -> AmfResult;
/// `AMF_RESULT AMF_CDECL_CALL AMFQueryVersion(amf_uint64* pVersion)`.
pub type AmfQueryVersionFn = unsafe extern "C" fn(version: *mut u64) -> AmfResult;

#[cfg(test)]
mod tests {
    use std::mem::{align_of, offset_of, size_of};

    use super::*;

    const SLOT: usize = size_of::<*const c_void>();

    #[test]
    fn variant_matches_the_c_layout() {
        assert_eq!(size_of::<AmfVariant>(), 24);
        assert_eq!(align_of::<AmfVariant>(), 8);
        assert_eq!(offset_of!(AmfVariant, value), 8);
        assert_eq!(size_of::<AmfGuid>(), 16);
        assert_eq!(size_of::<AmfSize>(), 8);
        assert_eq!(size_of::<AmfRate>(), 8);
    }

    #[test]
    fn vtables_have_the_header_slot_counts() {
        assert_eq!(size_of::<InterfaceVtbl>(), 3 * SLOT);
        assert_eq!(size_of::<PropertyStorageVtbl>(), 13 * SLOT);
        assert_eq!(size_of::<PropertyStorageExVtbl>(), 17 * SLOT);
        assert_eq!(size_of::<DataVtbl>(), 23 * SLOT);
        assert_eq!(size_of::<BufferVtbl>(), 28 * SLOT);
        assert_eq!(size_of::<SurfaceVtbl>(), 33 * SLOT);
        assert_eq!(size_of::<ContextVtbl>(), 50 * SLOT);
        assert_eq!(size_of::<ComponentVtbl>(), 28 * SLOT);
        assert_eq!(size_of::<FactoryVtbl>(), 7 * SLOT);
    }

    #[test]
    fn called_slots_sit_at_their_header_positions() {
        // 1-based slot numbers from the headers.
        let slot = |offset: usize| offset / SLOT + 1;
        assert_eq!(slot(offset_of!(PropertyStorageVtbl, set_property)), 4);
        assert_eq!(slot(offset_of!(DataVtbl, get_data_type)), 18);
        assert_eq!(slot(offset_of!(DataVtbl, set_pts)), 20);
        assert_eq!(slot(offset_of!(DataVtbl, get_pts)), 21);
        assert_eq!(slot(offset_of!(BufferVtbl, get_size)), 25);
        assert_eq!(slot(offset_of!(BufferVtbl, get_native)), 26);
        assert_eq!(slot(offset_of!(SurfaceVtbl, set_crop)), 30);
        assert_eq!(slot(offset_of!(ContextVtbl, terminate)), 14);
        assert_eq!(slot(offset_of!(ContextVtbl, init_dx11)), 19);
        assert_eq!(
            slot(offset_of!(ContextVtbl, create_surface_from_dx11_native)),
            50
        );
        assert_eq!(slot(offset_of!(ComponentVtbl, init)), 18);
        assert_eq!(slot(offset_of!(ComponentVtbl, drain)), 21);
        assert_eq!(slot(offset_of!(ComponentVtbl, submit_input)), 23);
        assert_eq!(slot(offset_of!(ComponentVtbl, query_output)), 24);
        assert_eq!(slot(offset_of!(FactoryVtbl, create_component)), 2);
    }

    #[test]
    fn version_packing_matches_the_header_macro() {
        assert_eq!(make_version(1, 5, 3, 0), 0x0001_0005_0003_0000);
    }
}
