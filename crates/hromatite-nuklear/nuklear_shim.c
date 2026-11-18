/*
 * nuklear_shim.c - thin C boundary over the vendored nuklear v4.13.4.
 *
 * nuklear is an immediate-mode GUI library with a C89-ish core. Rust could
 * declare the ~300 public functions directly, but most of them take structs by
 * value or pointers, and the interesting state (vertex/index/command buffers)
 * lives inside opaque `nk_buffer`/`nk_context` objects. This shim keeps that
 * state in C and exposes only:
 *
 *   - flat, primitive-typed calls (pointers, ints, floats),
 *   - opaque handles (`void*`) for context / font atlas / buffers,
 *   - bulk copy-out functions for draw commands, vertices and indices.
 *
 * Everything here is plain C99, no globals are shared between contexts, and no
 * function panics: every entry point tolerates NULL handles.
 *
 * Texture ids produced by `nk_shim_copy_draw_commands`:
 *   0 = the null texture (a white pixel) - used for filled shapes,
 *   1 = the baked font atlas - used for glyph quads.
 * The D3D11 backend owns both textures and switches on this id.
 */

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define NK_IMPLEMENTATION
#define NK_INCLUDE_FIXED_TYPES
#define NK_INCLUDE_DEFAULT_ALLOCATOR
#define NK_INCLUDE_STANDARD_IO
#define NK_INCLUDE_VERTEX_BUFFER_OUTPUT
#define NK_INCLUDE_FONT_BAKING
#define NK_INCLUDE_DEFAULT_FONT
#include "nuklear.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ------------------------------------------------------------------ */
/* vertex format shared with Rust                                      */
/* ------------------------------------------------------------------ */

typedef struct {
    float x, y;
    float u, v;
    uint8_t r, g, b, a;
} nk_shim_vertex;

typedef struct {
    float clip_x, clip_y, clip_w, clip_h;
    uint32_t elem_count;
    uint32_t texture_id;
} nk_shim_draw_command;

static const struct nk_draw_vertex_layout_element nk_shim_vertex_layout[] = {
    {NK_VERTEX_POSITION, NK_FORMAT_FLOAT, NK_OFFSETOF(nk_shim_vertex, x)},
    {NK_VERTEX_TEXCOORD, NK_FORMAT_FLOAT, NK_OFFSETOF(nk_shim_vertex, u)},
    {NK_VERTEX_COLOR, NK_FORMAT_R8G8B8A8, NK_OFFSETOF(nk_shim_vertex, r)},
    {NK_VERTEX_LAYOUT_END}
};

/* ------------------------------------------------------------------ */
/* constants copied out of the header (so Rust never hardcodes them)   */
/* ------------------------------------------------------------------ */

struct nk_shim_constants {
    int key_shift, key_ctrl, key_alt, key_del, key_enter, key_tab, key_backspace;
    int key_up, key_down, key_left, key_right;
    int button_left, button_middle, button_right;
    unsigned flag_border, flag_movable, flag_scalable, flag_minimizable, flag_title;
    unsigned flag_no_scrollbar, flag_scroll_auto_hide, flag_background;
    unsigned text_left, text_centered, text_right;
    int layout_static, layout_dynamic;
};

