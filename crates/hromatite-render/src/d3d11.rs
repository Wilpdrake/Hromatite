//! D3D11 immediate-context backend for the in-game UI ([`super::ui`]).
//!
//! Everything in the live layer runs on the game's own render thread — the
//! thread that calls `IDXGISwapChain::Present` — because the D3D11 immediate
//! context is not thread-safe and Unity's gfx thread owns it. The resident
//! installs the hook; this module turns one [`Frame`] of nuklear draw commands
//! into `DrawIndexed` calls and restores the game's state afterwards.
//!
//! The vtable indices and the numeric constants below were taken from the
//! `IDXGISwapChain`/`ID3D11Device`/`ID3D11DeviceContext` declarations and the
//! D3D11 enums; a wrong index is a crash, so they are listed next to their
//! source order and covered by `vtable_indices` unit tests.

#![cfg_attr(not(windows), allow(dead_code))]

use crate::nk::Frame;

/// `sizeof(struct nk_d3d11_vertex)`: `pos[2] f32, uv[2] f32, col[4] u8`.
pub const VERTEX_SIZE: usize = 20;

/// `IID_ID3D11Device` — used by `IDXGISwapChain::GetDevice`.
pub const IID_ID3D11_DEVICE: [u8; 16] = [
    0xdb, 0x6d, 0x6f, 0xdb, // Data1 0xdb6f6ddb (little endian)
    0x77, 0xac, // Data2 0xac77
    0x88, 0x4e, // Data3 0x4e88
    0x82, 0x53, 0x81, 0x9d, 0xf9, 0xbb, 0xf1, 0x40,
];

/// `IID_ID3D11Texture2D` — `{6f15aaf2-d208-4e89-9ab4-489535d34f9c}`, used by
/// `IDXGISwapChain::GetBuffer(0, ...)`.
pub const IID_ID3D11_TEXTURE2D: [u8; 16] = [
    0xf2, 0xaa, 0x15, 0x6f, // Data1 0x6f15aaf2 (little endian)
    0x08, 0xd2, // Data2 0xd208
    0x89, 0x4e, // Data3 0x4e89
    0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c,
];

// ------------------------------------------------------------------ indices

const IUNKNOWN_RELEASE: u32 = 2;

/// `IDXGISwapChain` (after `IDXGIDeviceSubObject` → `IDXGIObject`).
pub const SWAPCHAIN_GET_DEVICE: u32 = 7;
pub const SWAPCHAIN_PRESENT: u32 = 8;
pub const SWAPCHAIN_GET_BUFFER: u32 = 9;
pub const SWAPCHAIN_GET_DESC: u32 = 12;
/// `IDXGISwapChain` has 18 entries in DXGI 1.x.
pub const SWAPCHAIN_SLOTS: u32 = 18;

/// `ID3D11Device` (after `IUnknown`).
pub const DEVICE_CREATE_BUFFER: u32 = 3;
pub const DEVICE_CREATE_TEXTURE2D: u32 = 5;
pub const DEVICE_CREATE_SHADER_RESOURCE_VIEW: u32 = 7;
pub const DEVICE_CREATE_RENDER_TARGET_VIEW: u32 = 9;
pub const DEVICE_CREATE_INPUT_LAYOUT: u32 = 11;
pub const DEVICE_CREATE_VERTEX_SHADER: u32 = 12;
pub const DEVICE_CREATE_PIXEL_SHADER: u32 = 15;
pub const DEVICE_CREATE_BLEND_STATE: u32 = 20;
pub const DEVICE_CREATE_RASTERIZER_STATE: u32 = 22;
pub const DEVICE_CREATE_SAMPLER_STATE: u32 = 23;
pub const DEVICE_GET_IMMEDIATE_CONTEXT: u32 = 40;

/// `ID3D11DeviceContext` (after `ID3D11DeviceChild`).
pub const CTX_VS_SET_CONSTANT_BUFFERS: u32 = 7;
pub const CTX_PS_SET_SHADER_RESOURCES: u32 = 8;
pub const CTX_PS_SET_SHADER: u32 = 9;
pub const CTX_PS_SET_SAMPLERS: u32 = 10;
pub const CTX_VS_SET_SHADER: u32 = 11;
pub const CTX_DRAW_INDEXED: u32 = 12;
pub const CTX_MAP: u32 = 14;
pub const CTX_UNMAP: u32 = 15;
pub const CTX_IA_SET_INPUT_LAYOUT: u32 = 17;
pub const CTX_IA_SET_VERTEX_BUFFERS: u32 = 18;
pub const CTX_IA_SET_INDEX_BUFFER: u32 = 19;
pub const CTX_IA_SET_PRIMITIVE_TOPOLOGY: u32 = 24;
pub const CTX_OM_SET_RENDER_TARGETS: u32 = 33;
pub const CTX_OM_SET_BLEND_STATE: u32 = 35;
pub const CTX_RS_SET_STATE: u32 = 43;
pub const CTX_RS_SET_VIEWPORTS: u32 = 44;
pub const CTX_RS_SET_SCISSOR_RECTS: u32 = 45;
pub const CTX_UPDATE_SUBRESOURCE: u32 = 48;
pub const CTX_VS_GET_CONSTANT_BUFFERS: u32 = 72;
pub const CTX_PS_GET_SHADER_RESOURCES: u32 = 73;
pub const CTX_PS_GET_SHADER: u32 = 74;
pub const CTX_PS_GET_SAMPLERS: u32 = 75;
pub const CTX_VS_GET_SHADER: u32 = 76;
pub const CTX_IA_GET_INPUT_LAYOUT: u32 = 78;
pub const CTX_IA_GET_VERTEX_BUFFERS: u32 = 79;
pub const CTX_IA_GET_INDEX_BUFFER: u32 = 80;
pub const CTX_IA_GET_PRIMITIVE_TOPOLOGY: u32 = 83;
pub const CTX_OM_GET_RENDER_TARGETS: u32 = 89;
pub const CTX_OM_GET_BLEND_STATE: u32 = 91;
pub const CTX_RS_GET_STATE: u32 = 94;
pub const CTX_RS_GET_VIEWPORTS: u32 = 95;
pub const CTX_RS_GET_SCISSOR_RECTS: u32 = 96;

// -------------------------------------------------------------- D3D11 enums

const MAP_WRITE_DISCARD: u32 = 4;
const USAGE_DYNAMIC: u32 = 2;
const BIND_VERTEX_BUFFER: u32 = 0x1;
const BIND_INDEX_BUFFER: u32 = 0x2;
const BIND_CONSTANT_BUFFER: u32 = 0x4;
const BIND_SHADER_RESOURCE: u32 = 0x8;
const CPU_ACCESS_WRITE: u32 = 0x10_000;

const BLEND_SRC_ALPHA: u32 = 5;
const BLEND_INV_SRC_ALPHA: u32 = 6;
const BLEND_ZERO: u32 = 1;
const BLEND_OP_ADD: u32 = 1;
const COLOR_WRITE_ENABLE_ALL: u8 = 0x0f;

const FILL_SOLID: u32 = 3;
const CULL_NONE: u32 = 1;
const FILTER_MIN_MAG_MIP_LINEAR: u32 = 0x15;
const TEXTURE_ADDRESS_CLAMP: u32 = 3;
const COMPARISON_ALWAYS: u32 = 8;

const PRIMITIVE_TOPOLOGY_TRIANGLELIST: u32 = 4;

const FORMAT_R32G32_FLOAT: u32 = 16;
const FORMAT_R8G8B8A8_UNORM: u32 = 28;
const FORMAT_R16_UINT: u32 = 57;

