//! In-game ESP drawn by the game's *own* renderer.
//!
//! The stealth requirement decides the architecture: nothing here hooks the
//! graphics stack. There is no `dxgi!Present` detour, no ImGui, no second
//! thread and no overlay window — the only additions are managed objects any
//! Unity game owns (one `Mesh`, one `Material`, one `Vector3[]`) created
//! through il2cpp and one `Graphics.DrawMesh` call per frame from the managed
//! tick that already exists (`LateUpdate` / native `PlayerLoop`).
//!
//! Consequences that matter for detection:
//!   * no new executable page, no trampoline and no patched byte in
//!     `dxgi.dll`/`d3d11.dll`/`d3d12.dll`/`vulkan-1.dll`;
//!   * no new thread, so the thread-start-address heuristic stays clean;
//!   * every allocation is a normal managed object, visible to Unity's GC as
//!     game data rather than as a foreign region;
//!   * the geometry is transformed by the game's own camera, so it lands in the
//!     frame — including the game's own screenshots — with no projection code
//!     of ours.
//!
//! Vertex layout: each line segment is a camera-facing quad (4 vertices) and
//! both windings are indexed, so a material without `Cull Off` still draws it.

use crate::host::Host;
use crate::m3::Metadata;
use crate::telemetry;
use std::ffi::CString;

/// Four vertices per segment (one camera-facing quad).
const VERTS_PER_SEGMENT: usize = 4;
/// Two triangles per winding, so back-face culling can never hide a line.
const INDICES_PER_SEGMENT: usize = 12;
/// 1024 segments is 85 boxes: an 80-player instance plus the crosshair.
pub const MAX_SEGMENTS: usize = 1024;
const MAX_VERTS: usize = MAX_SEGMENTS * VERTS_PER_SEGMENT;
const MAX_INDICES: usize = MAX_SEGMENTS * INDICES_PER_SEGMENT;
/// Il2CppArray: `klass, monitor, bounds, max_length` then the elements.
const ARRAY_DATA: usize = 0x20;

/// Shaders are tried in order: the first one the build actually kept wins.
/// `Hidden/Internal-Colored` is the cheapest unlit path, the rest are fallbacks
/// for builds that stripped it (it is normally used by editor gizmos only).
const SHADERS: [&str; 4] = [
    "Hidden/Internal-Colored",
    "Sprites/Default",
    "Unlit/Color",
    "UI/Default",
];

/// Half a pixel of line, expressed in world units per metre of distance. A
/// 60-degree vertical FOV on a 1080-tall surface is ~0.00107 m/px/m; 1.6 px of
/// that keeps a box readable at any distance without becoming a wall close up.
const HALF_WIDTH_PER_METRE: f32 = 0.0017;
const HALF_WIDTH_MIN: f32 = 0.003;
const HALF_WIDTH_MAX: f32 = 0.06;
/// Rebuilding costs one `get_AllPlayers` plus one `GetPosition` per player, so
/// the mesh is refreshed at a fraction of the frame rate and redrawn every
/// frame from cached vertices.
const REBUILD_EVERY: u32 = 6;
/// A body is at the avatar root, so the box starts there and is this tall.
const BODY_HEIGHT: f32 = 1.85;
const BODY_HALF_WIDTH: f32 = 0.38;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    On,
}

pub struct Renderer {
    mode: Mode,
    api: Option<Api>,
    /// One line per state change, never per frame.
    reported: Option<String>,
    frames: u32,
    /// Geometry actually submitted by the last rebuild.
    segments: usize,
    players: usize,
    /// Consecutive faults; a run of them switches the renderer off instead of
    /// faulting the game once per frame.
    faults: u32,
    /// Consecutive vertex-upload faults (the mesh never received geometry).
    upload_faults: u32,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            api: None,
            reported: None,
            frames: 0,
            segments: 0,
            players: 0,
            faults: 0,
            upload_faults: 0,
        }
    }
}

/// Everything resolved once: managed handles, the staging buffer and the
/// identity matrix `Graphics.DrawMesh` receives by reference.
struct Api {
    mesh: usize,
    material: usize,
    verts: usize,
    verts_data: usize,
    mesh_class: usize,
    set_vertices: usize,
    /// `Mesh.vertices` setter — the legacy upload path, used only when
    /// `SetVertices(Vector3[])` throws inside Unity.
    set_vertices_legacy: usize,
    draw_mesh: usize,
    matrix: [f32; 16],
    staging: Vec<f32>,
    /// Element class and factory kept for the probe's own diagnostics.
    vector3_class: usize,
    array_new: usize,
    /// Floats currently holding geometry in the managed vertex array.
    written: usize,
    shader: String,
    /// Which classes ended up resolved, for the one-line report.
    classes: Vec<String>,
}

