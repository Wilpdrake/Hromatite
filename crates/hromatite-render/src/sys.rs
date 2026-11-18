//! Raw FFI boundary to `nuklear_shim.c`.
//!
//! Nothing outside this module may mention `unsafe`. Every wrapper takes or
//! returns plain `*mut c_void` handles; the C side tolerates NULL everywhere, so
//! a half-initialised `Nk` cannot turn into undefined behaviour.

use core::ffi::{c_char, c_void};
use core::marker::PhantomData;
use std::sync::OnceLock;

/* ------------------------------------------------------------------ */
/* extern declarations                                                 */
/* ------------------------------------------------------------------ */

unsafe extern "C" {
    fn nk_shim_get_constants(out: *mut RawConstants);

    fn nk_shim_sizeof_context() -> usize;
    fn nk_shim_sizeof_vertex() -> usize;
    fn nk_shim_sizeof_draw_command() -> usize;
    fn nk_shim_context_align() -> usize;
    fn nk_shim_vertex_align() -> usize;
    fn nk_shim_draw_command_align() -> usize;
    fn nk_shim_cmd_size_recommended() -> usize;

    fn nk_shim_font_new(height: f32, out_width: *mut i32, out_height: *mut i32) -> *mut c_void;
    fn nk_shim_font_free(font: *mut c_void);
    fn nk_shim_font_pixels(font: *mut c_void) -> *const u8;
    fn nk_shim_font_width(font: *mut c_void) -> i32;
    fn nk_shim_font_height(font: *mut c_void) -> i32;

    fn nk_shim_buffer_new() -> *mut c_void;
    fn nk_shim_buffer_free(buf: *mut c_void);
    fn nk_shim_buffer_clear(buf: *mut c_void);

    fn nk_shim_context_new(font: *mut c_void, cmd_memory: *mut u8, cmd_size: usize) -> *mut c_void;
    fn nk_shim_context_free(ctx: *mut c_void);
    fn nk_shim_clear(ctx: *mut c_void);
    fn nk_shim_window_get_canvas(ctx: *mut c_void) -> *mut c_void;

    fn nk_shim_input_begin(ctx: *mut c_void);
    fn nk_shim_input_end(ctx: *mut c_void);
    fn nk_shim_input_motion(ctx: *mut c_void, x: f32, y: f32);
    fn nk_shim_input_button(ctx: *mut c_void, button: i32, x: f32, y: f32, down: i32);
    fn nk_shim_input_scroll(ctx: *mut c_void, x: f32, y: f32);
    fn nk_shim_input_key(ctx: *mut c_void, key: i32, down: i32);
    fn nk_shim_input_char(ctx: *mut c_void, c: u8);

    fn nk_shim_begin_titled(
        ctx: *mut c_void,
        name: *const c_char,
        title: *const c_char,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        flags: u32,
    ) -> i32;
    fn nk_shim_end(ctx: *mut c_void);
    fn nk_shim_layout_row_static(ctx: *mut c_void, height: f32, item_width: f32, cols: i32);
    fn nk_shim_layout_row_dynamic(ctx: *mut c_void, height: f32, cols: i32);
    fn nk_shim_layout_row_begin(ctx: *mut c_void, fmt: i32, height: f32, cols: i32);
    fn nk_shim_layout_row_push(ctx: *mut c_void, value: f32);
    fn nk_shim_layout_row_end(ctx: *mut c_void);
    fn nk_shim_label(ctx: *mut c_void, text: *const c_char, align: u32);
    fn nk_shim_text(ctx: *mut c_void, text: *const c_char, len: i32, align: u32);
    fn nk_shim_button_label(ctx: *mut c_void, title: *const c_char) -> i32;
    fn nk_shim_check_label(ctx: *mut c_void, text: *const c_char, active: i32) -> i32;
    fn nk_shim_slider_float(ctx: *mut c_void, min: f32, val: *mut f32, max: f32, step: f32) -> i32;
    fn nk_shim_property_float(
        ctx: *mut c_void,
        name: *const c_char,
        min: f32,
        val: *mut f32,
        max: f32,
        step: f32,
        inc_per_pixel: f32,
    ) -> i32;
    fn nk_shim_combo_begin_label(ctx: *mut c_void, selected: *const c_char, w: f32, h: f32) -> i32;
    fn nk_shim_combo_end(ctx: *mut c_void);
    fn nk_shim_group_begin_titled(
        ctx: *mut c_void,
        id: *const c_char,
        title: *const c_char,
        flags: u32,
    ) -> i32;
    fn nk_shim_group_end(ctx: *mut c_void);
    fn nk_shim_spacing(ctx: *mut c_void, cols: i32);
    fn nk_shim_style_set_font(ctx: *mut c_void, font: *mut c_void);

    fn nk_shim_convert(ctx: *mut c_void, cmds: *mut c_void, verts: *mut c_void, elems: *mut c_void) -> i32;
    fn nk_shim_draw_command_count(ctx: *mut c_void) -> usize;
    fn nk_shim_vertex_count(verts: *mut c_void) -> usize;
    fn nk_shim_element_count(elems: *mut c_void) -> usize;
    fn nk_shim_copy_draw_commands(
        font: *mut c_void,
        ctx: *mut c_void,
        cmds: *mut c_void,
        out: *mut RawDrawCommand,
        max: usize,
    ) -> usize;
    fn nk_shim_copy_vertices(verts: *mut c_void, out: *mut RawVertex, max: usize) -> usize;
    fn nk_shim_copy_elements(elems: *mut c_void, out: *mut u16, max: usize) -> usize;
}