/// Vertex shader input: `POSITION float2`, `TEXCOORD float2`, `COLOR u8x4`.
/// The upstream demo declares `POSITION` as `R32G32B32A32_FLOAT`, which reads
/// 16 bytes where the struct has 8 and overlaps `uv`; `R32G32_FLOAT` is the
/// honest layout and the shader only consumes `xy` either way.
const INPUT_LAYOUT: [InputElement; 3] = [
    InputElement {
        semantic: c"POSITION".as_ptr() as *const u8,
        semantic_index: 0,
        format: FORMAT_R32G32_FLOAT,
        input_slot: 0,
        aligned_byte_offset: 0,
        slot_class: 0,
        instance_step_rate: 0,
    },
    InputElement {
        semantic: c"TEXCOORD".as_ptr() as *const u8,
        semantic_index: 0,
        format: FORMAT_R32G32_FLOAT,
        input_slot: 0,
        aligned_byte_offset: 8,
        slot_class: 0,
        instance_step_rate: 0,
    },
    InputElement {
        semantic: c"COLOR".as_ptr() as *const u8,
        semantic_index: 0,
        format: FORMAT_R8G8B8A8_UNORM,
        input_slot: 0,
        aligned_byte_offset: 16,
        slot_class: 0,
        instance_step_rate: 0,
    },
];

#[repr(C)]
struct InputElement {
    semantic: *const u8,
    semantic_index: u32,
    format: u32,
    input_slot: u32,
    aligned_byte_offset: u32,
    slot_class: u32,
    instance_step_rate: u32,
}

// ------------------------------------------------------- host-testable math

/// The projection the upstream demo uploads: row-major, row-vector convention
/// (`mul(float4(pos, 0, 1), mvp)`), so the translation sits in the last row.
/// `L = 0, R = width, T = 0, B = height` gives `x' = 2x/w - 1`, `y' = 1 - 2y/h`.
pub fn projection(width: f32, height: f32) -> [f32; 16] {
    let (w, h) = if width <= 0.0 || height <= 0.0 {
        (1.0, 1.0)
    } else {
        (width, height)
    };
    [
        2.0 / w,
        0.0,
        0.0,
        0.0,
        0.0,
        -2.0 / h,
        0.0,
        0.0,
        0.0,
        0.0,
        0.5,
        0.0,
        -1.0,
        1.0,
        0.5,
        1.0,
    ]
}

/// `D3D11_RECT` for a nuklear clip rect (`x, y, w, h`), clamped to the range a
/// 32-bit `LONG` scissor can express.
pub fn scissor_rect(clip: [f32; 4]) -> [i32; 4] {
    let clamp = |value: f32| -> i32 {
        if !value.is_finite() {
            0
        } else {
            value.round().clamp(-32768.0, 32767.0) as i32
        }
    };
    let left = clamp(clip[0]);
    let top = clamp(clip[1]);
    let right = clamp(clip[0] + clip[2]);
    let bottom = clamp(clip[1] + clip[3]);
    [left, top, right.max(left), bottom.max(top)]
}

/// Total vertices in a frame; used by the status line and the tests.
pub fn frame_vertices(frame: &Frame) -> usize {
    frame.commands.iter().map(|command| command.vertices.len()).sum()
}

/// One draw call inside the combined per-frame vertex/index buffers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackedDraw {
    pub index_start: u32,
    pub index_count: u32,
    pub clip: [f32; 4],
    pub texture: u32,
}

/// Pack all draw commands of `frame` into a single vertex byte buffer and a
/// single `u16` index byte buffer, rebasing each command's local `0..V` indices
/// by the running vertex offset so a single `Map(WRITE_DISCARD)` per buffer
/// serves the entire frame.
pub fn pack_frame(
    frame: &Frame,
    max_vertex_bytes: usize,
    max_index_bytes: usize,
) -> (Vec<u8>, Vec<u8>, Vec<PackedDraw>) {
    let mut all_vertices = Vec::with_capacity(
        frame_vertices(frame)
            .saturating_mul(VERTEX_SIZE)
            .min(max_vertex_bytes),
    );
    let mut all_indices = Vec::new();
    let mut draws = Vec::with_capacity(frame.commands.len());
    let mut vertex_base = 0usize;
    for command in &frame.commands {
        if command.vertices.is_empty() || command.indices.is_empty() {
            continue;
        }
        let v_bytes = command.vertices.len().saturating_mul(VERTEX_SIZE);
        let i_bytes = command.indices.len().saturating_mul(2);
        if all_vertices.len() + v_bytes > max_vertex_bytes
            || all_indices.len() + i_bytes > max_index_bytes
            || vertex_base + command.vertices.len() > u16::MAX as usize
        {
            break;
        }
        for vertex in &command.vertices {
            all_vertices.extend_from_slice(&vertex.pos[0].to_le_bytes());
            all_vertices.extend_from_slice(&vertex.pos[1].to_le_bytes());
            all_vertices.extend_from_slice(&vertex.uv[0].to_le_bytes());
            all_vertices.extend_from_slice(&vertex.uv[1].to_le_bytes());
            all_vertices.extend_from_slice(&vertex.color);
        }
        let index_start = (all_indices.len() / 2) as u32;
        let base_u16 = vertex_base as u16;
        for &index in &command.indices {
            all_indices.extend_from_slice(&index.wrapping_add(base_u16).to_le_bytes());
        }
        draws.push(PackedDraw {
            index_start,
            index_count: command.indices.len() as u32,
            clip: command.clip,
            texture: command.texture,
        });
        vertex_base += command.vertices.len();
    }
    (all_vertices, all_indices, draws)
}

/// The live layer: swapchain discovery and the D3D11 backend.
#[cfg(windows)]
pub use imp::{find_swapchain, swapchain_desc, Backend, SavedState};

// ------------------------------------------------------------- windows layer

#[cfg(windows)]
mod imp {
    use super::*;
    use hromatite_win::{log, veh};

    /// Read `vtable[index]` for a COM object.
    fn slot(object: usize, index: u32) -> Option<usize> {
        let table = hromatite_win::memory::read_u64(object)? as usize;
        hromatite_win::memory::read_u64(table + (index as usize) * 8)
            .filter(|value| *value >= 0x10000)
            .map(|value| value as usize)
    }