void nk_shim_get_constants(struct nk_shim_constants *out)
{
    if (!out) return;
    memset(out, 0, sizeof(*out));
    out->key_shift = NK_KEY_SHIFT;
    out->key_ctrl = NK_KEY_CTRL;
    out->key_alt = NK_KEY_ALT;
    out->key_del = NK_KEY_DEL;
    out->key_enter = NK_KEY_ENTER;
    out->key_tab = NK_KEY_TAB;
    out->key_backspace = NK_KEY_BACKSPACE;
    out->key_up = NK_KEY_UP;
    out->key_down = NK_KEY_DOWN;
    out->key_left = NK_KEY_LEFT;
    out->key_right = NK_KEY_RIGHT;

    out->button_left = NK_BUTTON_LEFT;
    out->button_middle = NK_BUTTON_MIDDLE;
    out->button_right = NK_BUTTON_RIGHT;

    out->flag_border = NK_WINDOW_BORDER;
    out->flag_movable = NK_WINDOW_MOVABLE;
    out->flag_scalable = NK_WINDOW_SCALABLE;
    out->flag_minimizable = NK_WINDOW_MINIMIZABLE;
    out->flag_title = NK_WINDOW_TITLE;
    out->flag_no_scrollbar = NK_WINDOW_NO_SCROLLBAR;
    out->flag_scroll_auto_hide = NK_WINDOW_SCROLL_AUTO_HIDE;
    out->flag_background = NK_WINDOW_BACKGROUND;

    out->text_left = NK_TEXT_LEFT;
    out->text_centered = NK_TEXT_CENTERED;
    out->text_right = NK_TEXT_RIGHT;

    /* nk_layout_row_begin takes `enum nk_layout_format`, which is NOT the same
     * enum as the NK_LAYOUT_* panel types. */
    out->layout_static = NK_STATIC;
    out->layout_dynamic = NK_DYNAMIC;
}

/* ------------------------------------------------------------------ */
/* sizes / limits                                                      */
/* ------------------------------------------------------------------ */

size_t nk_shim_sizeof_context(void) { return sizeof(struct nk_context); }
size_t nk_shim_sizeof_vertex(void) { return sizeof(nk_shim_vertex); }
size_t nk_shim_sizeof_draw_command(void) { return sizeof(nk_shim_draw_command); }
size_t nk_shim_context_align(void) { return (size_t)NK_ALIGNOF(struct nk_context); }
size_t nk_shim_vertex_align(void) { return (size_t)NK_ALIGNOF(nk_shim_vertex); }
size_t nk_shim_draw_command_align(void) { return (size_t)NK_ALIGNOF(nk_shim_draw_command); }

/* The fixed command/window buffer. nuklear packs both the command stream and
 * the window pool into it, so it must be generous; 256 KiB is ~100x what this
 * UI needs and still fits comfortably into a payload's slack. */
size_t nk_shim_cmd_size_recommended(void) { return 256u * 1024u; }

/* ------------------------------------------------------------------ */
/* font atlas                                                          */
/* ------------------------------------------------------------------ */

struct nk_shim_font {
    struct nk_font_atlas atlas;
    struct nk_font *font;
    nk_handle tex;
    void *pixels;   /* RGBA8888 copy of the baked atlas, kept alive for the backend */
    int width;
    int height;
};

void *nk_shim_font_new(float height, int *out_width, int *out_height)
{
    struct nk_shim_font *f;
    const void *baked;
    size_t bytes;

    if (height <= 0.0f) return 0;
    f = (struct nk_shim_font *)calloc(1, sizeof(*f));
    if (!f) return 0;

    nk_font_atlas_init_default(&f->atlas);
    nk_font_atlas_begin(&f->atlas);
    f->font = nk_font_atlas_add_default(&f->atlas, height, 0);
    if (!f->font) {
        nk_font_atlas_clear(&f->atlas);
        free(f);
        return 0;
    }

    baked = nk_font_atlas_bake(&f->atlas, &f->width, &f->height, NK_FONT_ATLAS_RGBA32);
    if (!baked || f->width <= 0 || f->height <= 0) {
        nk_font_atlas_clear(&f->atlas);
        free(f);
        return 0;
    }

    /* nk_font_atlas_end() releases the atlas' own copy of the pixels, so keep
     * our own for the backend to upload. */
    bytes = (size_t)f->width * (size_t)f->height * 4u;
    f->pixels = malloc(bytes);
    if (!f->pixels) {
        nk_font_atlas_clear(&f->atlas);
        free(f);
        return 0;
    }
    memcpy(f->pixels, baked, bytes);

    f->tex = nk_handle_ptr(f->pixels);
    nk_font_atlas_end(&f->atlas, f->tex, 0);

    if (out_width) *out_width = f->width;
    if (out_height) *out_height = f->height;
    return f;
}