impl Renderer {
    pub fn set(&mut self, enabled: bool) {
        self.mode = if enabled { Mode::On } else { Mode::Off };
        if !enabled {
            // Keeping the managed objects alive is deliberate: re-enabling is a
            // flag flip, and a stale mesh simply stops being drawn.
            self.reported = Some("draw: off (managed objects retained)".into());
        }
    }

    pub fn enabled(&self) -> bool {
        self.mode == Mode::On
    }

    /// `draw status` — one line per fact, no game-memory work.
    pub fn status(&self) -> Vec<String> {
        let mut out = vec![format!(
            "draw: {} mode segments={} players={} frames={}",
            if self.enabled() { "on" } else { "off" },
            self.segments,
            self.players,
            self.frames
        )];
        match &self.api {
            Some(api) => out.push(format!(
                "draw: mesh {:#x} material {:#x} shader {:?} verts {:#x} classes [{}]",
                api.mesh,
                api.material,
                api.shader,
                api.verts,
                api.classes.join(", ")
            )),
            None => out.push("draw: managed objects not created yet".into()),
        }
        if let Some(note) = &self.reported {
            out.push(format!("draw: {note}"));
        }
        out
    }

    /// `draw probe` — report the managed objects and re-run one call, so a
    /// fault inside Unity can be attributed instead of guessed.
    pub fn probe(&self, metadata: &mut Metadata<'_>) -> Vec<String> {
        let mut out = Vec::new();
        let Some(api) = self.api.as_ref() else {
            out.push("draw probe: managed objects not created yet (run `draw on`)".into());
            return out;
        };
        let mesh_class = metadata.host.read_usize(api.mesh).unwrap_or(0);
        let verts_class = metadata.host.read_usize(api.verts).unwrap_or(0);
        let verts_len = metadata.host.read_usize(api.verts + 0x18).unwrap_or(0);
        out.push(format!(
            "draw probe: mesh {:#x} klass {:#x}, material {:#x} klass {:#x}",
            api.mesh,
            mesh_class,
            api.material,
            metadata.host.read_usize(api.material).unwrap_or(0)
        ));
        out.push(format!(
            "draw probe: verts {:#x} klass {:#x} length {}",
            api.verts, verts_class, verts_len
        ));
        out.push(format!(
            "draw probe: SetVertices {:#x}, DrawMesh {:#x}, shader {:?}",
            api.set_vertices, api.draw_mesh, api.shader
        ));
        // Is the mesh object itself healthy? A zero vertex count on a freshly
        // constructed Mesh means the ctor did not run, not that the array is bad.
        let mesh_methods = metadata.methods(api.mesh_class);
        if let Some(get_count) = mesh_methods
            .iter()
            .find(|m| m.name == "get_vertexCount" && m.parameters_count == 0)
            .map(|m| m.entry)
            .filter(|entry| *entry != 0)
        {
            match metadata.host.call(get_count, &[api.mesh]) {
                Ok(value) => out.push(format!("draw probe: mesh.vertexCount = {value}")),
                Err(status) => out.push(format!("draw probe: get_vertexCount faulted, status {status}")),
            }
        }
        // Same call with a one-element array: distinguishes "the array is wrong"
        // from "the mesh is wrong".
        if let Ok(small) = metadata.host.call(api.array_new, &[api.vector3_class, 1])
            && small >= 0x10000
        {
            match metadata.host.call(api.set_vertices, &[api.mesh, small]) {
                Ok(value) => out.push(format!("draw probe: SetVertices(len 1) returned {value:#x}")),
                Err(status) => out.push(format!(
                    "draw probe: SetVertices(len 1) faulted, host status {status}"
                )),
            }
        }
        match metadata
            .host
            .call(api.set_vertices, &[api.mesh, api.verts])
        {
            Ok(value) => out.push(format!("draw probe: SetVertices returned {value:#x}")),
            Err(status) => out.push(format!("draw probe: SetVertices faulted, host status {status}")),
        }
        match metadata
            .host
            .call(api.draw_mesh, &[api.mesh, api.matrix.as_ptr() as usize, api.material, 0, 0, 0, 0, 0, 0, 0])
        {
            Ok(value) => out.push(format!("draw probe: DrawMesh returned {value:#x}")),
            Err(status) => out.push(format!("draw probe: DrawMesh faulted, host status {status}")),
        }
        out
    }