    /// Invoke a COM method. `args[0]` must be `this`; the Win64 ABI ignores the
    /// extra register/stack slots, so one 8-argument transmute serves every
    /// method we call and the callee only reads what it declares.
    ///
    /// # Safety
    /// `function` must be a `stdcall` (Win64) method pointer whose first
    /// parameter is the interface pointer, and `args` must start with `this`.
    /// The argument *values* are the caller's responsibility — a wrong pointer
    /// is a fault the VEH turns into `Err`.
    unsafe fn invoke(function: usize, args: &[u64; 8]) -> i32 {
        // SAFETY: the transmute is the contract documented above — `function`
        // is a Win64 method pointer and `args[0]` is its `this`.
        let method: unsafe extern "system" fn(
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
        ) -> i32 = unsafe { core::mem::transmute(function) };
        // SAFETY: the caller contract above — `function` is a Win64 method
        // pointer and `args[0]` is its `this`.
        unsafe {
            method(
                args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7],
            )
        }
    }

    /// Call a COM method, returning its `HRESULT` (`i32::MIN` when the slot or
    /// the call itself is unusable).
    fn call(function: usize, args: &[u64; 8]) -> i32 {
        let result = veh::call_guarded(|| {
            // SAFETY: `invoke`'s contract — `function` came from a COM vtable
            // slot and `args[0]` is the interface pointer for that slot.
            unsafe { invoke(function, args) }
        });
        result.unwrap_or(i32::MIN)
    }

    /// `object->index(args…)` for methods returning `HRESULT`.
    fn method(object: usize, index: u32, args: &[u64]) -> i32 {
        let Some(function) = slot(object, index) else {
            return i32::MIN;
        };
        let mut all = [0u64; 8];
        all[0] = object as u64;
        let count = args.len().min(7);
        all[1..=count].copy_from_slice(&args[..count]);
        call(function, &all)
    }

    /// `object->index(args…)` for `void` COM methods (`ID3D11DeviceContext`
    /// setters/getters, `Unmap`, `UpdateSubresource`, `Release`). Returns `true`
    /// when the slot exists and the call returned without a VEH fault.
    fn method_void(object: usize, index: u32, args: &[u64]) -> bool {
        let Some(function) = slot(object, index) else {
            return false;
        };
        let mut all = [0u64; 8];
        all[0] = object as u64;
        let count = args.len().min(7);
        all[1..=count].copy_from_slice(&args[..count]);
        veh::call_guarded(|| {
            // SAFETY: `invoke`'s contract — `function` came from a COM vtable
            // slot and `args[0]` is the interface pointer for that slot.
            unsafe { invoke(function, &all) }
        })
        .is_ok()
    }

    fn succeeded(hr: i32) -> bool {
        hr >= 0
    }

    // ----------------------------------------------------------- discovery

    /// Find the game's `IDXGISwapChain` by scanning writable memory for a
    /// pointer into `dxgi.dll` whose table looks like the swapchain vtable.
    ///
    /// The object lives in the heap (Unity's `GfxDeviceD3D11`), the table lives
    /// in `dxgi.dll`'s read-only data, so the scan only accepts qwords that
    /// land inside the module. Candidates are then validated by a real
    /// `GetDesc` call, which is the definitive test.
    pub fn find_swapchain() -> Option<usize> {
        let (base, size) = hromatite_win::procs::own_module("dxgi.dll")?;
        let end = base.checked_add(size)?;
        let regions = hromatite_win::procs::regions(8192);
        // Executable ranges of dxgi.dll: a swapchain slot must point there.
        let exec: Vec<(usize, usize)> = regions
            .iter()
            .filter(|region| {
                region.executable() && region.base >= base && region.base < end
            })
            .map(|region| (region.base, region.base + region.size))
            .collect();
        if exec.is_empty() {
            log::log_tag(
                "d3d11",
                &format!(
                    "find: dxgi.dll {base:#x}+{size:#x} has no executable region \
                     ({} regions walked)",
                    regions.len()
                ),
            );
            return None;
        }
        let in_exec = |value: usize| exec.iter().any(|&(lo, hi)| value >= lo && value < hi);

        let mut scanned = 0usize;
        // Unity's heap is large; the swapchain object can sit anywhere in it, so
        // the budget has to cover the whole writable heap rather than a corner.
        const BUDGET: usize = 4_000_000_000;
        let mut candidates = 0usize;
        for region in regions {
            if scanned >= BUDGET {
                log::log_tag("d3d11", &format!(
                    "find: budget exhausted after {scanned} bytes, {candidates} candidate(s)"
                ));
                break;
            }
            if !writable(region.protect) {
                continue;
            }
            let mut offset = 0usize;
            while offset < region.size && scanned < BUDGET {
                let want = (region.size - offset).min(0x10000);
                let Some(chunk) = hromatite_win::memory::read_bytes(region.base + offset, want) else {
                    offset += want;
                    scanned += want;
                    continue;
                };
                let (words, _) = chunk.as_chunks::<8>();
                for (index, word) in words.iter().enumerate() {
                    let table = u64::from_le_bytes(*word) as usize;
                    if table < base || table >= end {
                        continue;
                    }
                    let object = region.base + offset + index * 8;
                    if object == table {
                        continue;
                    }
                    candidates += 1;
                    if let Some(found) = validate_table(object, table, in_exec) {
                        log::log_tag(
                            "d3d11",
                            &format!("find: swapchain object {object:#x}, vtable {table:#x}"),
                        );
                        return Some(found);
                    }
                }
                offset += want;
                scanned += want;
            }
        }
        log::log_tag(
            "d3d11",
            &format!(
                "find: no swapchain after {scanned} bytes, {candidates} candidate(s), \
                 dxgi exec region(s) {}",
                exec.len()
            ),
        );
        None
    }

    fn writable(protect: u32) -> bool {
        matches!(protect & 0xFF, 0x04 | 0x02 | 0x06 | 0x08 | 0x40 | 0x80)
    }

    /// `object`'s first qword is `table`; check the swapchain shape and then
    /// prove it with `GetDesc`.
    fn validate_table(
        object: usize,
        table: usize,
        in_exec: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let mut slots = [0u64; SWAPCHAIN_SLOTS as usize];
        for (index, slot) in slots.iter_mut().enumerate() {
            let value = hromatite_win::memory::read_u64(table + index * 8)? as usize;
            if value < 0x10000 || !in_exec(value) {
                return None;
            }
            *slot = value as u64;
        }
        // `GetDesc` (slot 12) fills a DXGI_SWAP_CHAIN_DESC; a success code is the
        // proof that this is really a swapchain and not a same-shaped object.
        let mut desc = [0u8; 128];
        let hr = method(object, SWAPCHAIN_GET_DESC, &[(desc.as_mut_ptr() as u64)]);
        if !succeeded(hr) {
            log::log_tag(
                "d3d11",
                &format!("find: {object:#x} has a swapchain-shaped table but GetDesc={hr:#x}"),
            );
            return None;
        }
        Some(object)
    }

    /// Query `IDXGISwapChain::GetDesc` for `(BufferDesc.Width, BufferDesc.Height, OutputWindow)`.
    pub fn swapchain_desc(swapchain: usize) -> Option<(u32, u32, isize)> {
        if swapchain == 0 {
            return None;
        }
        let mut desc = [0u8; 128];
        let hr = method(swapchain, SWAPCHAIN_GET_DESC, &[(desc.as_mut_ptr() as u64)]);
        if !succeeded(hr) {
            return None;
        }
        let width = u32::from_le_bytes([desc[0], desc[1], desc[2], desc[3]]);
        let height = u32::from_le_bytes([desc[4], desc[5], desc[6], desc[7]]);
        let hwnd = isize::from_le_bytes([
            desc[48], desc[49], desc[50], desc[51], desc[52], desc[53], desc[54], desc[55],
        ]);
        Some((width, height, hwnd))
    }

    // -------------------------------------------------------------- state

    #[repr(C)]
    struct BufferDesc {
        byte_width: u32,
        usage: u32,
        bind_flags: u32,
        cpu_access_flags: u32,
        misc_flags: u32,
        structure_byte_stride: u32,
    }

    #[repr(C)]
    struct SubResourceData {
        sys_mem: usize,
        sys_mem_pitch: u32,
        sys_mem_slice_pitch: u32,
    }

    #[repr(C)]
    struct RasterizerDesc {
        fill_mode: u32,
        cull_mode: u32,
        front_counter_clockwise: i32,
        depth_bias: i32,
        depth_bias_clamp: f32,
        slope_scaled_depth_bias: f32,
        depth_clip_enable: i32,
        scissor_enable: i32,
        multisample_enable: i32,
        antialiased_line_enable: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RenderTargetBlendDesc {
        blend_enable: i32,
        src_blend: u32,
        dest_blend: u32,
        blend_op: u32,
        src_blend_alpha: u32,
        dest_blend_alpha: u32,
        blend_op_alpha: u32,
        render_target_write_mask: u8,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct BlendDesc {
        alpha_to_coverage_enable: i32,
        independent_blend_enable: i32,
        render_targets: [RenderTargetBlendDesc; 8],
    }

    #[repr(C)]
    struct SamplerDesc {
        filter: u32,
        address_u: u32,
        address_v: u32,
        address_w: u32,
        mip_lod_bias: f32,
        max_anisotropy: u32,
        comparison_func: u32,
        border_color: [f32; 4],
        min_lod: f32,
        max_lod: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Viewport {
        top_left_x: f32,
        top_left_y: f32,
        width: f32,
        height: f32,
        min_depth: f32,
        max_depth: f32,
    }

    impl Viewport {
        const ZERO: Viewport = Viewport {
            top_left_x: 0.0,
            top_left_y: 0.0,
            width: 0.0,
            height: 0.0,
            min_depth: 0.0,
            max_depth: 1.0,
        };
    }

    #[repr(C)]
    struct Mapped {
        data: usize,
        row_pitch: u32,
        depth_pitch: u32,
    }

    const MAX_VERTEX_BUFFER: u32 = 64 * 1024;
    const MAX_INDEX_BUFFER: u32 = 64 * 1024;
    const CONSTANT_BUFFER_SIZE: u32 = 64;
    /// `D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE`.
    const MAX_VIEWPORTS: usize = 16;

    /// Drop one COM reference if `object` is non-null.
    fn release(object: usize) {
        if object != 0 {
            let _ = method_void(object, IUNKNOWN_RELEASE, &[]);
        }
    }

    /// The live D3D11 objects. Raw COM pointers; all of them belong to the
    /// device that owns the swapchain, so they stay alive for the process.
    ///
    /// Note that no persistent `ID3D11RenderTargetView` on backbuffer 0 is kept
    /// here across frames: DXGI requires all backbuffer views to be released
    /// before `IDXGISwapChain::ResizeBuffers` can succeed, so `draw` acquires
    /// and releases the backbuffer RTV within each frame.
    pub struct Backend {
        swapchain: usize,
        device: usize,
        context: usize,
        vertex_shader: usize,
        pixel_shader: usize,
        input_layout: usize,
        vertex_buffer: usize,
        index_buffer: usize,
        constant_buffer: usize,
        blend_state: usize,
        rasterizer_state: usize,
        sampler_state: usize,
        white_view: usize,
        font_view: usize,
        width: f32,
        height: f32,
    }

    /// State captured at the start of a frame and restored at the end.
    #[derive(Clone, Copy)]
    pub struct SavedState {
        render_target: usize,
        depth_stencil: usize,
        blend_state: usize,
        blend_factor: [f32; 4],
        sample_mask: u32,
        rasterizer_state: usize,
        viewport_count: u32,
        viewports: [Viewport; MAX_VIEWPORTS],
        scissor_count: u32,
        scissors: [[i32; 4]; MAX_VIEWPORTS],
        input_layout: usize,
        vertex_buffer: usize,
        vertex_stride: u32,
        vertex_offset: u32,
        index_buffer: usize,
        index_format: u32,
        index_offset: u32,
        topology: u32,
        vertex_shader: usize,
        pixel_shader: usize,
        vs_constant_buffer: usize,
        ps_shader_resource: usize,
        ps_sampler: usize,
    }

    impl Backend {
        /// Acquire the device behind `swapchain` and build every object the UI
        /// needs. Returns `Err` with a log line naming the failed step; any
        /// COM handles created before the failure are released by `Drop`.
        pub fn new(swapchain: usize) -> Result<Backend, String> {
            let mut device = 0usize;
            let mut iid = IID_ID3D11_DEVICE;
            let hr = method(
                swapchain,
                SWAPCHAIN_GET_DEVICE,
                &[(iid.as_mut_ptr() as u64), (&mut device as *mut usize as u64)],
            );
            if !succeeded(hr) || device == 0 {
                return Err(format!("IDXGISwapChain::GetDevice failed: {hr:#x}"));
            }
            log::log_tag("d3d11", &format!("GetDevice -> device {device:#x} (hr {hr:#x})"));

            let mut backend = Backend {
                swapchain,
                device,
                context: 0,
                vertex_shader: 0,
                pixel_shader: 0,
                input_layout: 0,
                vertex_buffer: 0,
                index_buffer: 0,
                constant_buffer: 0,
                blend_state: 0,
                rasterizer_state: 0,
                sampler_state: 0,
                white_view: 0,
                font_view: 0,
                width: 1.0,
                height: 1.0,
            };

            // `ID3D11Device::GetImmediateContext` is void and lives at slot 40
            // (after the 3 `IUnknown` entries and 37 earlier `ID3D11Device`
            // entries). Probe 40 first, then the surrounding getter window;
            // only `GetImmediateContext` writes an out pointer.
            let mut context = 0usize;
            let mut used = DEVICE_GET_IMMEDIATE_CONTEXT;
            for index in [DEVICE_GET_IMMEDIATE_CONTEXT, 39, 38, 41, 37] {
                let mut candidate = 0usize;
                let ok = method_void(device, index, &[(&mut candidate as *mut usize as u64)]);
                log::log_tag(
                    "d3d11",
                    &format!("probe slot {index} -> ok {ok}, out {candidate:#x}"),
                );
                if ok && candidate >= 0x10000 {
                    context = candidate;
                    used = index;
                    break;
                }
            }
            if context == 0 {
                // Dump the surrounding vtable entries with their first bytes: the
                // next live run then settles the index without another rebuild.
                for index in 34..=44u32 {
                    let Some(function) = slot(device, index) else {
                        continue;
                    };
                    let bytes = hromatite_win::memory::read_upto(function, 12);
                    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    log::log_tag(
                        "d3d11",
                        &format!("  slot {index} @ {function:#x}: {}", hex.join(" ")),
                    );
                }
                return Err(format!(
                    "ID3D11Device::GetImmediateContext (slot {used}) failed: no context"
                ));
            }
            backend.context = context;
            log::log_tag("d3d11", &format!("immediate context {context:#x} via slot {used}"));

            backend.vertex_shader = {
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_VERTEX_SHADER,
                    &[
                        (super::super::d3d11_shaders::VERTEX_SHADER.as_ptr() as u64),
                        (super::super::d3d11_shaders::VERTEX_SHADER.len() as u64),
                        0,
                        (&mut value as *mut usize as u64),
                    ],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateVertexShader failed: {hr:#x}"));
                }
                value
            };
            backend.pixel_shader = {
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_PIXEL_SHADER,
                    &[
                        (super::super::d3d11_shaders::PIXEL_SHADER.as_ptr() as u64),
                        (super::super::d3d11_shaders::PIXEL_SHADER.len() as u64),
                        0,
                        (&mut value as *mut usize as u64),
                    ],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreatePixelShader failed: {hr:#x}"));
                }
                value
            };
            backend.input_layout = {
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_INPUT_LAYOUT,
                    &[
                        (INPUT_LAYOUT.as_ptr() as u64),
                        (INPUT_LAYOUT.len() as u64),
                        (super::super::d3d11_shaders::VERTEX_SHADER.as_ptr() as u64),
                        (super::super::d3d11_shaders::VERTEX_SHADER.len() as u64),
                        (&mut value as *mut usize as u64),
                    ],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateInputLayout failed: {hr:#x}"));
                }
                value
            };

            backend.rasterizer_state = {
                let desc = RasterizerDesc {
                    fill_mode: FILL_SOLID,
                    cull_mode: CULL_NONE,
                    front_counter_clockwise: 0,
                    depth_bias: 0,
                    depth_bias_clamp: 0.0,
                    slope_scaled_depth_bias: 0.0,
                    depth_clip_enable: 1,
                    scissor_enable: 1,
                    multisample_enable: 0,
                    antialiased_line_enable: 0,
                };
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_RASTERIZER_STATE,
                    &[((&desc as *const RasterizerDesc) as u64), (&mut value as *mut usize as u64)],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateRasterizerState failed: {hr:#x}"));
                }
                value
            };
            backend.blend_state = {
                let mut desc = BlendDesc {
                    alpha_to_coverage_enable: 0,
                    independent_blend_enable: 0,
                    render_targets: [RenderTargetBlendDesc {
                        blend_enable: 0,
                        src_blend: 0,
                        dest_blend: 0,
                        blend_op: 0,
                        src_blend_alpha: 0,
                        dest_blend_alpha: 0,
                        blend_op_alpha: 0,
                        render_target_write_mask: 0,
                    }; 8],
                };
                desc.render_targets[0] = RenderTargetBlendDesc {
                    blend_enable: 1,
                    src_blend: BLEND_SRC_ALPHA,
                    dest_blend: BLEND_INV_SRC_ALPHA,
                    blend_op: BLEND_OP_ADD,
                    src_blend_alpha: BLEND_INV_SRC_ALPHA,
                    dest_blend_alpha: BLEND_ZERO,
                    blend_op_alpha: BLEND_OP_ADD,
                    render_target_write_mask: COLOR_WRITE_ENABLE_ALL,
                };
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_BLEND_STATE,
                    &[((&desc as *const BlendDesc) as u64), (&mut value as *mut usize as u64)],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateBlendState failed: {hr:#x}"));
                }
                value
            };
            backend.sampler_state = {
                let desc = SamplerDesc {
                    filter: FILTER_MIN_MAG_MIP_LINEAR,
                    address_u: TEXTURE_ADDRESS_CLAMP,
                    address_v: TEXTURE_ADDRESS_CLAMP,
                    address_w: TEXTURE_ADDRESS_CLAMP,
                    mip_lod_bias: 0.0,
                    max_anisotropy: 0,
                    comparison_func: COMPARISON_ALWAYS,
                    border_color: [0.0; 4],
                    min_lod: 0.0,
                    max_lod: f32::MAX,
                };
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_SAMPLER_STATE,
                    &[((&desc as *const SamplerDesc) as u64), (&mut value as *mut usize as u64)],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateSamplerState failed: {hr:#x}"));
                }
                value
            };
            backend.vertex_buffer = {
                let desc = BufferDesc {
                    byte_width: MAX_VERTEX_BUFFER,
                    usage: USAGE_DYNAMIC,
                    bind_flags: BIND_VERTEX_BUFFER,
                    cpu_access_flags: CPU_ACCESS_WRITE,
                    misc_flags: 0,
                    structure_byte_stride: 0,
                };
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_BUFFER,
                    &[((&desc as *const BufferDesc) as u64), 0, (&mut value as *mut usize as u64)],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateBuffer(vertex) failed: {hr:#x}"));
                }
                value
            };
            backend.index_buffer = {
                let desc = BufferDesc {
                    byte_width: MAX_INDEX_BUFFER,
                    usage: USAGE_DYNAMIC,
                    bind_flags: BIND_INDEX_BUFFER,
                    cpu_access_flags: CPU_ACCESS_WRITE,
                    misc_flags: 0,
                    structure_byte_stride: 0,
                };
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_BUFFER,
                    &[((&desc as *const BufferDesc) as u64), 0, (&mut value as *mut usize as u64)],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateBuffer(index) failed: {hr:#x}"));
                }
                value
            };
            backend.constant_buffer = {
                let desc = BufferDesc {
                    byte_width: CONSTANT_BUFFER_SIZE,
                    usage: USAGE_DYNAMIC,
                    bind_flags: BIND_CONSTANT_BUFFER,
                    cpu_access_flags: CPU_ACCESS_WRITE,
                    misc_flags: 0,
                    structure_byte_stride: 0,
                };
                let matrix = projection(1.0, 1.0);
                let data = SubResourceData {
                    sys_mem: ((&matrix as *const [f32; 16]) as usize),
                    sys_mem_pitch: 0,
                    sys_mem_slice_pitch: 0,
                };
                let mut value = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_BUFFER,
                    &[
                        ((&desc as *const BufferDesc) as u64),
                        ((&data as *const SubResourceData) as u64),
                        (&mut value as *mut usize as u64),
                    ],
                );
                if !succeeded(hr) || value == 0 {
                    return Err(format!("CreateBuffer(constant) failed: {hr:#x}"));
                }
                value
            };
            backend.white_view = {
                // A 1x1 white texture: nuklear's `tex_null`, used for every
                // filled shape that has no atlas of its own.
                let desc = Texture2dDesc {
                    width: 1,
                    height: 1,
                    mip_levels: 1,
                    array_size: 1,
                    format: FORMAT_R8G8B8A8_UNORM,
                    sample_count: 1,
                    sample_quality: 0,
                    usage: 0, // DEFAULT
                    bind_flags: BIND_SHADER_RESOURCE,
                    cpu_access_flags: 0,
                    misc_flags: 0,
                };
                let pixel: [u8; 4] = [255, 255, 255, 255];
                let data = SubResourceData {
                    sys_mem: (pixel.as_ptr() as usize),
                    sys_mem_pitch: 4,
                    sys_mem_slice_pitch: 0,
                };
                let mut texture = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_TEXTURE2D,
                    &[
                        ((&desc as *const Texture2dDesc) as u64),
                        ((&data as *const SubResourceData) as u64),
                        (&mut texture as *mut usize as u64),
                    ],
                );
                if !succeeded(hr) || texture == 0 {
                    return Err(format!("CreateTexture2D(white) failed: {hr:#x}"));
                }
                let mut view = 0usize;
                let hr = method(
                    device,
                    DEVICE_CREATE_SHADER_RESOURCE_VIEW,
                    &[(texture as u64), 0, (&mut view as *mut usize as u64)],
                );
                release(texture);
                if !succeeded(hr) || view == 0 {
                    return Err(format!("CreateShaderResourceView(white) failed: {hr:#x}"));
                }
                view
            };

            log::log_tag(
                "d3d11",
                &format!(
                    "backend ready: device {device:#x} context {context:#x}, \
                     shader {:#x}/{:#x}",
                    backend.vertex_shader, backend.pixel_shader
                ),
            );
            Ok(backend)
        }

        /// Upload the baked nuklear font atlas once.
        pub fn upload_font(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<(), String> {
            if rgba.len() < (width as usize * height as usize * 4) {
                return Err(format!(
                    "font atlas is {} bytes, need {}",
                    rgba.len(),
                    width as usize * height as usize * 4
                ));
            }
            let desc = Texture2dDesc {
                width,
                height,
                mip_levels: 1,
                array_size: 1,
                format: FORMAT_R8G8B8A8_UNORM,
                sample_count: 1,
                sample_quality: 0,
                usage: 0,
                bind_flags: BIND_SHADER_RESOURCE,
                cpu_access_flags: 0,
                misc_flags: 0,
            };
            let data = SubResourceData {
                sys_mem: rgba.as_ptr() as usize,
                sys_mem_pitch: width * 4,
                sys_mem_slice_pitch: 0,
            };
            let mut texture = 0usize;
            let hr = method(
                self.device,
                DEVICE_CREATE_TEXTURE2D,
                &[
                    ((&desc as *const Texture2dDesc) as u64),
                    ((&data as *const SubResourceData) as u64),
                    (&mut texture as *mut usize as u64),
                ],
            );
            if !succeeded(hr) || texture == 0 {
                return Err(format!("CreateTexture2D(font) failed: {hr:#x}"));
            }
            let mut view = 0usize;
            let hr = method(
                self.device,
                DEVICE_CREATE_SHADER_RESOURCE_VIEW,
                &[(texture as u64), 0, (&mut view as *mut usize as u64)],
            );
            release(texture);
            if !succeeded(hr) || view == 0 {
                return Err(format!("CreateShaderResourceView(font) failed: {hr:#x}"));
            }
            release(self.font_view);
            self.font_view = view;
            Ok(())
        }

        /// Store the render-target size so the projection matches the frame.
        pub fn set_size(&mut self, width: f32, height: f32) {
            if width > 0.0 && height > 0.0 && (width != self.width || height != self.height) {
                self.width = width;
                self.height = height;
            }
        }

        /// Acquire a temporary RTV on backbuffer 0 for the current frame.
        /// Caller must `release` it before leaving the frame so `ResizeBuffers`
        /// is never blocked by a lingering backbuffer reference.
        fn acquire_rtv(&self) -> usize {
            if self.swapchain == 0 {
                return 0;
            }
            let mut backbuffer = 0usize;
            let mut tex_iid = IID_ID3D11_TEXTURE2D;
            let hr = method(
                self.swapchain,
                SWAPCHAIN_GET_BUFFER,
                &[
                    0,
                    (tex_iid.as_mut_ptr() as u64),
                    (&mut backbuffer as *mut usize as u64),
                ],
            );
            if !succeeded(hr) || backbuffer == 0 {
                return 0;
            }
            let mut rtv = 0usize;
            let rtv_hr = method(
                self.device,
                DEVICE_CREATE_RENDER_TARGET_VIEW,
                &[(backbuffer as u64), 0, (&mut rtv as *mut usize as u64)],
            );
            release(backbuffer);
            if succeeded(rtv_hr) && rtv != 0 {
                rtv
            } else {
                0
            }
        }

        pub fn save(&self) -> Option<SavedState> {
            // Query non-refcounted rasterizer arrays first so an unusable
            // context fails before any COM refcounts are incremented.
            // `RSGetViewports` and `RSGetScissorRects` take `[in, out]` counts:
            // passing `MAX_VIEWPORTS` (16) captures the full pipeline array.
            let mut viewport_count = MAX_VIEWPORTS as u32;
            let mut viewports = [Viewport::ZERO; MAX_VIEWPORTS];
            viewports[0].width = self.width;
            viewports[0].height = self.height;
            if !method_void(
                self.context,
                CTX_RS_GET_VIEWPORTS,
                &[
                    (&mut viewport_count as *mut u32 as u64),
                    (viewports.as_mut_ptr() as u64),
                ],
            ) {
                return None;
            }
            let mut scissor_count = MAX_VIEWPORTS as u32;
            let mut scissors = [[0i32; 4]; MAX_VIEWPORTS];
            if !method_void(
                self.context,
                CTX_RS_GET_SCISSOR_RECTS,
                &[
                    (&mut scissor_count as *mut u32 as u64),
                    (scissors.as_mut_ptr() as u64),
                ],
            ) {
                return None;
            }
            let mut render_target = 0usize;
            let mut depth_stencil = 0usize;
            if !method_void(
                self.context,
                CTX_OM_GET_RENDER_TARGETS,
                &[
                    1,
                    (&mut render_target as *mut usize as u64),
                    (&mut depth_stencil as *mut usize as u64),
                ],
            ) {
                return None;
            }
            let mut blend_state = 0usize;
            let mut blend_factor = [1.0f32; 4];
            let mut sample_mask = 0xffff_ffffu32;
            let _ = method_void(
                self.context,
                CTX_OM_GET_BLEND_STATE,
                &[
                    (&mut blend_state as *mut usize as u64),
                    (blend_factor.as_mut_ptr() as u64),
                    (&mut sample_mask as *mut u32 as u64),
                ],
            );
            let mut rasterizer_state = 0usize;
            let _ = method_void(
                self.context,
                CTX_RS_GET_STATE,
                &[(&mut rasterizer_state as *mut usize as u64)],
            );
            let mut input_layout = 0usize;
            let _ = method_void(
                self.context,
                CTX_IA_GET_INPUT_LAYOUT,
                &[(&mut input_layout as *mut usize as u64)],
            );
            let mut vertex_buffer = 0usize;
            let mut vertex_stride = 0u32;
            let mut vertex_offset = 0u32;
            let _ = method_void(
                self.context,
                CTX_IA_GET_VERTEX_BUFFERS,
                &[
                    0,
                    1,
                    (&mut vertex_buffer as *mut usize as u64),
                    (&mut vertex_stride as *mut u32 as u64),
                    (&mut vertex_offset as *mut u32 as u64),
                ],
            );
            let mut index_buffer = 0usize;
            let mut index_format = FORMAT_R16_UINT;
            let mut index_offset = 0u32;
            let _ = method_void(
                self.context,
                CTX_IA_GET_INDEX_BUFFER,
                &[
                    (&mut index_buffer as *mut usize as u64),
                    (&mut index_format as *mut u32 as u64),
                    (&mut index_offset as *mut u32 as u64),
                ],
            );
            let mut topology = PRIMITIVE_TOPOLOGY_TRIANGLELIST;
            let _ = method_void(
                self.context,
                CTX_IA_GET_PRIMITIVE_TOPOLOGY,
                &[(&mut topology as *mut u32 as u64)],
            );
            let mut vertex_shader = 0usize;
            let _ = method_void(
                self.context,
                CTX_VS_GET_SHADER,
                &[(&mut vertex_shader as *mut usize as u64), 0, 0],
            );
            let mut pixel_shader = 0usize;
            let _ = method_void(
                self.context,
                CTX_PS_GET_SHADER,
                &[(&mut pixel_shader as *mut usize as u64), 0, 0],
            );
            let mut vs_constant_buffer = 0usize;
            let _ = method_void(
                self.context,
                CTX_VS_GET_CONSTANT_BUFFERS,
                &[0, 1, (&mut vs_constant_buffer as *mut usize as u64)],
            );
            let mut ps_shader_resource = 0usize;
            let _ = method_void(
                self.context,
                CTX_PS_GET_SHADER_RESOURCES,
                &[0, 1, (&mut ps_shader_resource as *mut usize as u64)],
            );
            let mut ps_sampler = 0usize;
            let _ = method_void(
                self.context,
                CTX_PS_GET_SAMPLERS,
                &[0, 1, (&mut ps_sampler as *mut usize as u64)],
            );
            Some(SavedState {
                render_target,
                depth_stencil,
                blend_state,
                blend_factor,
                sample_mask,
                rasterizer_state,
                viewport_count,
                viewports,
                scissor_count,
                scissors,
                input_layout,
                vertex_buffer,
                vertex_stride,
                vertex_offset,
                index_buffer,
                index_format,
                index_offset,
                topology,
                vertex_shader,
                pixel_shader,
                vs_constant_buffer,
                ps_shader_resource,
                ps_sampler,
            })
        }

        pub fn restore(&self, state: &SavedState) {
            let _ = method_void(
                self.context,
                CTX_OM_SET_RENDER_TARGETS,
                &[
                    1,
                    (&state.render_target as *const usize as u64),
                    (state.depth_stencil as u64),
                ],
            );
            let _ = method_void(
                self.context,
                CTX_OM_SET_BLEND_STATE,
                &[
                    (state.blend_state as u64),
                    (state.blend_factor.as_ptr() as u64),
                    (state.sample_mask as u64),
                ],
            );
            let _ = method_void(
                self.context,
                CTX_RS_SET_STATE,
                &[(state.rasterizer_state as u64)],
            );
            let vp_count = state.viewport_count.min(MAX_VIEWPORTS as u32);
            if vp_count > 0 {
                let _ = method_void(
                    self.context,
                    CTX_RS_SET_VIEWPORTS,
                    &[(vp_count as u64), (state.viewports.as_ptr() as u64)],
                );
            } else {
                let _ = method_void(self.context, CTX_RS_SET_VIEWPORTS, &[0, 0]);
            }
            let sc_count = state.scissor_count.min(MAX_VIEWPORTS as u32);
            if sc_count > 0 {
                let _ = method_void(
                    self.context,
                    CTX_RS_SET_SCISSOR_RECTS,
                    &[(sc_count as u64), (state.scissors.as_ptr() as u64)],
                );
            } else {
                let _ = method_void(self.context, CTX_RS_SET_SCISSOR_RECTS, &[0, 0]);
            }
            let _ = method_void(
                self.context,
                CTX_IA_SET_INPUT_LAYOUT,
                &[(state.input_layout as u64)],
            );
            let _ = method_void(
                self.context,
                CTX_IA_SET_VERTEX_BUFFERS,
                &[
                    0,
                    1,
                    (&state.vertex_buffer as *const usize as u64),
                    (&state.vertex_stride as *const u32 as u64),
                    (&state.vertex_offset as *const u32 as u64),
                ],
            );
            let index_format = if state.index_format == 0 {
                FORMAT_R16_UINT
            } else {
                state.index_format
            };
            let _ = method_void(
                self.context,
                CTX_IA_SET_INDEX_BUFFER,
                &[
                    (state.index_buffer as u64),
                    (index_format as u64),
                    (state.index_offset as u64),
                ],
            );
            let _ = method_void(
                self.context,
                CTX_IA_SET_PRIMITIVE_TOPOLOGY,
                &[(state.topology as u64)],
            );
            let _ = method_void(
                self.context,
                CTX_VS_SET_SHADER,
                &[(state.vertex_shader as u64), 0, 0],
            );
            let _ = method_void(
                self.context,
                CTX_PS_SET_SHADER,
                &[(state.pixel_shader as u64), 0, 0],
            );
            let _ = method_void(
                self.context,
                CTX_VS_SET_CONSTANT_BUFFERS,
                &[0, 1, (&state.vs_constant_buffer as *const usize as u64)],
            );
            let _ = method_void(
                self.context,
                CTX_PS_SET_SHADER_RESOURCES,
                &[0, 1, (&state.ps_shader_resource as *const usize as u64)],
            );
            let _ = method_void(
                self.context,
                CTX_PS_SET_SAMPLERS,
                &[0, 1, (&state.ps_sampler as *const usize as u64)],
            );

            // Every `*Get*` call in `save` increments the COM refcount of the
            // returned interface pointer; balance those references now.
            release(state.render_target);
            release(state.depth_stencil);
            release(state.blend_state);
            release(state.rasterizer_state);
            release(state.input_layout);
            release(state.vertex_buffer);
            release(state.index_buffer);
            release(state.vertex_shader);
            release(state.pixel_shader);
            release(state.vs_constant_buffer);
            release(state.ps_shader_resource);
            release(state.ps_sampler);
        }

        /// Draw one nuklear frame. `state` must be the value [`Backend::save`]
        /// returned for this frame; it is restored by [`Backend::restore`].
        pub fn draw(&mut self, frame: &Frame, state: &SavedState) {
            if frame.commands.is_empty() {
                return;
            }
            let (all_vertices, all_indices, draws) = pack_frame(
                frame,
                MAX_VERTEX_BUFFER as usize,
                MAX_INDEX_BUFFER as usize,
            );
            if draws.is_empty() {
                return;
            }
            if !self.map_data(
                self.vertex_buffer,
                MAX_VERTEX_BUFFER as usize,
                &all_vertices,
            ) || !self.map_data(self.index_buffer, MAX_INDEX_BUFFER as usize, &all_indices)
            {
                return;
            }

            // Projection + viewport.
            let matrix = projection(self.width, self.height);
            // SAFETY: `matrix` is a live local; the slice covers exactly its 16
            // floats, which is the 64-byte constant buffer's whole extent.
            let matrix_bytes =
                unsafe { core::slice::from_raw_parts(matrix.as_ptr() as *const u8, 64) };
            if !self.map_data(
                self.constant_buffer,
                CONSTANT_BUFFER_SIZE as usize,
                matrix_bytes,
            ) {
                return;
            }

            let own_rtv = self.acquire_rtv();
            let rtv = if own_rtv != 0 {
                own_rtv
            } else {
                state.render_target
            };
            if rtv == 0 {
                return;
            }

            let _ = method_void(
                self.context,
                CTX_OM_SET_RENDER_TARGETS,
                &[1, (&rtv as *const usize as u64), 0],
            );
            let viewport = Viewport {
                top_left_x: 0.0,
                top_left_y: 0.0,
                width: self.width,
                height: self.height,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let _ = method_void(
                self.context,
                CTX_RS_SET_VIEWPORTS,
                &[1, ((&viewport as *const Viewport) as u64)],
            );

            let _ = method_void(
                self.context,
                CTX_IA_SET_PRIMITIVE_TOPOLOGY,
                &[PRIMITIVE_TOPOLOGY_TRIANGLELIST as u64],
            );
            let _ = method_void(
                self.context,
                CTX_IA_SET_INPUT_LAYOUT,
                &[(self.input_layout as u64)],
            );
            let stride = VERTEX_SIZE as u32;
            let offset = 0u32;
            let _ = method_void(
                self.context,
                CTX_IA_SET_VERTEX_BUFFERS,
                &[
                    0,
                    1,
                    (&self.vertex_buffer as *const usize as u64),
                    (&stride as *const u32 as u64),
                    (&offset as *const u32 as u64),
                ],
            );
            let _ = method_void(
                self.context,
                CTX_IA_SET_INDEX_BUFFER,
                &[(self.index_buffer as u64), (FORMAT_R16_UINT as u64), 0],
            );
            let _ = method_void(
                self.context,
                CTX_VS_SET_SHADER,
                &[(self.vertex_shader as u64), 0, 0],
            );
            let _ = method_void(
                self.context,
                CTX_PS_SET_SHADER,
                &[(self.pixel_shader as u64), 0, 0],
            );
            let _ = method_void(
                self.context,
                CTX_VS_SET_CONSTANT_BUFFERS,
                &[0, 1, (&self.constant_buffer as *const usize as u64)],
            );
            let _ = method_void(
                self.context,
                CTX_PS_SET_SAMPLERS,
                &[0, 1, (&self.sampler_state as *const usize as u64)],
            );
            let factor = [1.0f32, 1.0, 1.0, 1.0];
            let _ = method_void(
                self.context,
                CTX_OM_SET_BLEND_STATE,
                &[
                    (self.blend_state as u64),
                    (factor.as_ptr() as u64),
                    0xffff_ffff,
                ],
            );
            let _ = method_void(
                self.context,
                CTX_RS_SET_STATE,
                &[(self.rasterizer_state as u64)],
            );

            for draw in draws {
                let rect = scissor_rect(draw.clip);
                let _ = method_void(
                    self.context,
                    CTX_RS_SET_SCISSOR_RECTS,
                    &[1, (rect.as_ptr() as u64)],
                );
                let view = if draw.texture == 0 || self.font_view == 0 {
                    self.white_view
                } else {
                    self.font_view
                };
                let _ = method_void(
                    self.context,
                    CTX_PS_SET_SHADER_RESOURCES,
                    &[0, 1, (&view as *const usize as u64)],
                );
                let _ = method_void(
                    self.context,
                    CTX_DRAW_INDEXED,
                    &[(draw.index_count as u64), (draw.index_start as u64), 0],
                );
            }

            if own_rtv != 0 {
                let null_rtv = 0usize;
                let _ = method_void(
                    self.context,
                    CTX_OM_SET_RENDER_TARGETS,
                    &[1, (&null_rtv as *const usize as u64), 0],
                );
                release(own_rtv);
            }
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            release(self.font_view);
            release(self.white_view);
            release(self.sampler_state);
            release(self.rasterizer_state);
            release(self.blend_state);
            release(self.constant_buffer);
            release(self.index_buffer);
            release(self.vertex_buffer);
            release(self.input_layout);
            release(self.pixel_shader);
            release(self.vertex_shader);
            release(self.context);
            release(self.device);
        }
    }

    #[repr(C)]
    struct Texture2dDesc {
        width: u32,
        height: u32,
        mip_levels: u32,
        array_size: u32,
        format: u32,
        sample_count: u32,
        sample_quality: u32,
        usage: u32,
        bind_flags: u32,
        cpu_access_flags: u32,
        misc_flags: u32,
    }

    /// `Map(WRITE_DISCARD)` + copy + `Unmap` for a dynamic buffer.
    impl Backend {
        fn map_data(&self, buffer: usize, capacity: usize, data: &[u8]) -> bool {
            if data.len() > capacity {
                return false;
            }
            let mut mapped = Mapped {
                data: 0,
                row_pitch: 0,
                depth_pitch: 0,
            };
            let hr = method(
                self.context,
                CTX_MAP,
                &[
                    (buffer as u64),
                    0,
                    (MAP_WRITE_DISCARD as u64),
                    0,
                    ((&mut mapped as *mut Mapped) as u64),
                ],
            );
            if !succeeded(hr) || mapped.data == 0 {
                return false;
            }
            // SAFETY: `Map` returned writable memory of at least `capacity`
            // bytes and `data.len() <= capacity` was checked above, so the copy
            // stays inside both the source slice and the mapped region.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), mapped.data as *mut u8, data.len());
            }
            method_void(self.context, CTX_UNMAP, &[(buffer as u64), 0])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nk::{DrawCommand, FrameStats, Vertex};

    #[test]
    fn projection_maps_screen_corners() {
        let m = projection(800.0, 600.0);
        // Row-vector convention: x' = 2x/w - 1, y' = 1 - 2y/h.
        let x0 = 0.0 * m[0] + m[12];
        let x1 = 800.0 * m[0] + m[12];
        let y0 = 0.0 * m[5] + m[13];
        let y1 = 600.0 * m[5] + m[13];
        assert!((x0 + 1.0).abs() < 1e-6, "left {x0}");
        assert!((x1 - 1.0).abs() < 1e-6, "right {x1}");
        assert!((y0 - 1.0).abs() < 1e-6, "top {y0}");
        assert!((y1 + 1.0).abs() < 1e-6, "bottom {y1}");
        assert_eq!(m[10], 0.5);
        assert_eq!(m[15], 1.0);
    }

    #[test]
    fn degenerate_size_falls_back_to_a_unit_viewport() {
        let m = projection(0.0, -1.0);
        assert_eq!(m[0], 2.0);
        assert_eq!(m[5], -2.0);
    }

    #[test]
    fn scissor_rect_is_ordered_and_clamped() {
        assert_eq!(scissor_rect([10.0, 20.0, 30.0, 40.0]), [10, 20, 40, 60]);
        assert_eq!(scissor_rect([10.4, 20.5, 5.0, 5.0]), [10, 21, 15, 26]);
        // Inverted or huge clips must not produce a negative extent.
        assert_eq!(scissor_rect([100.0, 100.0, -50.0, -50.0]), [100, 100, 100, 100]);
        let huge = scissor_rect([1.0e9, 1.0e9, 1.0e9, 1.0e9]);
        assert!(huge.iter().all(|value| *value >= -32768 && *value <= 32767));
    }

    #[test]
    fn pack_frame_rebases_indices_across_commands() {
        let v = Vertex {
            pos: [1.0, 2.0],
            uv: [0.5, 0.5],
            color: [10, 20, 30, 40],
        };
        let frame = Frame {
            commands: vec![
                DrawCommand {
                    clip: [0.0, 0.0, 100.0, 50.0],
                    vertices: vec![v, v, v],
                    indices: vec![0, 1, 2],
                    texture: 0,
                },
                DrawCommand {
                    clip: [10.0, 10.0, 80.0, 30.0],
                    vertices: vec![v, v, v, v],
                    indices: vec![0, 1, 2, 0, 2, 3],
                    texture: 1,
                },
            ],
            stats: FrameStats::default(),
        };
        let (vb, ib, draws) = pack_frame(&frame, 64 * 1024, 64 * 1024);
        assert_eq!(vb.len(), 7 * VERTEX_SIZE);
        assert_eq!(ib.len(), 9 * 2);
        assert_eq!(draws.len(), 2);
        assert_eq!(draws[0].index_start, 0);
        assert_eq!(draws[0].index_count, 3);
        assert_eq!(draws[0].texture, 0);
        assert_eq!(draws[1].index_start, 3);
        assert_eq!(draws[1].index_count, 6);
        assert_eq!(draws[1].texture, 1);
        let decoded: Vec<u16> = ib
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        // Second command's local 0..3 indices are rebased by the 3 vertices of
        // the first command.
        assert_eq!(decoded, vec![0, 1, 2, 3, 4, 5, 3, 5, 6]);
    }

    #[test]
    fn vtable_indices_match_the_headers() {
        // A slip here is a crash on the first frame, so the numbers are pinned
        // against the declarations the backend was written from.
        assert_eq!(SWAPCHAIN_PRESENT, 8);
        assert_eq!(SWAPCHAIN_GET_DEVICE, 7);
        assert_eq!(SWAPCHAIN_GET_BUFFER, 9);
        assert_eq!(SWAPCHAIN_GET_DESC, 12);
        assert_eq!(DEVICE_CREATE_RENDER_TARGET_VIEW, 9);
        assert_eq!(DEVICE_GET_IMMEDIATE_CONTEXT, 40);
        assert_eq!(CTX_DRAW_INDEXED, 12);
        assert_eq!(CTX_MAP, 14);
        assert_eq!(CTX_IA_SET_INDEX_BUFFER, 19);
        assert_eq!(CTX_OM_SET_BLEND_STATE, 35);
        assert_eq!(CTX_RS_SET_SCISSOR_RECTS, 45);
        assert_eq!(CTX_UPDATE_SUBRESOURCE, 48);
        assert_eq!(CTX_OM_GET_RENDER_TARGETS, 89);
        assert_eq!(VERTEX_SIZE, 20);
    }

    #[test]
    fn iid_matches_the_header_value() {
        assert_eq!(IID_ID3D11_DEVICE[0], 0xdb);
        assert_eq!(IID_ID3D11_DEVICE[3], 0xdb);
        assert_eq!(u16::from_le_bytes([IID_ID3D11_DEVICE[4], IID_ID3D11_DEVICE[5]]), 0xac77);
        assert_eq!(u16::from_le_bytes([IID_ID3D11_DEVICE[6], IID_ID3D11_DEVICE[7]]), 0x4e88);
        assert_eq!(u32::from_le_bytes([
            IID_ID3D11_TEXTURE2D[0],
            IID_ID3D11_TEXTURE2D[1],
            IID_ID3D11_TEXTURE2D[2],
            IID_ID3D11_TEXTURE2D[3],
        ]), 0x6f15_aaf2);
    }
}