void nk_shim_font_free(void *font)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    if (!f) return;
    nk_font_atlas_clear(&f->atlas);
    free(f->pixels);
    free(f);
}

const void *nk_shim_font_pixels(void *font)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    return f ? f->pixels : 0;
}

int nk_shim_font_width(void *font)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    return f ? f->width : 0;
}

int nk_shim_font_height(void *font)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    return f ? f->height : 0;
}

/* ------------------------------------------------------------------ */
/* convert buffers                                                     */
/* ------------------------------------------------------------------ */

void *nk_shim_buffer_new(void)
{
    struct nk_buffer *b = (struct nk_buffer *)malloc(sizeof(*b));
    if (!b) return 0;
    nk_buffer_init_default(b);
    return b;
}

void nk_shim_buffer_free(void *buf)
{
    struct nk_buffer *b = (struct nk_buffer *)buf;
    if (!b) return;
    nk_buffer_free(b);
    free(b);
}

void nk_shim_buffer_clear(void *buf)
{
    struct nk_buffer *b = (struct nk_buffer *)buf;
    if (!b) return;
    nk_buffer_clear(b);
}

/* ------------------------------------------------------------------ */
/* context                                                             */
/* ------------------------------------------------------------------ */

void *nk_shim_context_new(void *font, void *cmd_memory, size_t cmd_size)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    struct nk_context *ctx;

    if (!f || !f->font || !cmd_memory || cmd_size == 0) return 0;
    ctx = (struct nk_context *)malloc(sizeof(*ctx));
    if (!ctx) return 0;
    if (!nk_init_fixed(ctx, cmd_memory, (nk_size)cmd_size, &f->font->handle)) {
        free(ctx);
        return 0;
    }
    return ctx;
}

void nk_shim_context_free(void *ctx)
{
    struct nk_context *c = (struct nk_context *)ctx;
    if (!c) return;
    nk_free(c);
    free(c);
}

void nk_shim_clear(void *ctx)
{
    nk_clear((struct nk_context *)ctx);
}

void *nk_shim_window_get_canvas(void *ctx)
{
    return (void *)nk_window_get_canvas((const struct nk_context *)ctx);
}

/* ------------------------------------------------------------------ */
/* input                                                               */
/* ------------------------------------------------------------------ */

void nk_shim_input_begin(void *ctx) { nk_input_begin((struct nk_context *)ctx); }
void nk_shim_input_end(void *ctx) { nk_input_end((struct nk_context *)ctx); }

void nk_shim_input_motion(void *ctx, float x, float y)
{
    nk_input_motion((struct nk_context *)ctx, (int)x, (int)y);
}

void nk_shim_input_button(void *ctx, int button, float x, float y, int down)
{
    nk_input_button((struct nk_context *)ctx, (enum nk_buttons)button, (int)x, (int)y, down ? nk_true : nk_false);
}

void nk_shim_input_scroll(void *ctx, float x, float y)
{
    nk_input_scroll((struct nk_context *)ctx, nk_vec2(x, y));
}

void nk_shim_input_key(void *ctx, int key, int down)
{
    nk_input_key((struct nk_context *)ctx, (enum nk_keys)key, down ? nk_true : nk_false);
}

void nk_shim_input_char(void *ctx, char c)
{
    nk_input_char((struct nk_context *)ctx, c);
}

/* ------------------------------------------------------------------ */
/* windows, layout, widgets                                            */
/* ------------------------------------------------------------------ */

int nk_shim_begin_titled(void *ctx, const char *name, const char *title,
    float x, float y, float w, float h, unsigned flags)
{
    return nk_begin_titled((struct nk_context *)ctx, name, title,
        nk_rect(x, y, w, h), (nk_flags)flags) == nk_true ? 1 : 0;
}

