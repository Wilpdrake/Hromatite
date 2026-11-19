//! File-based runtime console: safe memory diagnostics, verified M3 accessors,
//! GC/LateUpdate diagnostics and the telemetry contract consumed by the overlay.

#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use hromatite_il2cpp::procs;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const CMD_FILE: &str = "hromatite-cmd.txt";

pub fn cmd_path() -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".to_string());
    std::path::PathBuf::from(base).join(CMD_FILE)
}

pub struct Reader {
    path: std::path::PathBuf,
    pos: u64,
}

impl Reader {
    /// Start at the current end of file so that commands sent before injection
    /// are not replayed.
    pub fn new() -> Self {
        let path = cmd_path();
        let pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { path, pos }
    }

    /// Complete lines appended since the last call (a partial tail is kept for
    /// the next poll).
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut f = match std::fs::OpenOptions::new().read(true).open(&self.path) {
            Ok(f) => f,
            Err(_) => return out,
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            // host truncated/recreated the file
            self.pos = 0;
        }
        if len == self.pos {
            return out;
        }
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return out;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return out;
        }
        self.pos = len;
        for line in buf.lines() {
            let l = line.trim();
            if !l.is_empty() {
                out.push(l.to_string());
            }
        }
        out
    }
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(PartialEq)]
pub enum Action {
    None,
    Quit,
    Seal,
}

fn parse_addr(s: &str) -> Option<usize> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    usize::from_str_radix(t, 16).ok()
}

// Every read below goes through `safe` — the console's job is exploring memory
// whose shape is unknown, so an unguarded deref here is an AV in the stub
// thread, i.e. a dead game instead of a bad answer line.

fn qword(addr: usize) -> Option<u64> {
    hromatite_il2cpp::safe::read_u64(addr)
}

fn bytes(addr: usize, len: usize) -> Vec<u8> {
    // partial read on purpose: `b <addr> n` should show what is there
    hromatite_il2cpp::safe::read_upto(addr, len)
}

fn ascii(addr: usize, max: usize) -> String {
    hromatite_il2cpp::safe::ascii_lossy(addr, max)
}

fn utf16(addr: usize, max: usize) -> String {
    hromatite_il2cpp::safe::utf16_lossy(addr, max)
}

fn tag_of(v: usize) -> &'static str {
    let ga = procs::own_module(hromatite_il2cpp::GAME_ASSEMBLY);
    let up = procs::own_module(hromatite_il2cpp::UNITY_PLAYER);
    if let Some((b, s)) = ga
        && v >= b
        && v < b + s
    {
        return "GA";
    }
    if let Some((b, s)) = up
        && v >= b
        && v < b + s
    {
        return "UP";
    }
    ""
}

fn dump_qwords(addr: usize, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..n {
        let a = addr + i * 8;
        match qword(a) {
            Some(v) => {
                let asc = ascii(v as usize, 24);
                let asc = if asc.is_empty() {
                    String::new()
                } else {
                    format!(" '{asc}'")
                };
                out.push(format!("  {a:#x}: {v:#016x}{asc} {}", tag_of(v as usize)));
            }
            None => {
                out.push(format!("  {a:#x}: (unmapped)"));
                break;
            }
        }
    }
    out
}

/// Vector3 out of three little-endian floats.
fn vec3(bits: [u64; 2]) -> (f32, f32, f32) {
    (
        f32::from_bits(bits[0] as u32),
        f32::from_bits((bits[0] >> 32) as u32),
        f32::from_bits(bits[1] as u32),
    )
}

/// M3 end-to-end proof: `Camera.main` → `transform` → `position`.
///
/// Nothing here needs an identified il2cpp export: the classes come from the
/// image's own type table, and a method is called through the entry point that
/// sits at `MethodInfo+0x00`. The Vector3 comes back through the hidden return
/// pointer that Win64 uses for structs larger than 8 bytes (`rcx` = buffer,
/// `rdx` = this).
fn pos_probe() -> Vec<String> {
    let mut out = Vec::new();
    // Everything below uses the verified Unity 6 layout (crate::m3):
    // name@+0x98, methods@+0x50, parent@+0x10.
    let cam = match hromatite_il2cpp::m3::find("UnityEngine", "Camera", 120_000).first() {
        Some((i, c)) => {
            out.push(format!(
                "pos: Camera class {c:#x} ({}), #{i}",
                hromatite_il2cpp::m3::class_full_name(*c)
            ));
            *c
        }
        None => return vec!["pos: Camera class not found (class table)".into()],
    };

    // 1. static Camera.main
    let main = match hromatite_il2cpp::m3::methods(cam)
        .into_iter()
        .find(|m| m.name == "get_main")
    {
        Some(m) => m.entry,
        None => {
            out.push("pos: no Camera.get_main".into());
            return out;
        }
    };
    // SAFETY: get_main is Camera's static accessor (no arguments) taken from
    // the live MethodInfo table; VEH contains a bad target.
    let cam_inst = match unsafe { hromatite_il2cpp::veh::call0(main) } {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("pos: Camera.main faulted: {e}"));
            return out;
        }
    };
    out.push(format!("pos: Camera.main = {cam_inst:#x}"));
    if cam_inst < 0x10000 {
        out.push("pos: no main camera in this world".into());
        return out;
    }

    // 2. `Component.transform` — found through the parent chain
    //    (Camera → Behaviour → Component), never in Camera's own method list.
    let (owner, get_transform) = match hromatite_il2cpp::m3::method_in_chain(cam, "get_transform") {
        Some(v) => v,
        None => {
            out.push("pos: no get_transform in the class chain".into());
            return out;
        }
    };
    out.push(format!(
        "pos: get_transform from {}",
        hromatite_il2cpp::m3::class_full_name(owner)
    ));
    // SAFETY: instance getter, exactly one `this` pointer, from a live MethodInfo.
    let tr = match unsafe { hromatite_il2cpp::veh::call1(get_transform, cam_inst) } {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("pos: get_transform faulted: {e}"));
            return out;
        }
    };
    out.push(format!(
        "pos: transform = {tr:#x} ({})",
        hromatite_il2cpp::m3::class_full_name(hromatite_il2cpp::safe::read_usize(tr).unwrap_or(0))
    ));
    if tr < 0x10000 {
        return out;
    }

    // 3. `Transform.position` and `localPosition` (struct return: hidden buffer
    //    in rcx, `this` in rdx — measured, not assumed).
    let trc = match hromatite_il2cpp::m3::find("UnityEngine", "Transform", 120_000).first() {
        Some((_, c)) => *c,
        None => {
            out.push("pos: Transform class not found".into());
            return out;
        }
    };
    for name in ["get_position", "get_localPosition"] {
        let f = match hromatite_il2cpp::m3::methods(trc).into_iter().find(|m| m.name == name) {
            Some(m) => m.entry,
            None => {
                out.push(format!("pos: no Transform.{name}"));
                continue;
            }
        };
        let mut buf = [0u64; 2];
        // SAFETY: get_*Position returns a 12-byte Vector3 through a hidden
        // output pointer (first argument) and takes `this` second; `buf` is a
        // live two-qword writable buffer on our own stack.
        match unsafe { hromatite_il2cpp::veh::call2(f, buf.as_mut_ptr() as usize, tr) } {
            Ok(_) => {
                let (x, y, z) = vec3(buf);
                out.push(format!(
                    "pos: {name} = ({x:.3}, {y:.3}, {z:.3})  [raw {:#x} {:#x}]",
                    buf[0], buf[1]
                ));
            }
            Err(e) => out.push(format!("pos: {name} faulted: {e}")),
        }
    }
    out
}