/* ------------------------------------------------------------------ */
/* constants                                                           */
/* ------------------------------------------------------------------ */

/// Mirror of `struct nk_shim_constants` from the shim.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct RawConstants {
    pub key_shift: i32,
    pub key_ctrl: i32,
    pub key_alt: i32,
    pub key_del: i32,
    pub key_enter: i32,
    pub key_tab: i32,
    pub key_backspace: i32,
    pub key_up: i32,
    pub key_down: i32,
    pub key_left: i32,
    pub key_right: i32,

    pub button_left: i32,
    pub button_middle: i32,
    pub button_right: i32,

    pub flag_border: u32,
    pub flag_movable: u32,
    pub flag_scalable: u32,
    pub flag_minimizable: u32,
    pub flag_title: u32,
    pub flag_no_scrollbar: u32,
    pub flag_scroll_auto_hide: u32,
    pub flag_background: u32,

    pub text_left: u32,
    pub text_centered: u32,
    pub text_right: u32,

    pub layout_static: i32,
    pub layout_dynamic: i32,
}

/// nuklear's own enum values, read from the shim once so Rust never hardcodes
/// them (they are properties of the vendored header, not of this crate).
pub fn constants() -> &'static RawConstants {
    static C: OnceLock<RawConstants> = OnceLock::new();
    C.get_or_init(|| {
        let mut raw = RawConstants::default();
        // SAFETY: the shim writes exactly `size_of::<RawConstants>()` bytes and
        // only when the pointer is non-null; `raw` is a valid, aligned buffer.
        unsafe { nk_shim_get_constants(&mut raw) };
        raw
    })
}

/* ------------------------------------------------------------------ */
/* raw payload types                                                   */
/* ------------------------------------------------------------------ */

/// Mirror of `nk_shim_vertex` from the shim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RawVertex {
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// Mirror of `nk_shim_draw_command` from the shim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RawDrawCommand {
    pub clip_x: f32,
    pub clip_y: f32,
    pub clip_w: f32,
    pub clip_h: f32,
    pub elem_count: u32,
    pub texture_id: u32,
}

/* ------------------------------------------------------------------ */
/* aligned ownership of the fixed command buffer                       */
/* ------------------------------------------------------------------ */