void nk_shim_end(void *ctx) { nk_end((struct nk_context *)ctx); }

void nk_shim_layout_row_static(void *ctx, float height, float item_width, int cols)
{
    nk_layout_row_static((struct nk_context *)ctx, height, item_width, cols);
}

void nk_shim_layout_row_dynamic(void *ctx, float height, int cols)
{
    nk_layout_row_dynamic((struct nk_context *)ctx, height, cols);
}

void nk_shim_layout_row_begin(void *ctx, int fmt, float height, int cols)
{
    nk_layout_row_begin((struct nk_context *)ctx, (enum nk_layout_format)fmt, height, cols);
}

void nk_shim_layout_row_push(void *ctx, float value)
{
    nk_layout_row_push((struct nk_context *)ctx, value);
}

void nk_shim_layout_row_end(void *ctx)
{
    nk_layout_row_end((struct nk_context *)ctx);
}

void nk_shim_label(void *ctx, const char *text, unsigned align)
{
    nk_label((struct nk_context *)ctx, text, (nk_flags)align);
}

void nk_shim_text(void *ctx, const char *text, int len, unsigned align)
{
    nk_text((struct nk_context *)ctx, text, len, (nk_flags)align);
}

int nk_shim_button_label(void *ctx, const char *title)
{
    return nk_button_label((struct nk_context *)ctx, title) == nk_true ? 1 : 0;
}

int nk_shim_check_label(void *ctx, const char *text, int active)
{
    return nk_check_label((struct nk_context *)ctx, text, active ? nk_true : nk_false) == nk_true ? 1 : 0;
}

int nk_shim_slider_float(void *ctx, float min, float *val, float max, float step)
{
    return nk_slider_float((struct nk_context *)ctx, min, val, max, step) == nk_true ? 1 : 0;
}

int nk_shim_property_float(void *ctx, const char *name, float min, float *val,
    float max, float step, float inc_per_pixel)
{
    return nk_property_float((struct nk_context *)ctx, name, min, val, max, step,
        inc_per_pixel) == nk_true ? 1 : 0;
}

int nk_shim_combo_begin_label(void *ctx, const char *selected, float w, float h)
{
    return nk_combo_begin_label((struct nk_context *)ctx, selected, nk_vec2(w, h)) == nk_true ? 1 : 0;
}

void nk_shim_combo_end(void *ctx) { nk_combo_end((struct nk_context *)ctx); }

int nk_shim_group_begin_titled(void *ctx, const char *id, const char *title, unsigned flags)
{
    return nk_group_begin_titled((struct nk_context *)ctx, id, title, (nk_flags)flags) == nk_true ? 1 : 0;
}

void nk_shim_group_end(void *ctx) { nk_group_end((struct nk_context *)ctx); }

void nk_shim_spacing(void *ctx, int cols) { nk_spacing((struct nk_context *)ctx, cols); }

void nk_shim_style_set_font(void *ctx, void *font)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    if (!f || !f->font) return;
    nk_style_set_font((struct nk_context *)ctx, &f->font->handle);
}

/* ------------------------------------------------------------------ */
/* conversion to draw commands                                         */
/* ------------------------------------------------------------------ */

static struct nk_convert_config nk_shim_make_config(void)
{
    struct nk_convert_config cfg;
    memset(&cfg, 0, sizeof(cfg));
    cfg.global_alpha = 1.0f;
    cfg.line_AA = NK_ANTI_ALIASING_ON;
    cfg.shape_AA = NK_ANTI_ALIASING_ON;
    cfg.circle_segment_count = 22;
    cfg.arc_segment_count = 22;
    cfg.curve_segment_count = 22;
    /* Shapes sample a white pixel from texture 0; glyphs use texture 1. */
    cfg.tex_null.texture = nk_handle_ptr(0);
    cfg.tex_null.uv = nk_vec2(0.5f, 0.5f);
    cfg.vertex_layout = nk_shim_vertex_layout;
    cfg.vertex_size = sizeof(nk_shim_vertex);
    cfg.vertex_alignment = (nk_size)NK_ALIGNOF(nk_shim_vertex);
    return cfg;
}