/// `Il2CppString*` → Rust string (length at +0x10, UTF-16 chars at +0x14).
fn il2cpp_string(p: usize) -> Option<String> {
    if p < 0x10000 {
        return None;
    }
    let len = hromatite_il2cpp::safe::read_u32(p + 0x10)? as usize;
    if len > 4096 {
        return None;
    }
    let bytes = hromatite_il2cpp::safe::read_bytes(p + 0x14, len.checked_mul(2)?)?;
    let (units, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = units.iter().map(|c| u16::from_le_bytes(*c)).collect();
    Some(String::from_utf16_lossy(&units))
}

/// `who` — every player in the instance, through the public SDK API
/// (`VRC.SDKBase.VRCPlayerApi`): the list comes from `get_AllPlayers`
/// (`List<VRCPlayerApi>`), id from `GetPlayerId(player)`, position from
/// `GetPosition(player)` (Vector3 through a hidden output pointer).
///
/// This deliberately avoids VRChat's own obfuscated classes: the SDK surface is
/// stable across builds and is what world scripts themselves use.
/// Class by namespace/name; if the class table has not materialised it yet,
/// walk the assembly's `image_get_class(image, i)` (which builds it on demand).
fn class_or_materialise(ns: &str, name: &str, assembly: &str) -> Option<usize> {
    if let Some((_, c)) = hromatite_il2cpp::m3::find(ns, name, 120_000).first() {
        return Some(*c);
    }
    let f = hromatite_il2cpp::m3::image_get_class_addr()?;
    let first = hromatite_il2cpp::il2cpp::find_domain()?;
    let image = hromatite_il2cpp::m3::image_of(first, assembly)?;
    for i in 0..40_000usize {
        // SAFETY: image_get_class(image, i) with the measured ABI; VEH guards it.
        let cls = unsafe { hromatite_il2cpp::veh::call2(f, image, i) }.ok()?;
        if cls < 0x10000 || !hromatite_il2cpp::safe::readable_at(cls) {
            continue;
        }
        if hromatite_il2cpp::m3::class_name(cls).as_deref() == Some(name)
            && hromatite_il2cpp::m3::class_namespace(cls).as_deref() == Some(ns)
        {
            return Some(cls);
        }
    }
    None
}

/// Method entries and classes that `who`/`esp` need, resolved once per process.
///
/// Resolving them walks the whole type table (tens of milliseconds) and `esp`
/// runs four to five times per second, so the records are `Box::leak`ed instead
/// of cached under a `Mutex`: a panic inside the guard longjmps past
/// destructors, and a leaked `MutexGuard` would hang every later command.
struct Sdk {
    api_class: usize,
    get_all: usize,
    get_id: usize,
    get_pos: usize,
    get_objects: usize,
    obj_name: usize,
}

struct CamApi {
    get_main: usize,
    get_transform: usize,
    tr_position: usize,
    tr_forward: usize,
    get_proj: usize,
    get_view: usize,
    get_pixw: usize,
    get_pixh: usize,
}

static SDK: AtomicUsize = AtomicUsize::new(0);
static CAM: AtomicUsize = AtomicUsize::new(0);

fn method_entry(ms: &[hromatite_il2cpp::m3::Method], name: &str) -> usize {
    ms.iter().find(|m| m.name == name).map_or(0, |m| m.entry)
}

fn sdk() -> Option<&'static Sdk> {
    let cached = SDK.load(Ordering::Relaxed);
    if cached != 0 {
        // SAFETY: `cached` is a `Box::into_raw` pointer that is never freed.
        return Some(unsafe { &*(cached as *const Sdk) });
    }
    let api_class = class_or_materialise("VRC.SDKBase", "VRCPlayerApi", "VRCSDKBase")?;
    let ms = hromatite_il2cpp::m3::methods(api_class);
    let get_all = method_entry(&ms, "get_AllPlayers");
    let get_id = method_entry(&ms, "GetPlayerId");
    if get_all == 0 || get_id == 0 {
        return None;
    }
    let obj_name = hromatite_il2cpp::m3::find("UnityEngine", "Object", 120_000)
        .first()
        .and_then(|(_, c)| {
            hromatite_il2cpp::m3::methods(*c)
                .into_iter()
                .find(|m| m.name == "get_name")
                .map(|m| m.entry)
        })
        .unwrap_or(0);
    let rec = Sdk {
        api_class,
        get_all,
        get_id,
        get_pos: method_entry(&ms, "GetPosition"),
        get_objects: method_entry(&ms, "GetPlayerObjects"),
        obj_name,
    };
    let ptr = Box::into_raw(Box::new(rec));
    SDK.store(ptr as usize, Ordering::Relaxed);
    // SAFETY: freshly boxed and leaked on purpose; valid for the process.
    Some(unsafe { &*ptr })
}