/// A byte buffer with an explicit alignment, allocated through `std::alloc`.
///
/// nuklear's fixed command buffer holds `nk_command` and window objects, so it
/// needs pointer alignment; a plain `Vec<u8>` only guarantees 1.
pub struct AlignedBox {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedBox {
    /// Allocate `len` bytes aligned to `align`. `len` must be non-zero.
    pub fn new(len: usize, align: usize) -> Option<AlignedBox> {
        if len == 0 {
            return None;
        }
        let layout = std::alloc::Layout::from_size_align(len, align).ok()?;
        // SAFETY: `layout` is non-zero and valid (checked above), so `alloc`
        // returns either a suitably aligned non-null pointer or null.
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return None;
        }
        Some(AlignedBox { ptr, layout })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for AlignedBox {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` came from `alloc` with exactly this layout and has
            // not been deallocated since; `Layout` is `Copy`.
            unsafe { std::alloc::dealloc(self.ptr, self.layout) };
            self.ptr = core::ptr::null_mut();
        }
    }
}

/* ------------------------------------------------------------------ */
/* font atlas                                                          */
/* ------------------------------------------------------------------ */

/// A borrowed view of the baked font atlas.
///
/// Texture id 1 in a [`crate::nk::DrawCommand`] refers to these pixels.
pub struct FontTexture<'a> {
    ptr: *const u8,
    width: u32,
    height: u32,
    _marker: PhantomData<&'a ()>,
}

impl FontTexture<'_> {
    /// RGBA8888 pixels, `width * height * 4` bytes.
    pub fn data(&self) -> &[u8] {
        // SAFETY: the shim keeps the atlas copy alive for as long as the font
        // handle, and `Nk` owns that handle; the borrow checker ties this value
        // to the `&Nk` it came from.
        unsafe { core::slice::from_raw_parts(self.ptr, self.width as usize * self.height as usize * 4) }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

pub fn font_new(height: f32) -> Option<*mut c_void> {
    // SAFETY: the shim writes at most two ints through the pointers and
    // returns null on failure; passing null for the out-parameters is allowed.
    let ptr = unsafe { nk_shim_font_new(height, core::ptr::null_mut(), core::ptr::null_mut()) };
    if ptr.is_null() { None } else { Some(ptr) }
}

pub fn font_free(font: *mut c_void) {
    // SAFETY: the shim accepts null and frees the handle exactly once; `Nk`
    // drops the handle only after the context that referenced it.
    unsafe { nk_shim_font_free(font) };
}

pub fn font_texture(font: *mut c_void) -> Option<FontTexture<'static>> {
    // SAFETY: all three calls accept null and only read from the handle.
    let (ptr, width, height) = unsafe { (
        nk_shim_font_pixels(font),
        nk_shim_font_width(font),
        nk_shim_font_height(font),
    ) };
    if ptr.is_null() || width <= 0 || height <= 0 {
        return None;
    }
    Some(FontTexture { ptr, width: width as u32, height: height as u32, _marker: PhantomData })
}

/* ------------------------------------------------------------------ */
/* sizes                                                               */
/* ------------------------------------------------------------------ */

pub fn sizeof_context() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_sizeof_context() }
}

pub fn sizeof_vertex() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_sizeof_vertex() }
}

pub fn sizeof_draw_command() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_sizeof_draw_command() }
}

pub fn context_align() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_context_align() }
}

pub fn vertex_align() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_vertex_align() }
}

pub fn draw_command_align() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_draw_command_align() }
}

pub fn cmd_size_recommended() -> usize {
    // SAFETY: pure query, no arguments.
    unsafe { nk_shim_cmd_size_recommended() }
}

/* ------------------------------------------------------------------ */
/* buffers and context                                                 */
/* ------------------------------------------------------------------ */

pub fn buffer_new() -> Option<*mut c_void> {
    // SAFETY: no arguments; the shim returns null when malloc fails.
    let ptr = unsafe { nk_shim_buffer_new() };
    if ptr.is_null() { None } else { Some(ptr) }
}

pub fn buffer_free(buf: *mut c_void) {
    // SAFETY: the shim accepts null and frees the buffer exactly once.
    unsafe { nk_shim_buffer_free(buf) };
}

pub fn buffer_clear(buf: *mut c_void) {
    // SAFETY: the shim accepts null and only resets the buffer's write offset.
    unsafe { nk_shim_buffer_clear(buf) };
}

pub fn context_new(font: *mut c_void, cmd_memory: *mut u8, cmd_size: usize) -> Option<*mut c_void> {
    // SAFETY: `font` and `cmd_memory` are owned by `Nk`, which guarantees they
    // outlive the returned context; the shim copies nothing and keeps the
    // pointers only inside the context it initialises.
    let ptr = unsafe { nk_shim_context_new(font, cmd_memory, cmd_size) };
    if ptr.is_null() { None } else { Some(ptr) }
}

pub fn context_free(ctx: *mut c_void) {
    // SAFETY: the shim accepts null and frees the context exactly once.
    unsafe { nk_shim_context_free(ctx) };
}

pub fn clear(ctx: *mut c_void) {
    // SAFETY: the shim forwards to `nk_clear`, which accepts null.
    unsafe { nk_shim_clear(ctx) };
}

pub fn window_get_canvas(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: the shim forwards to `nk_window_get_canvas`, which accepts null.
    unsafe { nk_shim_window_get_canvas(ctx) }
}

/* ------------------------------------------------------------------ */
/* input                                                               */
/* ------------------------------------------------------------------ */

pub fn input_begin(ctx: *mut c_void) {
    // SAFETY: forwards to `nk_input_begin`, which accepts null.
    unsafe { nk_shim_input_begin(ctx) };
}

pub fn input_end(ctx: *mut c_void) {
    // SAFETY: forwards to `nk_input_end`, which accepts null.
    unsafe { nk_shim_input_end(ctx) };
}

pub fn input_motion(ctx: *mut c_void, x: f32, y: f32) {
    // SAFETY: forwards to `nk_input_motion`; floats are truncated to ints there.
    unsafe { nk_shim_input_motion(ctx, x, y) };
}

pub fn input_button(ctx: *mut c_void, button: i32, x: f32, y: f32, down: bool) {
    // SAFETY: forwards to `nk_input_button`; `down` is mapped to nk_bool.
    unsafe { nk_shim_input_button(ctx, button, x, y, down as i32) };
}

pub fn input_scroll(ctx: *mut c_void, x: f32, y: f32) {
    // SAFETY: forwards to `nk_input_scroll`.
    unsafe { nk_shim_input_scroll(ctx, x, y) };
}

pub fn input_key(ctx: *mut c_void, key: i32, down: bool) {
    // SAFETY: forwards to `nk_input_key`; `down` is mapped to nk_bool.
    unsafe { nk_shim_input_key(ctx, key, down as i32) };
}

pub fn input_char(ctx: *mut c_void, c: u8) {
    // SAFETY: forwards to `nk_input_char`.
    unsafe { nk_shim_input_char(ctx, c) };
}

/* ------------------------------------------------------------------ */
/* windows, layout, widgets                                            */
/* ------------------------------------------------------------------ */

#[allow(clippy::too_many_arguments)]
pub fn begin_titled(
    ctx: *mut c_void,
    name: *const c_char,
    title: *const c_char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    flags: u32,
) -> bool {
    // SAFETY: all pointers are either null-tolerant or point at static,
    // NUL-terminated ASCII literals; the floats are plain coordinates.
    unsafe { nk_shim_begin_titled(ctx, name, title, x, y, w, h, flags) != 0 }
}

pub fn end(ctx: *mut c_void) {
    // SAFETY: forwards to `nk_end`, which accepts null.
    unsafe { nk_shim_end(ctx) };
}

pub fn layout_row_static(ctx: *mut c_void, height: f32, item_width: f32, cols: i32) {
    // SAFETY: forwards to `nk_layout_row_static`.
    unsafe { nk_shim_layout_row_static(ctx, height, item_width, cols) };
}

pub fn layout_row_dynamic(ctx: *mut c_void, height: f32, cols: i32) {
    // SAFETY: forwards to `nk_layout_row_dynamic`.
    unsafe { nk_shim_layout_row_dynamic(ctx, height, cols) };
}

pub fn layout_row_begin(ctx: *mut c_void, fmt: i32, height: f32, cols: i32) {
    // SAFETY: forwards to `nk_layout_row_begin`.
    unsafe { nk_shim_layout_row_begin(ctx, fmt, height, cols) };
}

pub fn layout_row_push(ctx: *mut c_void, value: f32) {
    // SAFETY: forwards to `nk_layout_row_push`.
    unsafe { nk_shim_layout_row_push(ctx, value) };
}

pub fn layout_row_end(ctx: *mut c_void) {
    // SAFETY: forwards to `nk_layout_row_end`.
    unsafe { nk_shim_layout_row_end(ctx) };
}

pub fn label(ctx: *mut c_void, text: *const c_char, align: u32) {
    // SAFETY: `text` points at a static, NUL-terminated ASCII literal.
    unsafe { nk_shim_label(ctx, text, align) };
}

pub fn text(ctx: *mut c_void, text: *const c_char, len: i32, align: u32) {
    // SAFETY: `text` points at `len` readable bytes of ASCII (never
    // NUL-terminated, which is why the length is passed explicitly).
    unsafe { nk_shim_text(ctx, text, len, align) };
}

pub fn button_label(ctx: *mut c_void, title: *const c_char) -> bool {
    // SAFETY: `title` points at a static, NUL-terminated ASCII literal.
    unsafe { nk_shim_button_label(ctx, title) != 0 }
}

pub fn check_label(ctx: *mut c_void, text: *const c_char, active: bool) -> bool {
    // SAFETY: `text` points at a static, NUL-terminated ASCII literal; the
    // shim returns the new state rather than writing through a pointer.
    unsafe { nk_shim_check_label(ctx, text, active as i32) != 0 }
}

pub fn slider_float(ctx: *mut c_void, min: f32, val: &mut f32, max: f32, step: f32) -> bool {
    // SAFETY: `val` is a live local; the shim writes a clamped value back.
    unsafe { nk_shim_slider_float(ctx, min, val, max, step) != 0 }
}

pub fn property_float(
    ctx: *mut c_void,
    name: *const c_char,
    min: f32,
    val: &mut f32,
    max: f32,
    step: f32,
    inc_per_pixel: f32,
) -> bool {
    // SAFETY: `name` points at a static, NUL-terminated ASCII literal and
    // `val` is a live local.
    unsafe { nk_shim_property_float(ctx, name, min, val, max, step, inc_per_pixel) != 0 }
}

pub fn combo_begin_label(ctx: *mut c_void, selected: *const c_char, w: f32, h: f32) -> bool {
    // SAFETY: `selected` points at a static, NUL-terminated ASCII literal.
    unsafe { nk_shim_combo_begin_label(ctx, selected, w, h) != 0 }
}

pub fn combo_end(ctx: *mut c_void) {
    // SAFETY: forwards to `nk_combo_end`.
    unsafe { nk_shim_combo_end(ctx) };
}

pub fn group_begin_titled(
    ctx: *mut c_void,
    id: *const c_char,
    title: *const c_char,
    flags: u32,
) -> bool {
    // SAFETY: both pointers reference static, NUL-terminated ASCII literals.
    unsafe { nk_shim_group_begin_titled(ctx, id, title, flags) != 0 }
}

pub fn group_end(ctx: *mut c_void) {
    // SAFETY: forwards to `nk_group_end`.
    unsafe { nk_shim_group_end(ctx) };
}

pub fn spacing(ctx: *mut c_void, cols: i32) {
    // SAFETY: forwards to `nk_spacing`.
    unsafe { nk_shim_spacing(ctx, cols) };
}

pub fn style_set_font(ctx: *mut c_void, font: *mut c_void) {
    // SAFETY: `font` is the handle returned by [`font_new`] and still owned.
    unsafe { nk_shim_style_set_font(ctx, font) };
}

/* ------------------------------------------------------------------ */
/* conversion                                                          */
/* ------------------------------------------------------------------ */

pub fn convert(ctx: *mut c_void, cmds: *mut c_void, verts: *mut c_void, elems: *mut c_void) -> bool {
    // SAFETY: all four handles are owned by `Nk`; the shim only reads the
    // context and appends to the three buffers.
    unsafe { nk_shim_convert(ctx, cmds, verts, elems) == 0 }
}

/// Number of draw commands produced by the last `nk_convert`.
///
/// nuklear keeps them in a back buffer, so the count lives on the context's
/// draw list rather than in the buffer.
pub fn draw_command_count(ctx: *mut c_void) -> usize {
    // SAFETY: the shim only reads `draw_list.cmd_count` and accepts null.
    unsafe { nk_shim_draw_command_count(ctx) }
}

pub fn vertex_count(verts: *mut c_void) -> usize {
    // SAFETY: the shim accepts null.
    unsafe { nk_shim_vertex_count(verts) }
}

pub fn element_count(elems: *mut c_void) -> usize {
    // SAFETY: the shim accepts null.
    unsafe { nk_shim_element_count(elems) }
}

pub fn copy_draw_commands(
    font: *mut c_void,
    ctx: *mut c_void,
    cmds: *mut c_void,
    out: *mut RawDrawCommand,
    max: usize,
) -> usize {
    // SAFETY: `out` points at `max` contiguous `RawDrawCommand` slots; the shim
    // walks nuklear's draw list and writes at most `max` of them.
    unsafe { nk_shim_copy_draw_commands(font, ctx, cmds, out, max) }
}

pub fn copy_vertices(verts: *mut c_void, out: *mut RawVertex, max: usize) -> usize {
    // SAFETY: `out` points at `max` contiguous `RawVertex` slots; the shim
    // writes at most `max` of them.
    unsafe { nk_shim_copy_vertices(verts, out, max) }
}

pub fn copy_elements(elems: *mut c_void, out: *mut u16, max: usize) -> usize {
    // SAFETY: `out` points at `max` contiguous `u16` slots; the shim writes at
    // most `max` of them.
    unsafe { nk_shim_copy_elements(elems, out, max) }
}

/* ------------------------------------------------------------------ */
/* tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the shim is that these layouts match; if the vendored
    /// header ever changes shape, this fails before any UI code misbehaves.
    #[test]
    fn abi_matches_the_shim() {
        assert_eq!(core::mem::size_of::<RawVertex>(), sizeof_vertex());
        assert_eq!(core::mem::align_of::<RawVertex>(), vertex_align());
        assert_eq!(core::mem::size_of::<RawDrawCommand>(), sizeof_draw_command());
        assert_eq!(core::mem::align_of::<RawDrawCommand>(), draw_command_align());
        assert!(core::mem::size_of::<RawConstants>() > 0);
        assert!(sizeof_context() > 0);
        assert!(cmd_size_recommended() >= 1024);
    }

    #[test]
    fn constants_come_from_the_header() {
        let c = constants();
        assert_eq!(c.button_left, 0);
        assert_ne!(c.flag_border, 0);
        assert_ne!(c.flag_title, 0);
        assert_ne!(c.text_left, c.text_centered);
        // `nk_layout_row_begin` takes `enum nk_layout_format` (NK_DYNAMIC/STATIC),
        // not the NK_LAYOUT_* panel types - mixing them up silently turns a
        // dynamic row into a sub-pixel static one.
        assert_eq!(c.layout_dynamic, 0);
        assert_eq!(c.layout_static, 1);
    }

    #[test]
    fn aligned_box_round_trips() {
        let buf = AlignedBox::new(4096, 16).expect("allocation");
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.as_ptr() as usize % 16, 0);
        assert!(AlignedBox::new(0, 16).is_none());
    }
}