    /// Per-frame work on the established game thread. Draws every frame and
    /// rebuilds the vertex buffer occasionally.
    pub fn frame(&mut self, metadata: &mut Metadata<'_>, scene_cache: &mut telemetry::Cache) {
        if self.mode == Mode::Off {
            return;
        }
        if self.api.is_none() {
            match build_api(metadata) {
                Ok(api) => {
                    let line = format!(
                        "draw: in-game renderer ready — shader {:?}, mesh {:#x}, \
                         {} vertices, classes [{}]",
                        api.shader,
                        api.mesh,
                        MAX_VERTS,
                        api.classes.join(", ")
                    );
                    metadata.host.log(&line);
                    self.reported = Some(line);
                    self.api = Some(api);
                }
                Err(error) => {
                    // One attempt per `draw on`; a failure is reported once and
                    // the mode drops back so the game is never faulted per frame.
                    let line = format!("draw: renderer unavailable — {error}");
                    metadata.host.log(&line);
                    self.reported = Some(line);
                    self.mode = Mode::Off;
                    return;
                }
            }
        }
        let Some(api) = self.api.as_mut() else {
            return;
        };
        self.frames = self.frames.saturating_add(1);

        if self.frames % REBUILD_EVERY == 1 {
            let (segments, players, upload_fault) = rebuild(metadata, scene_cache, api);
            self.players = players;
            match upload_fault {
                None => {
                    self.segments = segments;
                    self.upload_faults = 0;
                }
                Some(status) => {
                    // Retry twice (the other upload path may work), then stop:
                    // a fault per rebuild is an exception per rebuild inside Unity.
                    self.segments = 0;
                    self.upload_faults = self.upload_faults.saturating_add(1);
                    if self.upload_faults == 1 {
                        metadata.host.log(&format!(
                            "draw: Mesh.SetVertices faulted (host status {status}); trying the legacy \
                             vertices setter"
                        ));
                    }
                    if self.upload_faults >= 3 {
                        metadata
                            .host
                            .log("draw: vertex upload keeps faulting — renderer switched off");
                        self.mode = Mode::Off;
                    }
                }
            }
        }

        // SAFETY: the matrix is only read for the duration of the call, and it
        // is a local `[f32;16]`, i.e. a valid address in this process.
        let matrix = api.matrix.as_ptr() as usize;
        // Ten arguments: mesh, matrix, material, layer, camera, submesh index,
        // property block, cast shadows, receive shadows, light probes. The
        // camera and the property block are null (every camera, no overrides),
        // shadows are off and light probes are not used.
        let arguments = [
            api.mesh,
            matrix,
            api.material,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        match metadata.host.call(api.draw_mesh, &arguments) {
            Ok(_) => self.faults = 0,
            Err(status) => {
                self.faults = self.faults.saturating_add(1);
                if self.faults == 1 {
                    metadata
                        .host
                        .log(&format!("draw: Graphics.DrawMesh faulted (host status {status})"));
                }
                if self.faults >= 16 {
                    metadata
                        .host
                        .log("draw: 16 consecutive draw faults — renderer switched off");
                    self.mode = Mode::Off;
                }
            }
        }
    }
}

/// Replace the vertex buffer with the current scene.
fn rebuild(
    metadata: &mut Metadata<'_>,
    scene_cache: &mut telemetry::Cache,
    api: &mut Api,
) -> (usize, usize, Option<i32>) {
    api.staging.clear();
    let scene = telemetry::scene(metadata, scene_cache);
    let eye = scene.as_ref().and_then(|scene| scene.eye);
    let mut players = 0usize;

    if let Some(scene) = &scene {
        for (_, position) in &scene.players {
            let eye = eye.unwrap_or(*position);
            let distance = length(sub(*position, eye));
            let half = (distance * HALF_WIDTH_PER_METRE)
                .clamp(HALF_WIDTH_MIN, HALF_WIDTH_MAX);
            let min = (
                position.0 - BODY_HALF_WIDTH,
                position.1,
                position.2 - BODY_HALF_WIDTH,
            );
            let max = (
                position.0 + BODY_HALF_WIDTH,
                position.1 + BODY_HEIGHT,
                position.2 + BODY_HALF_WIDTH,
            );
            if push_box(&mut api.staging, min, max, eye, half) {
                players += 1;
            }
        }
        // Crosshair: a fixed-size cross in front of the camera proves the
        // pipeline even in an empty world.
        if let (Some(eye), Some(forward)) = (scene.eye, scene.forward) {
            let centre = add(eye, scale(forward, 1.5));
            push_cross(&mut api.staging, centre, forward, 0.012, eye);
        }
    }

    // Nothing to draw (no SDK yet, empty scene): clear previously drawn geometry
    // once instead of writing an empty buffer every rebuild.
    if api.staging.is_empty() {
        if api.written > 0 {
            let zeros = vec![0u8; api.written * 4];
            metadata.host.write_bytes(api.verts_data, &zeros);
            api.written = 0;
            let _ = metadata.host.call(api.set_vertices, &[api.mesh, api.verts]);
        }
        return (0, players, None);
    }

    // Unused vertices collapse onto the origin, which rasterises to nothing.
    // Only the prefix that carries geometry is written: the mesh keeps its
    // vertices between frames, and the tail was last cleared below.
    let filled = api.staging.len().min(MAX_VERTS * 3);
    api.staging.resize(MAX_VERTS * 3, 0.0);
    let bytes = f32_bytes(&api.staging[..filled]);
    if !metadata.host.write_bytes(api.verts_data, bytes) {
        metadata.host.log("draw: could not write the vertex array");
        return (0, 0, None);
    }
    // When the box count drops, the vertices written last time are still in the
    // mesh; clear exactly that tail.
    if filled < api.written {
        let tail = f32_bytes(&api.staging[filled..api.written]);
        metadata.host.write_bytes(api.verts_data + filled * 4, tail);
    }
    api.written = filled;
    // `SetVertices(Vector3[])` has thrown inside Unity on this build; the legacy
    // `Mesh.vertices` setter is a different code path and is tried once before
    // the frame is declared broken.
    let mut uploaded = false;
    let mut last_status = 0;
    for entry in [api.set_vertices, api.set_vertices_legacy] {
        if entry == 0 {
            continue;
        }
        match metadata.host.call(entry, &[api.mesh, api.verts]) {
            Ok(_) => {
                uploaded = true;
                break;
            }
            Err(status) => last_status = status,
        }
    }
    if !uploaded {
        return (0, 0, Some(last_status));
    }
    (filled / 12, players, None)
}

/// Reinterpret an `f32` slice as bytes without copying: the staging buffer is
/// the same memory the write service reads for the duration of the call.
fn f32_bytes(values: &[f32]) -> &[u8] {
    // SAFETY: `f32` has no padding and no invalid bit patterns, so re-borrowing
    // the same allocation with a byte view is total and has the same lifetime.
    unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), values.len() * 4) }
}