fn cam_api() -> Option<&'static CamApi> {
    let cached = CAM.load(Ordering::Relaxed);
    if cached != 0 {
        // SAFETY: `cached` is a `Box::into_raw` pointer that is never freed.
        return Some(unsafe { &*(cached as *const CamApi) });
    }
    let cam_class = hromatite_il2cpp::m3::find("UnityEngine", "Camera", 120_000).first()?.1;
    let (trc, get_transform) = hromatite_il2cpp::m3::method_in_chain(cam_class, "get_transform")?;
    let trms = hromatite_il2cpp::m3::methods(trc);
    let ms = hromatite_il2cpp::m3::methods(cam_class);
    let get_proj = method_entry(&ms, "get_projectionMatrix");
    let get_view = method_entry(&ms, "get_worldToCameraMatrix");
    let get_main = method_entry(&ms, "get_main");
    if get_proj == 0 || get_view == 0 || get_main == 0 {
        return None;
    }
    let rec = CamApi {
        get_main,
        get_transform,
        tr_position: method_entry(&trms, "get_position"),
        tr_forward: method_entry(&trms, "get_forward"),
        get_proj,
        get_view,
        get_pixw: method_entry(&ms, "get_pixelWidth"),
        get_pixh: method_entry(&ms, "get_pixelHeight"),
    };
    let ptr = Box::into_raw(Box::new(rec));
    CAM.store(ptr as usize, Ordering::Relaxed);
    // SAFETY: freshly boxed and leaked on purpose; valid for the process.
    Some(unsafe { &*ptr })
}

/// One player as the SDK sees it.
struct Pl {
    api: usize,
    id: usize,
    pos: Option<(f32, f32, f32)>,
    name: Option<String>,
}

struct Players {
    list: usize,
    size: usize,
    items: usize,
    rows: Vec<Pl>,
}

/// Walk `List<VRCPlayerApi>` into plain Rust data. Positions come from
/// `GetPosition(player)` (Vector3 through a hidden output pointer).
fn enumerate_players(sdk: &Sdk, with_name: bool) -> Result<Players, String> {
    // SAFETY: get_AllPlayers is the SDK's static accessor (no arguments).
    let list = match unsafe { hromatite_il2cpp::veh::call0(sdk.get_all) } {
        Ok(v) if v >= 0x10000 => v,
        Ok(_) => {
            return Ok(Players {
                list: 0,
                size: 0,
                items: 0,
                rows: Vec::new(),
            });
        }
        Err(e) => return Err(format!("get_AllPlayers faulted: {e}")),
    };
    // List<T>: _items @+0x10, _size @+0x18; Il2CppArray: length @+0x18, data @+0x20.
    let items = hromatite_il2cpp::safe::read_usize(list + 0x10).unwrap_or(0);
    let size = hromatite_il2cpp::safe::read_u32(list + 0x18).unwrap_or(0) as usize;
    let maxlen = hromatite_il2cpp::safe::read_usize(items + 0x18).unwrap_or(0);
    let mut rows = Vec::new();
    for i in 0..size.min(maxlen).min(256) {
        let p = hromatite_il2cpp::safe::read_usize(items + 0x20 + i * 8).unwrap_or(0);
        if p < 0x10000 {
            continue;
        }
        // SAFETY: GetPlayerId(VRCPlayerApi*) is a pure accessor on a live instance.
        let id = unsafe { hromatite_il2cpp::veh::call1(sdk.get_id, p) }.unwrap_or(usize::MAX);
        let mut pos = None;
        if sdk.get_pos != 0 {
            let mut buf = [0u64; 2];
            // SAFETY: GetPosition returns Vector3 through a hidden output
            // pointer (rcx) with `this` in rdx; buf is a live stack buffer.
            if unsafe { hromatite_il2cpp::veh::call2(sdk.get_pos, buf.as_mut_ptr() as usize, p) }.is_ok() {
                pos = Some(vec3(buf));
            }
        }
        // The player's GameObject name (avatar/root name) — handy for a human
        // and read through UnityEngine.Object.get_name, nothing VRChat-specific.
        let mut name = None;
        if with_name
            && sdk.get_objects != 0
            && sdk.obj_name != 0
            // SAFETY: GetPlayerObjects is a pure accessor on this live SDK player.
            && let Ok(arr) = unsafe { hromatite_il2cpp::veh::call1(sdk.get_objects, p) }
            && arr >= 0x10000
        {
            let len = hromatite_il2cpp::safe::read_usize(arr + 0x18).unwrap_or(0);
            let first = hromatite_il2cpp::safe::read_usize(arr + 0x20).unwrap_or(0);
            if len > 0 && first >= 0x10000 {
                // SAFETY: Object.get_name on a live UnityEngine.Object.
                if let Ok(s) = unsafe { hromatite_il2cpp::veh::call1(sdk.obj_name, first) } {
                    name = il2cpp_string(s).filter(|s| !s.is_empty());
                }
            }
        }
        rows.push(Pl {
            api: p,
            id,
            pos,
            name,
        });
    }
    Ok(Players {
        list,
        size,
        items,
        rows,
    })
}

fn who_probe() -> Vec<String> {
    let mut out = Vec::new();
    let Some(sdk) = sdk() else {
        return vec!["who: VRCPlayerApi not resolved (no get_AllPlayers/GetPlayerId)".into()];
    };
    out.push(format!(
        "who: VRCPlayerApi {:#x}, get_pos {}",
        sdk.api_class,
        sdk.get_pos != 0
    ));
    let ps = match enumerate_players(sdk, true) {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("who: {e}"));
            return out;
        }
    };
    out.push(format!(
        "who: list {:#x}, size {}, items {:#x}, cap {}",
        ps.list,
        ps.size,
        ps.items,
        ps.rows.len()
    ));
    for (i, p) in ps.rows.iter().enumerate() {
        let pos = match p.pos {
            Some((x, y, z)) => format!("pos=({x:.2}, {y:.2}, {z:.2})"),
            None => "pos=?".to_string(),
        };
        let label = match &p.name {
            Some(n) => format!(" go=\"{n}\""),
            None => String::new(),
        };
        out.push(format!(
            "  player #{i}: api {:#x} id={} {pos}{label}",
            p.api, p.id
        ));
    }
    out.push(format!("who: {} player(s)", ps.rows.len()));
    out
}