int nk_shim_convert(void *ctx, void *cmds, void *verts, void *elems)
{
    struct nk_convert_config cfg = nk_shim_make_config();
    nk_flags res = nk_convert((struct nk_context *)ctx, (struct nk_buffer *)cmds,
        (struct nk_buffer *)verts, (struct nk_buffer *)elems, &cfg);
    return res == NK_CONVERT_SUCCESS ? 0 : -1;
}

size_t nk_shim_buffer_used(void *buf)
{
    struct nk_buffer *b = (struct nk_buffer *)buf;
    if (!b) return 0;
    /* For a front buffer `allocated` is the number of bytes in use;
     * `memory.size` is the capacity, which is what nk_buffer_total returns. */
    return (size_t)b->allocated;
}

size_t nk_shim_draw_command_count(void *ctx)
{
    struct nk_context *c = (struct nk_context *)ctx;
    return c ? (size_t)c->draw_list.cmd_count : 0;
}

size_t nk_shim_vertex_count(void *verts)
{
    return nk_shim_buffer_used(verts) / sizeof(nk_shim_vertex);
}

size_t nk_shim_element_count(void *elems)
{
    return nk_shim_buffer_used(elems) / sizeof(nk_draw_index);
}

size_t nk_shim_copy_draw_commands(void *font, void *ctx, void *cmds,
    nk_shim_draw_command *out, size_t max)
{
    struct nk_shim_font *f = (struct nk_shim_font *)font;
    struct nk_context *c = (struct nk_context *)ctx;
    struct nk_buffer *b = (struct nk_buffer *)cmds;
    const struct nk_draw_command *cmd;
    size_t i = 0;

    if (!c || !b || !out || max == 0) return 0;
    /* nuklear keeps draw commands in a back buffer (filled from the end), so
     * the buffer's `allocated` counter says nothing about them. The public
     * nk_draw_list_foreach iterator is the supported way to walk them. */
    nk_draw_list_foreach(cmd, &c->draw_list, b) {
        if (i >= max) break;
        out[i].clip_x = cmd->clip_rect.x;
        out[i].clip_y = cmd->clip_rect.y;
        out[i].clip_w = cmd->clip_rect.w;
        out[i].clip_h = cmd->clip_rect.h;
        out[i].elem_count = cmd->elem_count;
        out[i].texture_id = (f && cmd->texture.ptr == f->tex.ptr) ? 1u : 0u;
        i++;
    }
    return i;
}

size_t nk_shim_copy_vertices(void *verts, nk_shim_vertex *out, size_t max)
{
    struct nk_buffer *b = (struct nk_buffer *)verts;
    size_t count, bytes;
    if (!b || !out || max == 0) return 0;
    count = nk_shim_buffer_used(verts) / sizeof(nk_shim_vertex);
    if (count > max) count = max;
    bytes = count * sizeof(nk_shim_vertex);
    memcpy(out, nk_buffer_memory_const(b), bytes);
    return count;
}

size_t nk_shim_copy_elements(void *elems, uint16_t *out, size_t max)
{
    struct nk_buffer *b = (struct nk_buffer *)elems;
    const nk_draw_index *src;
    size_t count, i;
    if (!b || !out || max == 0) return 0;
    count = nk_shim_buffer_used(elems) / sizeof(nk_draw_index);
    if (count > max) count = max;
    src = (const nk_draw_index *)nk_buffer_memory_const(b);
    for (i = 0; i < count; ++i)
        out[i] = (uint16_t)src[i];
    return count;
}

#ifdef __cplusplus
}
#endif