// ---- managed API resolution ------------------------------------------------

fn build_api(metadata: &mut Metadata<'_>) -> Result<Api, String> {
    let host = metadata.host;
    let api = metadata.api_anchors();
    let object_new = rva(host, api.object_new)?;
    let array_new = rva(host, api.array_new)?;
    let string_new = rva(host, api.string_new)?;
    let class_init = rva(host, api.class_init)?;

    let mut classes = Vec::new();
    let mesh_class = resolve_class(metadata, "UnityEngine", "Mesh", "UnityEngine.CoreModule")?;
    let material_class =
        resolve_class(metadata, "UnityEngine", "Material", "UnityEngine.CoreModule")?;
    let shader_class = resolve_class(metadata, "UnityEngine", "Shader", "UnityEngine.CoreModule")?;
    let graphics_class =
        resolve_class(metadata, "UnityEngine", "Graphics", "UnityEngine.CoreModule")?;
    let vector3_class =
        resolve_class(metadata, "UnityEngine", "Vector3", "UnityEngine.CoreModule")?;
    let int32_class = resolve_class(metadata, "System", "Int32", "mscorlib")?;
    classes.push(format!("Mesh {mesh_class:#x}"));
    classes.push(format!("Material {material_class:#x}"));
    classes.push(format!("Shader {shader_class:#x}"));
    classes.push(format!("Graphics {graphics_class:#x}"));
    classes.push(format!("Vector3 {vector3_class:#x}"));
    classes.push(format!("Int32 {int32_class:#x}"));

    // Overload selection is by signature, never by name and arity alone:
    // `Mesh.SetVertices`/`SetTriangles` take either `T[]` or `List<T>` with the
    // same parameter count, and passing an array where a `List` is expected
    // makes the callee read fields that do not exist in that object.
    let shader_methods = metadata.methods(shader_class);
    let find = shader_methods
        .iter()
        .find(|m| {
            m.name == "Find"
                && m.parameters_count == 1
                && metadata.first_parameter_kind(m.info) == Some(TYPE_STRING)
        })
        .map(|m| m.entry)
        .filter(|entry| *entry != 0)
        .ok_or_else(|| "UnityEngine.Shader.Find(string) not found".to_string())?;

    let material_methods = metadata.methods(material_class);
    let material_ctor = material_methods
        .iter()
        .find(|m| {
            m.name == ".ctor"
                && m.parameters_count == 1
                && metadata.parameter_class(m.info, 0) == Some(shader_class)
        })
        .map(|m| m.entry)
        .filter(|entry| *entry != 0)
        .ok_or_else(|| "UnityEngine.Material..ctor(Shader) not found".to_string())?;

    let mesh_methods = metadata.methods(mesh_class);
    let mesh_ctor = mesh_methods
        .iter()
        .find(|m| m.name == ".ctor" && m.parameters_count == 0)
        .map(|m| m.entry)
        .filter(|entry| *entry != 0)
        .ok_or_else(|| "UnityEngine.Mesh..ctor() not found".to_string())?;
    let set_vertices = mesh_methods
        .iter()
        .find(|m| {
            m.name == "SetVertices"
                && m.parameters_count == 1
                && metadata.parameter_is_array(m.info, 0)
        })
        .map(|m| m.entry)
        .filter(|entry| *entry != 0)
        .ok_or_else(|| "UnityEngine.Mesh.SetVertices(Vector3[]) not found".to_string())?;
    let set_vertices_legacy = mesh_methods
        .iter()
        .find(|m| {
            m.name == "set_vertices"
                && m.parameters_count == 1
                && metadata.parameter_is_array(m.info, 0)
        })
        .map(|m| m.entry)
        .filter(|entry| *entry != 0)
        .unwrap_or(0);
    // `SetTriangles` follows the same collapsing rule as `DrawMesh`: the
    // two-parameter C# shape may exist as `(int[], int)` or only as
    // `(int[], int, bool calculateBounds)`. Both are called through the same
    // entry, with the third argument set to "recalculate bounds".
    let mut set_triangles = None;
    let mut triangle_shapes = Vec::new();
    for candidate in mesh_methods
        .iter()
        .filter(|m| m.name == "SetTriangles" && (2..=3).contains(&m.parameters_count))
        .filter(|m| metadata.parameter_is_array(m.info, 0))
    {
        let element = metadata.parameter_array_element_class(candidate.info, 0);
        triangle_shapes.push(format!(
            "params={} element={element:?}",
            candidate.parameters_count
        ));
        let better = match set_triangles {
            None => true,
            Some((_, count, element)) => {
                (element == Some(int32_class) && candidate.parameters_count <= count)
                    || (element != Some(int32_class) && candidate.parameters_count < count)
            }
        };
        if better {
            set_triangles = Some((candidate.entry, candidate.parameters_count, element));
        }
    }
    let Some((set_triangles, set_triangles_arity, _)) = set_triangles else {
        return Err(format!(
            "UnityEngine.Mesh.SetTriangles(int[], int) not found (array overloads: {triangle_shapes:?})"
        ));
    };
    let set_bounds = mesh_methods
        .iter()
        .find(|m| m.name == "set_bounds" && m.parameters_count == 1)
        .map(|m| m.entry)
        .filter(|entry| *entry != 0);
    let set_color = material_methods
        .iter()
        .find(|m| m.name == "set_color" && m.parameters_count == 1)
        .map(|m| m.entry)
        .filter(|entry| *entry != 0);
    // `SetInt` has two overloads (`string,int` and `int,int`); the string one is
    // told apart by its first parameter being a managed string.
    let set_int = method_named_with_string_first(metadata, material_class, "SetInt")
        .or_else(|| method_named_with_string_first(metadata, material_class, "SetInteger"));
    let draw_mesh = draw_mesh_entry(metadata, graphics_class)?;

    // Lazy type initializers must run before instances/arrays exist.
    for class in [
        mesh_class,
        material_class,
        shader_class,
        graphics_class,
        vector3_class,
        int32_class,
    ] {
        let _ = host.call(class_init, &[class]);
    }

    let mut shader_name = String::new();
    let mut shader = 0usize;
    for candidate in SHADERS {
        let managed = match managed_string(host, string_new, candidate) {
            Ok(managed) => managed,
            Err(error) => {
                host.log(&format!("draw: {error}"));
                continue;
            }
        };
        match host.call(find, &[managed]) {
            Ok(found) if found >= 0x10000 => {
                shader = found;
                shader_name = candidate.to_string();
                break;
            }
            Ok(_) => continue,
            Err(status) => {
                host.log(&format!("draw: Shader.Find({candidate:?}) faulted: {status}"));
                continue;
            }
        }
    }
    if shader == 0 {
        return Err("none of the fallback shaders is present in this build".into());
    }

    let material = host
        .call(object_new, &[material_class])
        .map_err(|status| format!("il2cpp_object_new(Material) faulted: {status}"))?;
    if material < 0x10000 {
        return Err("il2cpp_object_new(Material) returned null".into());
    }
    host.call(material_ctor, &[material, shader])
        .map_err(|status| format!("Material..ctor faulted: {status}"))?;

    // A visible colour and no depth test: an ESP that walls occlude is useless.
    if let Some(set_color) = set_color {
        // Color is four floats passed by reference.
        let colour: [f32; 4] = [0.25, 0.95, 1.0, 1.0];
        let _ = host.call(set_color, &[material, colour.as_ptr() as usize]);
    }
    if let (Some(set_int), Ok(property)) = (set_int, managed_string(host, string_new, "_ZTest")) {
        // CompareFunction.Always = 8, so the boxes stay visible through geometry.
        let _ = host.call(set_int, &[material, property, 8]);
        for (name, value) in [("_ZWrite", 0usize), ("_Cull", 0)] {
            if let Ok(property) = managed_string(host, string_new, name) {
                let _ = host.call(set_int, &[material, property, value]);
            }
        }
    }

    let mesh = host
        .call(object_new, &[mesh_class])
        .map_err(|status| format!("il2cpp_object_new(Mesh) faulted: {status}"))?;
    if mesh < 0x10000 {
        return Err("il2cpp_object_new(Mesh) returned null".into());
    }
    host.call(mesh_ctor, &[mesh])
        .map_err(|status| format!("Mesh..ctor faulted: {status}"))?;

    // `HideFlags.HideAndDontSave` (61): the objects are ours for the session and
    // must not take part in scene saving or the object browser.
    let set_hide_flags = metadata
        .method_in_chain(mesh_class, "set_hideFlags")
        .map(|(_, entry)| entry)
        .filter(|entry| *entry != 0);
    if let Some(set_hide_flags) = set_hide_flags {
        let _ = host.call(set_hide_flags, &[mesh, 61]);
        let _ = host.call(set_hide_flags, &[material, 61]);
    }

    let verts = host
        .call(array_new, &[vector3_class, MAX_VERTS])
        .map_err(|status| format!("il2cpp_array_new(Vector3[]) faulted: {status}"))?;
    if verts < 0x10000 {
        return Err("il2cpp_array_new(Vector3[]) returned null".into());
    }

    let triangles = host
        .call(array_new, &[int32_class, MAX_INDICES])
        .map_err(|status| format!("il2cpp_array_new(int[]) faulted: {status}"))?;
    if triangles < 0x10000 {
        return Err("il2cpp_array_new(int[]) returned null".into());
    }
    let indices = triangle_indices();
    let bytes: Vec<u8> = indices.iter().flat_map(|value| value.to_le_bytes()).collect();
    if !host.write_bytes(triangles.wrapping_add(ARRAY_DATA), &bytes) {
        return Err("could not fill the index array".into());
    }
    let triangle_arguments: Vec<usize> = if set_triangles_arity >= 3 {
        vec![mesh, triangles, 0, 1]
    } else {
        vec![mesh, triangles, 0]
    };
    host.call(set_triangles, &triangle_arguments)
        .map_err(|status| format!("Mesh.SetTriangles faulted: {status}"))?;

    // One huge bounds box: the mesh is rebuilt every few frames and Unity would
    // otherwise cull it out of the camera.
    if let Some(set_bounds) = set_bounds {
        let bounds: [f32; 6] = [0.0, 0.0, 0.0, 1.0e6, 1.0e6, 1.0e6];
        let _ = host.call(set_bounds, &[mesh, bounds.as_ptr() as usize]);
    }

    // The vertices start out collapsed; the first rebuild fills them.
    let zero = vec![0u8; MAX_VERTS * 12];
    host.write_bytes(verts.wrapping_add(ARRAY_DATA), &zero);

    host.log(&format!(
        "draw: managed API resolved — shader {shader_name:?} ({shader:#x}), \
         DrawMesh {draw_mesh:#x}, SetVertices {set_vertices:#x}"
    ));

    Ok(Api {
        mesh,
        material,
        verts,
        verts_data: verts.wrapping_add(ARRAY_DATA),
        mesh_class,
        set_vertices,
        set_vertices_legacy,
        draw_mesh,
        matrix: [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
        staging: Vec::with_capacity(MAX_VERTS * 3),
        vector3_class,
        array_new,
        written: 0,
        shader: shader_name,
        classes,
    })
}

fn rva(host: &Host, value: u32) -> Result<usize, String> {
    if value == 0 {
        return Err("this build has no such il2cpp export".into());
    }
    let (base, size) = host.game_assembly_range();
    base.checked_add(value as usize)
        .filter(|address| address.checked_add(8).is_some_and(|end| end <= base + size))
        .ok_or_else(|| format!("rva {value:#x} is outside GameAssembly.dll"))
}

/// Already-materialised class first (a table scan with no calls), then
/// `il2cpp_class_from_name`, which is one call per class instead of a
/// 40 000-probe walk over the whole image.
fn resolve_class(
    metadata: &mut Metadata<'_>,
    namespace: &str,
    name: &str,
    assembly: &str,
) -> Result<usize, String> {
    class_in(metadata, namespace, name, assembly)
        .ok_or_else(|| format!("{namespace}.{name} not found in '{assembly}'"))
}

/// `class_from_name` first: one call, no table walk. The definition-table scan
/// is only a fallback, because it costs one remote read per class and would
/// stall the game thread for seconds when repeated per class.
fn class_in(
    metadata: &mut Metadata<'_>,
    namespace: &str,
    name: &str,
    assembly: &str,
) -> Option<usize> {
    if let Some(image) = metadata.image_of_assembly(assembly)
        && let Some(class) = metadata.class_by_name(image, namespace, name)
    {
        return Some(class);
    }
    metadata
        .find(namespace, name, crate::m3::CLASS_MAX)
        .first()
        .map(|(_, class)| *class)
}

/// `Il2CppTypeEnum::STRING` — the kind that tells `SetInt(string,int)` apart
/// from `SetInt(int,int)` when both have the same name and arity.
const TYPE_STRING: u32 = 0x0e;
/// `Il2CppTypeEnum::BOOLEAN`.
const TYPE_BOOLEAN: u32 = 0x02;

/// The `DrawMesh` overload to call.
///
/// Unity 6 collapsed the C# optional parameters into one wide method: the
/// measured build has `DrawMesh` with 10, 11 and 12 parameters and **no**
/// four-argument shape at all. The ten-argument one is the classic
/// `(mesh, matrix, material, layer, camera, submeshIndex, properties,
/// castShadows, receiveShadows, useLightProbes)` — its last three parameters are
/// booleans, while the wider shapes end in `ShadowCastingMode`/`LightProbeUsage`
/// enums or a `LightProbeProxyVolume`.
fn draw_mesh_entry(metadata: &Metadata<'_>, class: usize) -> Result<usize, String> {
    let mut counts = Vec::new();
    let mut ten = None;
    for candidate in metadata.methods(class) {
        if candidate.name != "DrawMesh" {
            continue;
        }
        let count = candidate.parameters_count;
        counts.push(count);
        if count != 10 {
            continue;
        }
        let boolean_tail = (7..10u8).all(|index| {
            metadata.parameter_kind(candidate.info, index) == Some(TYPE_BOOLEAN)
        });
        if boolean_tail {
            return Ok(candidate.entry);
        }
        ten.get_or_insert(candidate.entry);
    }
    ten.ok_or_else(|| {
        format!(
            "no ten-argument Graphics.DrawMesh in this build (parameter counts found: {counts:?})"
        )
    })
}

/// The overload of `name` whose first parameter is a managed `string`.
fn method_named_with_string_first(
    metadata: &Metadata<'_>,
    class: usize,
    name: &str,
) -> Option<usize> {
    metadata
        .methods(class)
        .into_iter()
        .filter(|candidate| candidate.name == name && candidate.parameters_count > 0)
        .find(|candidate| metadata.first_parameter_kind(candidate.info) == Some(TYPE_STRING))
        .map(|candidate| candidate.entry)
        .filter(|entry| *entry != 0)
}

/// Copy a UTF-8 literal into the game's heap as a managed `string`.
///
/// The literal itself lives in this image's read-only data, which is a valid
/// address in the game's process; `il2cpp_string_new` copies the characters, so
/// the pointer never outlives the call.
fn managed_string(host: &Host, string_new: usize, text: &str) -> Result<usize, String> {
    let literal = CString::new(text).map_err(|error| error.to_string())?;
    let managed = host
        .call(string_new, &[literal.as_ptr() as usize])
        .map_err(|status| format!("il2cpp_string_new({text:?}) faulted: {status}"))?;
    if managed < 0x10000 {
        return Err(format!("il2cpp_string_new({text:?}) returned null"));
    }
    Ok(managed)
}

/// Both windings for every segment: `(0,1,2) (0,2,3)` and the reverse.
fn triangle_indices() -> Vec<i32> {
    let mut out = Vec::with_capacity(MAX_INDICES);
    for segment in 0..MAX_SEGMENTS {
        let base = (segment * VERTS_PER_SEGMENT) as i32;
        out.extend_from_slice(&[
            base,
            base + 1,
            base + 2,
            base,
            base + 2,
            base + 3,
            base + 2,
            base + 1,
            base,
            base + 3,
            base + 2,
            base,
        ]);
    }
    out
}

// ---- geometry --------------------------------------------------------------

type P3 = (f32, f32, f32);

fn sub(a: P3, b: P3) -> P3 {
    (a.0 - b.0, a.1 - b.1, a.2 - b.2)
}

fn add(a: P3, b: P3) -> P3 {
    (a.0 + b.0, a.1 + b.1, a.2 + b.2)
}

fn scale(a: P3, k: f32) -> P3 {
    (a.0 * k, a.1 * k, a.2 * k)
}

fn length(a: P3) -> f32 {
    (a.0 * a.0 + a.1 * a.1 + a.2 * a.2).sqrt()
}

fn cross(a: P3, b: P3) -> P3 {
    (
        a.1 * b.2 - a.2 * b.1,
        a.2 * b.0 - a.0 * b.2,
        a.0 * b.1 - a.1 * b.0,
    )
}

fn vertex(out: &mut Vec<f32>, point: P3) {
    out.extend_from_slice(&[point.0, point.1, point.2]);
}

/// One camera-facing quad from `a` to `b`.
fn push_segment(out: &mut Vec<f32>, a: P3, b: P3, eye: P3, half: f32) -> bool {
    if out.len() + VERTS_PER_SEGMENT * 3 > MAX_VERTS * 3 {
        return false;
    }
    let direction = sub(b, a);
    let middle = scale(add(a, b), 0.5);
    let to_eye = sub(eye, middle);
    let side_raw = cross(direction, to_eye);
    let side_len = length(side_raw);
    // Degenerate (edge pointing straight at the camera): pick any perpendicular
    // so the quad still has area instead of vanishing.
    let side = if side_len < 1e-6 {
        (half, 0.0, 0.0)
    } else {
        scale(side_raw, half / side_len)
    };
    vertex(out, sub(a, side));
    vertex(out, add(a, side));
    vertex(out, add(b, side));
    vertex(out, sub(b, side));
    true
}

/// The twelve edges of an axis-aligned box.
fn push_box(out: &mut Vec<f32>, min: P3, max: P3, eye: P3, half: f32) -> bool {
    let corners = [
        (min.0, min.1, min.2),
        (max.0, min.1, min.2),
        (max.0, min.1, max.2),
        (min.0, min.1, max.2),
        (min.0, max.1, min.2),
        (max.0, max.1, min.2),
        (max.0, max.1, max.2),
        (min.0, max.1, max.2),
    ];
    const EDGES: [(usize, usize); 12] = [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ];
    for (start, end) in EDGES {
        if !push_segment(out, corners[start], corners[end], eye, half) {
            return false;
        }
    }
    true
}

/// A small cross facing the camera, so the pipeline is visible with no players.
fn push_cross(out: &mut Vec<f32>, centre: P3, forward: P3, size: f32, eye: P3) {
    let right = {
        let candidate = cross(forward, (0.0, 1.0, 0.0));
        let len = length(candidate);
        if len < 1e-6 {
            (1.0, 0.0, 0.0)
        } else {
            scale(candidate, 1.0 / len)
        }
    };
    let up = cross(right, forward);
    let half = 0.0015;
    push_segment(
        out,
        sub(centre, scale(right, size)),
        add(centre, scale(right, size)),
        eye,
        half,
    );
    push_segment(
        out,
        sub(centre, scale(up, size)),
        add(centre, scale(up, size)),
        eye,
        half,
    );
}

/// Kept next to the geometry so the geometry tests can reach it.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_cover_both_windings_for_every_segment() {
        let indices = triangle_indices();
        assert_eq!(indices.len(), MAX_INDICES);
        for segment in 0..MAX_SEGMENTS {
            let base = (segment * VERTS_PER_SEGMENT) as i32;
            let slice = &indices[segment * INDICES_PER_SEGMENT..(segment + 1) * INDICES_PER_SEGMENT];
            assert!(slice.iter().all(|index| *index >= base && *index < base + 4));
            // A CW triangle and its CCW twin must both be present.
            assert_eq!(slice[0], base);
            assert_eq!(slice[9], base + 3);
        }
    }

    #[test]
    fn a_box_becomes_twelve_camera_facing_quads() {
        let mut out = Vec::new();
        let eye = (0.0, 1.0, 0.0);
        assert!(push_box(&mut out, (-0.4, 0.0, -0.4), (0.4, 1.8, 0.4), eye, 0.01));
        assert_eq!(out.len(), 12 * VERTS_PER_SEGMENT * 3);
        assert!(out.iter().all(|value| value.is_finite()));
        // Every quad keeps its width: opposite corners of a quad differ by 2*half
        // along the side vector, i.e. 0.02 apart.
        for quad in out.chunks(VERTS_PER_SEGMENT * 3) {
            let first = (quad[0], quad[1], quad[2]);
            let second = (quad[3], quad[4], quad[5]);
            let distance = length(sub(first, second));
            assert!((distance - 0.02).abs() < 1e-3, "width {distance}");
        }
    }

    #[test]
    fn a_camera_inside_the_box_still_produces_visible_quads() {
        let mut out = Vec::new();
        let eye = (0.0, 0.9, 0.0);
        assert!(push_box(&mut out, (-0.4, 0.0, -0.4), (0.4, 1.8, 0.4), eye, 0.01));
        assert_eq!(out.len(), 12 * VERTS_PER_SEGMENT * 3);
        // No quad may collapse: each one must have non-zero area.
        for quad in out.chunks(VERTS_PER_SEGMENT * 3) {
            let a = (quad[0], quad[1], quad[2]);
            let b = (quad[3], quad[4], quad[5]);
            let c = (quad[6], quad[7], quad[8]);
            let area = length(cross(sub(b, a), sub(c, a)));
            assert!(area > 1e-9, "degenerate quad");
        }
    }

    #[test]
    fn the_capacity_bound_is_respected() {
        let mut out = Vec::new();
        let eye = (0.0, 1.0, -10.0);
        let mut accepted = 0;
        for _ in 0..(MAX_SEGMENTS + 16) {
            if push_segment(&mut out, (0.0, 0.0, 0.0), (0.0, 1.0, 0.0), eye, 0.01) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_SEGMENTS);
        assert!(out.len() <= MAX_VERTS * 3);
    }

    /// The renderer must be inert until switched on: a disabled overlay makes no
    /// managed calls at all, which is what keeps it invisible when not in use.
    #[test]
    fn status_reports_the_mode_without_touching_the_game() {
        let mut renderer = Renderer::default();
        assert!(!renderer.enabled());
        let off = renderer.status().join("\n");
        assert!(off.contains("draw: off mode"));
        assert!(off.contains("managed objects not created yet"), "{off}");

        renderer.set(true);
        assert!(renderer.enabled());
        let on = renderer.status().join("\n");
        assert!(on.contains("draw: on mode"), "{on}");

        renderer.set(false);
        assert!(!renderer.enabled());
        let after = renderer.status().join("\n");
        assert!(after.contains("retained"), "{after}");
    }
}