// ---- esp: players projected to screen pixels (M4) --------------------------

/// `Matrix4x4` (64 B) returned through the hidden output pointer in `rcx`,
/// `this` in `rdx` — the same ABI the Vector3 getters use, only bigger.
fn call_matrix(f: usize, this: usize) -> Option<[f32; 16]> {
    let mut buf = [0u64; 8];
    // SAFETY: get_projectionMatrix/get_worldToCameraMatrix are pure getters; the
    // buffer is a live 64-byte stack array and the VEH guards the call.
    unsafe { hromatite_il2cpp::veh::call2(f, buf.as_mut_ptr() as usize, this) }.ok()?;
    let mut out = [0f32; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = f32::from_bits((buf[i / 2] >> ((i % 2) * 32)) as u32);
    }
    Some(out)
}

/// `Transform.get_position` / `get_forward` (Vector3 through a hidden pointer).
fn transform_vec3(entry: usize, this: usize) -> Option<(f32, f32, f32)> {
    if entry == 0 || this < 0x10000 {
        return None;
    }
    let mut buf = [0u64; 2];
    // SAFETY: instance getter, hidden Vector3 return pointer in rcx and `this`
    // in rdx; buf is a live stack buffer, VEH guards the call.
    unsafe { hromatite_il2cpp::veh::call2(entry, buf.as_mut_ptr() as usize, this) }.ok()?;
    Some(vec3(buf))
}

/// Column-major 4x4 multiply (`Matrix4x4` storage order in Unity).
fn mul4(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut s = 0f32;
            for k in 0..4 {
                s += a[row + k * 4] * b[k + col * 4];
            }
            r[row + col * 4] = s;
        }
    }
    r
}

fn mul_vec4(m: &[f32; 16], v: [f32; 4]) -> [f32; 4] {
    let mut r = [0f32; 4];
    for row in 0..4 {
        r[row] = m[row] * v[0] + m[row + 4] * v[1] + m[row + 8] * v[2] + m[row + 12] * v[3];
    }
    r
}

/// World → pixel projection. Screen coordinates are top-left origin, because
/// that is what the X11 overlay draws in; Unity works bottom-left.
fn project(pv: &[f32; 16], w: f32, h: f32, p: (f32, f32, f32)) -> Option<(f32, f32)> {
    let c = mul_vec4(pv, [p.0, p.1, p.2, 1.0]);
    if c[3] <= 0.0001 {
        return None; // behind the camera
    }
    let ndc_x = c[0] / c[3];
    let ndc_y = c[1] / c[3];
    Some(((ndc_x * 0.5 + 0.5) * w, (0.5 - ndc_y * 0.5) * h))
}

/// `esp` — every player's screen box, plus the camera rig it was computed from.
///
/// `projectionMatrix * worldToCameraMatrix` is read through the same guarded
/// struct-return path the Vector3 getters use, and the multiplication happens in
/// Rust, so no unknown ABI for struct *arguments* is involved (that is what
/// makes `WorldToScreenPoint` risky and this safe).
///
/// Self-check in the header: the main camera's own position must project to the
/// centre of the viewport, so `err` (pixels off centre) is 0 in a sane world.
fn esp_probe() -> Vec<String> {
    let mut out = Vec::new();
    let (Some(cam), Some(sdk)) = (cam_api(), sdk()) else {
        return vec![format!(
            "esp: unresolved (camera {}, players {})",
            cam_api().is_some(),
            sdk().is_some()
        )];
    };
    // SAFETY: Camera.get_main is the static accessor.
    let inst = match unsafe { hromatite_il2cpp::veh::call0(cam.get_main) } {
        Ok(v) if v >= 0x10000 => v,
        Ok(v) => {
            out.push(format!("esp: no main camera ({v:#x})"));
            return out;
        }
        Err(e) => {
            out.push(format!("esp: Camera.main faulted: {e}"));
            return out;
        }
    };
    let mut pw = if cam.get_pixw != 0 {
        // SAFETY: Camera.pixelWidth is a constant instance getter.
        unsafe { hromatite_il2cpp::veh::call1(cam.get_pixw, inst) }.unwrap_or(0) as u32
    } else {
        0
    };
    let mut ph = if cam.get_pixh != 0 {
        // SAFETY: Camera.pixelHeight is a constant instance getter.
        unsafe { hromatite_il2cpp::veh::call1(cam.get_pixh, inst) }.unwrap_or(0) as u32
    } else {
        0
    };
    if pw == 0 || ph == 0 {
        pw = 1920;
        ph = 1080;
    }
    // SAFETY: Component.get_transform on a live Camera instance.
    let tr = unsafe { hromatite_il2cpp::veh::call1(cam.get_transform, inst) }.unwrap_or(0);
    let cam_pos = transform_vec3(cam.tr_position, tr);
    let fwd = transform_vec3(cam.tr_forward, tr);
    let (Some(proj), Some(view)) = (
        call_matrix(cam.get_proj, inst),
        call_matrix(cam.get_view, inst),
    ) else {
        out.push("esp: projection/worldToCamera matrix faulted".into());
        return out;
    };
    let pv = mul4(&proj, &view);
    let (fw, fh) = (pw as f32, ph as f32);
    let err = cam_pos
        .and_then(|p| project(&pv, fw, fh, p))
        .map(|(x, y)| (x - fw / 2.0).abs().max((y - fh / 2.0).abs()))
        .unwrap_or(-1.0);
    let players = match enumerate_players(sdk, true) {
        Ok(v) => v.rows,
        Err(e) => {
            out.push(format!("esp: {e}"));
            return out;
        }
    };
    let head = |p: (f32, f32, f32)| (p.0, p.1 + 1.70, p.2);
    out.push(format!(
        "esp: head w={pw} h={ph} err={err:.1} cam=({:.2}, {:.2}, {:.2}) fwd=({:.2}, {:.2}, {:.2})",
        cam_pos.map_or(0.0, |p| p.0),
        cam_pos.map_or(0.0, |p| p.1),
        cam_pos.map_or(0.0, |p| p.2),
        fwd.map_or(0.0, |p| p.0),
        fwd.map_or(0.0, |p| p.1),
        fwd.map_or(0.0, |p| p.2)
    ));
    for p in players {
        let Some(pos) = p.pos else { continue };
        let Some((sx, sy)) = project(&pv, fw, fh, pos) else {
            continue;
        };
        let Some((_, hy)) = project(&pv, fw, fh, head(pos)) else {
            continue;
        };
        let dist = match cam_pos {
            Some(c) => {
                let (dx, dy, dz) = (pos.0 - c.0, pos.1 - c.1, pos.2 - c.2);
                (dx * dx + dy * dy + dz * dz).sqrt()
            }
            None => -1.0,
        };
        // `w=` carries the world position too: the host overlay uses it for the
        // radar, and it costs nothing here.
        out.push(format!(
            "esp: id={} d={dist:.2} x={sx:.1} y={sy:.1} h={hy:.1} w={:.2},{:.2},{:.2} name={}",
            p.id,
            pos.0,
            pos.1,
            pos.2,
            p.name.unwrap_or_default()
        ));
    }
    out
}

/// The in-game `help` list, generated from the shared command table.
///
/// The same table feeds the host TUI palette, completion and risk gate, so a
/// command is declared once and can never be listed in one place and missing in
/// another.
fn help_lines() -> Vec<String> {
    hromatite_commands::help_lines(hromatite_commands::Scope::Resident)
}

/// Execute one command. Returns the answer lines and a control action.
#[cfg(target_os = "windows")]
pub fn dispatch(line: &str) -> (Vec<String>, Action) {
    let mut it = line.splitn(4, char::is_whitespace);
    let cmd = it.next().unwrap_or("").to_ascii_lowercase();
    let a1 = it.next().unwrap_or("");
    let a2 = it.next().unwrap_or("");
    let rest = it.next().unwrap_or("");

    match cmd.as_str() {
        "help" | "?" => (help_lines(), Action::None),
        "q" | "qword" => match parse_addr(a1) {
            Some(addr) => {
                let n = a2.parse::<usize>().unwrap_or(8).min(64);
                (dump_qwords(addr, n), Action::None)
            }
            None => (vec!["usage: q <addr> [n]".into()], Action::None),
        },
        "b" | "bytes" => match parse_addr(a1) {
            Some(addr) => {
                let n = a2.parse::<usize>().unwrap_or(32).min(512);
                let bs = bytes(addr, n);
                let hex: Vec<String> = bs.iter().map(|b| format!("{b:02x}")).collect();
                (
                    vec![format!("{addr:#x}: {} ({} bytes)", hex.join(" "), bs.len())],
                    Action::None,
                )
            }
            None => (vec!["usage: b <addr> [n]".into()], Action::None),
        },
        "s" => match parse_addr(a1) {
            Some(addr) => (
                vec![format!("{addr:#x}: '{}'", ascii(addr, 128))],
                Action::None,
            ),
            None => (vec!["usage: s <addr>".into()], Action::None),
        },
        "ws" => match parse_addr(a1) {
            Some(addr) => (
                vec![format!("{addr:#x}: '{}'", utf16(addr, 128))],
                Action::None,
            ),
            None => (vec!["usage: ws <addr>".into()], Action::None),
        },
        // Boehm's own thread table vs. the threads the process really has.
        // `gct` alone = detection report + full dump; `gct <rva>` forces a base.
        "gct" => {
            let forced = parse_addr(a1).map(|v| v as u32);
            let mut out = hromatite_il2cpp::gcthreads::probe(forced);
            match hromatite_il2cpp::gcthreads::snapshot(forced) {
                Some(t) => out.extend(t.lines()),
                None => out.push("gct: table not found (see candidate scores above)".into()),
            }
            (out, Action::None)
        }
        // Writes. Deliberate, single pokes — the payload has no other way to
        // change game state, and a dead Boehm record has to be unlinked by hand
        // when the automatic repair is off.
        "wq" => match (parse_addr(a1), parse_addr(a2)) {
            (Some(addr), Some(v)) => {
                let ok = hromatite_il2cpp::safe::write_usize(addr, v);
                (
                    vec![format!(
                        "wq {addr:#x} <- {v:#x}: {ok} (now {:#x})",
                        hromatite_il2cpp::safe::read_usize(addr).unwrap_or(0)
                    )],
                    Action::None,
                )
            }
            _ => (vec!["usage: wq <addr> <hex value>".into()], Action::None),
        },
        "wb" => match (parse_addr(a1), rest.trim()) {
            (Some(addr), hex) if !hex.is_empty() => {
                let cleaned: String = hex
                    .chars()
                    .filter(|c| !c.is_whitespace() && *c != '_')
                    .collect();
                let bytes: Option<Vec<u8>> = cleaned
                    .len()
                    .is_multiple_of(2)
                    .then(|| {
                        (0..cleaned.len())
                            .step_by(2)
                            .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
                            .collect::<Option<Vec<u8>>>()
                    })
                    .flatten();
                match bytes {
                    Some(b) => {
                        let ok = hromatite_il2cpp::safe::write_bytes(addr, &b);
                        (
                            vec![format!("wb {addr:#x} <- {} byte(s): {ok}", b.len())],
                            Action::None,
                        )
                    }
                    None => (
                        vec!["usage: wb <addr> <hex bytes, e.g. 0100ff>".into()],
                        Action::None,
                    ),
                }
            }
            _ => (vec!["usage: wb <addr> <hex bytes>".into()], Action::None),
        },
        // Resume every thread Boehm knows about: if a collection died half-way,
        // the world is left suspended and the game looks frozen while we still
        // run. Harmless when nothing is suspended (a resume of a running thread
        // only decrements a count that is already 0... and returns 0/-1).
        "gctresume" => match hromatite_il2cpp::gcthreads::snapshot(None) {
            Some(t) => (hromatite_il2cpp::gcthreads::resume_all(&t), Action::None),
            None => (vec!["gctresume: table not found".into()], Action::None),
        },
        // Raw guarded call: `call <addr> [a1] [a2] [a3]`.
        "call" => {
            let args: Vec<usize> = line
                .split_whitespace()
                .skip(1)
                .map(|s| parse_addr(s).unwrap_or(0))
                .collect();
            let f = args.first().copied().unwrap_or(0);
            if f == 0 {
                return (
                    vec!["usage: call <addr> [a1] [a2] [a3]".into()],
                    Action::None,
                );
            }
            let g = |i: usize| args.get(i + 1).copied().unwrap_or(0);
            let mut out = vec![format!(
                "call {f:#x}({:#x}, {:#x}, {:#x})",
                g(0),
                g(1),
                g(2)
            )];
            // SAFETY: this is the explicit low-level `call` command: the user
            // supplied the Win64 address and all opaque pointer arguments knowingly.
            match unsafe { hromatite_il2cpp::veh::call3(f, g(0), g(1), g(2)) } {
                Ok(v) => out.push(format!("  -> {v:#x}")),
                Err(e) => out.push(format!("  fault: {e}")),
            }
            (out, Action::None)
        }
        // Unlink every dead record from Boehm's table right now.
        "gctfix" => {
            let mut out = Vec::new();
            match hromatite_il2cpp::gcthreads::snapshot(None) {
                Some(t) => {
                    out.push(t.summary());
                    if t.dead == 0 {
                        out.push("nothing to repair".into());
                    } else {
                        out.extend(hromatite_il2cpp::gcthreads::repair(&t));
                        if let Some(t2) = hromatite_il2cpp::gcthreads::snapshot(None) {
                            out.push(format!("after repair: {}", t2.summary()));
                        }
                    }
                }
                None => out.push("gctfix: table not found".into()),
            }
            (out, Action::None)
        }
        // All threads with their start address (which function created them) and
        // whether Boehm has a record for them: a thread without a record, or a
        // record whose thread is gone, is what aborts the collector.
        "threads" => {
            let table = hromatite_il2cpp::gcthreads::snapshot(None);
            (hromatite_il2cpp::gcthreads::thread_rows(table.as_ref()), Action::None)
        }
        // In-game nuklear UI: report the hook, the frame count and the panel
        // state. The overlay itself is drawn from the render thread's Present
        // hook, so there is nothing to toggle here — `HROMATITE_UI=1` is the
        // switch and it is read at startup.
        "ui" => match a1 {
            "" | "status" => (hromatite_render::ui::status(), Action::None),
            _ => (vec!["usage: ui [status]".into()], Action::None),
        },
        // Raw diagnostic calls stay behind the same VEH boundary as M3 getters.
        // `guard <addr>` calls a raw GameAssembly RVA-relative function through
        // the vectored handler: the answer is either a return value or a fault
        // line, never a dead game (that is the whole point of `veh`).
        "guard" => match parse_addr(a1) {
            Some(addr) => {
                hromatite_il2cpp::log::log_tag("con", &format!("guard {addr:#x}: calling under VEH"));
                // SAFETY: the explicit guard command is a delegated raw Win64
                // probe; VEH captures the calling thread before dereferencing code.
                match unsafe { hromatite_il2cpp::veh::call0(addr) } {
                    Ok(v) => (
                        vec![format!("guard {addr:#x}: returned {v:#x}")],
                        Action::None,
                    ),
                    Err(f) => (vec![format!("guard {addr:#x}: {f}")], Action::None),
                }
            }
            None => (vec!["usage: guard <addr>".into()], Action::None),
        },
        // ---- Unity 6 layout (verified live 2026-11-15, see m3.rs) ---------
        // `m3f <ns> [name]` — search the class table directly (no API call).
        "m3f" => {
            let ns = a1;
            let name = a2;
            let hits = hromatite_il2cpp::m3::find(ns, name, 120_000);
            let mut out = vec![format!(
                "m3f {ns:?} {name:?}: {} class(es), table {:#x}",
                hits.len(),
                hromatite_il2cpp::m3::class_table().unwrap_or(0)
            )];
            for (i, p) in hits.iter().take(32) {
                out.push(format!("  #{i} {p:#x}  {}", hromatite_il2cpp::m3::class_full_name(*p)));
            }
            (out, Action::None)
        }
        // `m3s <substr>` — search only the *materialised* class table (no calls).
        "m3s" => {
            let want = a1.to_ascii_lowercase();
            let mut out = vec![format!("m3s {want:?}")];
            let mut n = 0usize;
            for (i, p) in hromatite_il2cpp::m3::classes(120_000) {
                let nm = hromatite_il2cpp::m3::class_name(p).unwrap_or_default();
                if want.is_empty() || nm.to_ascii_lowercase().contains(&want) {
                    n += 1;
                    if n <= 40 {
                        out.push(format!("  #{i} {p:#x}  {}", hromatite_il2cpp::m3::class_full_name(p)));
                    }
                }
            }
            out.insert(1, format!("  {n} match(es)"));
            (out, Action::None)
        }
        // `m3l <assembly> [substr] [max]` — enumerate an image's classes through
        // `il2cpp_image_get_class`, which also *materialises* lazily-built
        // classes (VRChat's own assembly is not in the table until touched).
        "m3l" => {
            let asm = if a1.is_empty() { "Assembly-CSharp" } else { a1 };
            let filt = a2.to_ascii_lowercase();
            let max = parse_addr(rest.trim()).unwrap_or(30_000).min(60_000);
            if hromatite_il2cpp::procs::own_module(hromatite_il2cpp::GAME_ASSEMBLY).is_none() {
                return (
                    vec!["m3l: GameAssembly.dll not loaded".into()],
                    Action::None,
                );
            }
            let Some(f) = hromatite_il2cpp::m3::image_get_class_addr() else {
                return (
                    vec!["m3l: image_get_class outside the module".into()],
                    Action::None,
                );
            };
            let first = match hromatite_il2cpp::il2cpp::find_domain() {
                Some(v) => v,
                None => {
                    return (
                        vec!["m3l: get_corlib getter not found".into()],
                        Action::None,
                    );
                }
            };
            let image = match hromatite_il2cpp::m3::image_of(first, asm) {
                Some(i) => i,
                None => return (vec![format!("m3l: no image for '{asm}'")], Action::None),
            };
            let mut out = vec![format!(
                "m3l {asm}: image {image:#x}, fn {f:#x}, filter {filt:?}, max {max}"
            )];
            let mut seen = 0usize;
            let mut shown = 0usize;
            let mut faults = 0usize;
            for i in 0..max {
                // SAFETY: image_get_class(image, i) is the measured (image,index)
                // ABI; i stays inside the caller's bound and VEH contains faults.
                let cls = match unsafe { hromatite_il2cpp::veh::call2(f, image, i) } {
                    Ok(v) => v,
                    Err(_) => {
                        faults += 1;
                        if faults > 32 {
                            out.push(format!("  stopped at {i}: too many faults"));
                            break;
                        }
                        continue;
                    }
                };
                if cls < 0x10000 || !hromatite_il2cpp::safe::readable_at(cls) {
                    continue;
                }
                seen += 1;
                let nm = hromatite_il2cpp::m3::class_name(cls).unwrap_or_default();
                if nm.is_empty() {
                    continue;
                }
                if filt.is_empty() || nm.to_ascii_lowercase().contains(&filt) {
                    shown += 1;
                    if shown <= 60 {
                        out.push(format!(
                            "  #{i} {cls:#x}  {}.{nm}",
                            hromatite_il2cpp::m3::class_namespace(cls).unwrap_or_default()
                        ));
                    }
                }
            }
            out.push(format!(
                "  {seen} class(es) seen, {shown} match(es), {faults} fault(s)"
            ));
            (out, Action::None)
        }
        // `m3c <class>` — fields and methods through the measured layout.
        "m3c" => match parse_addr(a1) {
            Some(c) if c >= 0x10000 => {
                let mut out = vec![format!(
                    "m3c {c:#x}  {}  (image {:#x})",
                    hromatite_il2cpp::m3::class_full_name(c),
                    hromatite_il2cpp::safe::read_usize(c).unwrap_or(0)
                )];
                let fld = hromatite_il2cpp::m3::fields(c);
                out.push(format!("  fields: {}", fld.len()));
                for f in fld.iter().take(40) {
                    out.push(format!(
                        "    +{:#06x} {:<28} type {:#x} parent {:#x}",
                        f.offset_or_index, f.name, f.ty, f.parent
                    ));
                }
                let ms = hromatite_il2cpp::m3::methods(c);
                out.push(format!("  methods: {}", ms.len()));
                for m in ms.iter().take(60) {
                    out.push(format!("    {:<36} entry {:#x}", m.name, m.entry));
                }
                (out, Action::None)
            }
            _ => (vec!["usage: m3c <classaddr>".into()], Action::None),
        },
        // `m3i <assembly>` — Il2CppImage* (images are getter+i*0x48).
        "m3i" => match hromatite_il2cpp::il2cpp::find_domain() {
            Some(first) => {
                let mut out = vec![format!("m3i: corlib image {first:#x}")];
                let want = if a1.is_empty() {
                    "UnityEngine.CoreModule"
                } else {
                    a1
                };
                match hromatite_il2cpp::m3::image_of(first, want) {
                    Some(img) => {
                        out.push(format!(
                            "  {want}: image {img:#x}, il2cpp_image_get_class {:#x}",
                            hromatite_il2cpp::m3::image_get_class_addr().unwrap_or(0)
                        ));
                    }
                    None => out.push(format!("  {want}: not found")),
                }
                (out, Action::None)
            }
            None => (
                vec!["m3i: get_corlib getter not found".into()],
                Action::None,
            ),
        },
        // Native PlayerLoop discovery/status; still no SetPlayerLoop call.
        "loopc" => (hromatite_il2cpp::playerloop::snapshot_lines(), Action::None),
        "loop" => (hromatite_il2cpp::playerloop::status(), Action::None),
        // `pos` — the M3 proof: Camera.main -> transform -> position, i.e. a
        // real Vector3 read out of the running game with plain memory reads
        // plus three guarded method calls.
        "pos" => (pos_probe(), Action::None),
        // `who` — players in the instance through the public SDK surface:
        // VRCPlayerApi.get_AllPlayers → List<VRCPlayerApi> → GetPlayerId /
        // GetPosition. Obfuscated VRChat classes are not needed at all.
        "who" => (who_probe(), Action::None),
        // `esp` — the same player list projected to *pixels*, so the host
        // overlay can draw boxes over them. Costs four guarded calls per line of
        // the camera rig (main, transform, both matrices, pixel size), which is
        // why the method entries are cached in `CamApi`.
        "esp" => (esp_probe(), Action::None),
        // ---- plan 5 self-tests: prove guard()/VEH actually hold -----------
        // `panic` must produce a `[dll] PANIC at file:line:` line and *not*
        // kill the game; the session continues with the next command.
        "panic" => {
            let msg = line
                .split_once(char::is_whitespace)
                .map_or("console panic test", |x| x.1)
                .trim();
            let msg = if msg.is_empty() {
                "console panic test"
            } else {
                msg
            };
            hromatite_il2cpp::log::log_tag("con", &format!("panic test: about to panic with '{msg}'"));
            panic!("console panic test: {msg}");
        }
        // `av <addr>` shows the difference between the two failure classes:
        // the safe read survives (returns None), the raw deref is exactly what
        // the VEH from plan 2 has to catch. Default 0x1 — never mapped.
        "av" => {
            let addr = parse_addr(a1).unwrap_or(1);
            let guarded = match hromatite_il2cpp::safe::read_u64(addr) {
                Some(v) => format!("{v:#016x}"),
                None => "(unreadable, survived)".to_string(),
            };
            let head = format!(
                "av @{addr:#x}: readable_at={}, safe::read_u64={guarded}",
                hromatite_il2cpp::safe::readable_at(addr)
            );
            // log before the raw deref: if the process dies here, this line is
            // the only witness we get.
            hromatite_il2cpp::log::log_tag("con", &head);
            // SAFETY: this command deliberately performs an unaligned probe of
            // an arbitrary address under VEH, as documented by diagnostic_raw_read.
            let raw = match unsafe { hromatite_il2cpp::safe::diagnostic_raw_read(addr) } {
                Ok(v) => format!("raw deref @{addr:#x} = {v:#016x} (address was mapped)"),
                Err(e) => format!("raw deref @{addr:#x}: {e} (VEH caught it)"),
            };
            (vec![head, raw], Action::None)
        }
        "latec" | "latehook" | "late" => {
            (hromatite_il2cpp::hook::command(line).unwrap_or_default(), Action::None)
        }
        // `eac.module` is the one eac.* command the resident owns: it reads the
        // anti-cheat's own image through Win32 and `/proc`, which a feature
        // image cannot do. Everything else in the eac.* family stays with the
        // feature image.
        "eac.module" => (
            hromatite_eac::eacimage::command(
                line.split_once(char::is_whitespace)
                    .map(|(_, rest)| rest.trim())
                    .unwrap_or(""),
            ),
            Action::None,
        ),
        "seal" => (
            vec!["seal requested, returning from DllMain".into()],
            Action::Seal,
        ),
        "quit" => (vec!["bye".into()], Action::Quit),
        "" => (Vec::new(), Action::None),
        other => (
            vec![format!("unknown command: {other} (try help)")],
            Action::None,
        ),
    }
}

/// Host tests exercise pure formatting/projection code without instantiating
/// the Windows dispatcher and its Win32/VEH import graph.
#[cfg(not(target_os = "windows"))]
pub fn dispatch(line: &str) -> (Vec<String>, Action) {
    let cmd = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match cmd.as_str() {
        "help" | "?" => (help_lines(), Action::None),
        "eac.module" => (
            vec!["eac.module: needs the Windows payload".into()],
            Action::None,
        ),
        "" => (Vec::new(), Action::None),
        other => (
            vec![format!("unknown command: {other} (try help)")],
            Action::None,
        ),
    }
}

#[cfg(test)]
mod command_tests {
    use super::dispatch;

    #[test]
    fn the_one_resident_eac_command_is_not_reported_unknown() {
        let (lines, _) = dispatch("eac.module 64");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            !lines[0].starts_with("unknown command"),
            "eac.module is resident-scoped but the console does not route it: {lines:?}"
        );
    }

    #[test]
    fn help_is_generated_and_lists_only_routable_resident_commands() {
        let (lines, _) = dispatch("help");
        let listed: Vec<&str> = lines
            .iter()
            .map(|line| line.split_whitespace().next().unwrap_or(""))
            .collect();
        for name in [
            "help",
            "pos",
            "who",
            "esp",
            "q",
            "b",
            "s",
            "ws",
            "gct",
            "threads",
            "gctfix",
            "gctresume",
            "call",
            "wq",
            "wb",
            "guard",
            "m3f",
            "m3s",
            "m3l",
            "m3c",
            "m3i",
            "loopc",
            "loop",
            "latec",
            "latehook",
            "late",
            "reload",
            "rollback",
            "config",
            "panic",
            "av",
            "seal",
            "quit",
        ] {
            assert!(listed.contains(&name), "resident help lost {name}");
        }
        // A feature image owns the eac.* family; the one exception is
        // `eac.module`, which reads EAC's own modules through Win32 and is
        // therefore dispatched by the resident (checked before the eac.* branch).
        assert!(listed.contains(&"eac.module"), "{listed:?}");
        assert!(
            !listed
                .iter()
                .any(|name| name.starts_with("eac.") && *name != "eac.module"),
            "{listed:?}"
        );
        assert!(!listed.contains(&"feature"), "{listed:?}");
        // Every line is the shared table's line, not a private copy.
        for line in &lines {
            assert!(
                hromatite_commands::SPECS
                    .iter()
                    .any(|spec| spec.help_line() == *line),
                "hand-written help line survived: {line}"
            );
        }
    }
}

#[cfg(test)]
mod project_tests {
    //! The projection is the one part of `esp` that can be checked without the
    //! game: `projectionMatrix * worldToCameraMatrix` follows Unity's column-major
    //! `Matrix4x4` storage, and a point straight ahead must land dead centre.

    use super::{mul_vec4, mul4, project};

    fn perspective(fov_y_deg: f32, aspect: f32) -> [f32; 16] {
        let f = 1.0 / (fov_y_deg.to_radians() / 2.0).tan();
        let mut m = [0f32; 16];
        m[0] = f / aspect;
        m[5] = f;
        m[10] = -1.0002;
        m[11] = -1.0;
        m[14] = -0.20002;
        m
    }

    fn identity() -> [f32; 16] {
        let mut m = [0f32; 16];
        for i in 0..4 {
            m[i * 5] = 1.0;
        }
        m
    }

    #[test]
    fn centre_of_view_projects_to_centre_of_screen() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        let (x, y) = project(&pv, 1920.0, 1080.0, (0.0, 0.0, -5.0)).unwrap();
        assert!((x - 960.0).abs() < 0.5, "x = {x}");
        assert!((y - 540.0).abs() < 0.5, "y = {y}");
    }

    #[test]
    fn behind_the_camera_is_none() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        assert!(project(&pv, 1920.0, 1080.0, (0.0, 0.0, 5.0)).is_none());
    }

    #[test]
    fn right_of_centre_lands_right_of_centre() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        let (x, _) = project(&pv, 1920.0, 1080.0, (1.0, 0.0, -5.0)).unwrap();
        assert!(x > 960.0, "x = {x}");
    }

    #[test]
    fn column_major_multiply_matches_hand_math() {
        let mut a = identity();
        a[12] = 3.0; // translation on x, stored in the last column
        let v = mul_vec4(&a, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(v[0], 4.0);
        assert_eq!(v[1], 0.0);
    }
}
