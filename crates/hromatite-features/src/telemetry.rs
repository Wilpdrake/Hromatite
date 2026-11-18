//! Camera/player telemetry and the established host-overlay text contract.
//! Struct-returning Unity methods use the measured Win64 hidden out pointer:
//! `rcx = output`, `rdx = this`; invocation remains behind `HostApiV1`.

use crate::m3::{CLASS_MAX, Metadata};
use hromatite_abi::{STATUS_NOT_READY, STATUS_OK};

pub(crate) type Vec3 = (f32, f32, f32);

/// One read of the world for the in-game renderer: player positions plus where
/// the camera is and where it looks. Refreshed at a fraction of the frame rate.
pub(crate) struct Scene {
    pub players: Vec<(usize, Vec3)>,
    pub eye: Option<Vec3>,
    pub forward: Option<Vec3>,
}

/// Player body positions and the camera pose, reusing the same resolved SDK and
/// camera entries the `who`/`esp` commands use.
pub(crate) fn scene(metadata: &mut Metadata<'_>, cache: &mut Cache) -> Option<Scene> {
    let sdk = sdk(metadata, cache)?;
    let players = enumerate_players(metadata, &sdk, false)
        .map(|players| {
            players
                .rows
                .into_iter()
                .filter_map(|row| row.pos.map(|pos| (row.id, pos)))
                .collect()
        })
        .unwrap_or_default();
    let mut eye = None;
    let mut forward = None;
    if let Some(camera) = camera(metadata, cache)
        && let Ok(instance) = metadata.host.call(camera.get_main, &[])
        && instance >= 0x10000
        && let Ok(transform) = metadata.host.call(camera.get_transform, &[instance])
        && transform >= 0x10000
    {
        eye = call_vec3(metadata, camera.tr_position, transform);
        forward = call_vec3(metadata, camera.tr_forward, transform);
    }
    Some(Scene {
        players,
        eye,
        forward,
    })
}

#[derive(Clone, Copy)]
struct Sdk {
    api_class: usize,
    get_all: usize,
    get_id: usize,
    get_pos: usize,
    get_objects: usize,
    obj_name: usize,
}

#[derive(Clone, Copy)]
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

#[derive(Default)]
pub struct Cache {
    sdk: Option<Sdk>,
    camera: Option<CamApi>,
}

struct Player {
    api: usize,
    id: usize,
    pos: Option<Vec3>,
    name: Option<String>,
}

struct Players {
    list: usize,
    size: usize,
    items: usize,
    rows: Vec<Player>,
}

fn vec3(bits: [u64; 2]) -> Vec3 {
    (
        f32::from_bits(bits[0] as u32),
        f32::from_bits((bits[0] >> 32) as u32),
        f32::from_bits(bits[1] as u32),
    )
}

fn call_vec3(metadata: &Metadata<'_>, entry: usize, this: usize) -> Option<Vec3> {
    if entry == 0 || this < 0x10000 {
        return None;
    }
    let mut out = [0u64; 2];
    metadata
        .host
        .call(entry, &[out.as_mut_ptr() as usize, this])
        .ok()?;
    Some(vec3(out))
}

fn call_matrix(metadata: &Metadata<'_>, entry: usize, this: usize) -> Option<[f32; 16]> {
    let mut out = [0u64; 8];
    metadata
        .host
        .call(entry, &[out.as_mut_ptr() as usize, this])
        .ok()?;
    let mut matrix = [0f32; 16];
    for (index, value) in matrix.iter_mut().enumerate() {
        *value = f32::from_bits((out[index / 2] >> ((index % 2) * 32)) as u32);
    }
    Some(matrix)
}

fn resolve_sdk(metadata: &mut Metadata<'_>) -> Option<Sdk> {
    let api_class = metadata.class_or_materialise("VRC.SDKBase", "VRCPlayerApi", "VRCSDKBase")?;
    let methods = metadata.methods(api_class);
    let get_all = Metadata::method_entry(&methods, "get_AllPlayers");
    let get_id = Metadata::method_entry(&methods, "GetPlayerId");
    if get_all == 0 || get_id == 0 {
        return None;
    }
    let obj_name = metadata
        .find("UnityEngine", "Object", CLASS_MAX)
        .first()
        .map(|(_, class)| Metadata::method_entry(&metadata.methods(*class), "get_name"))
        .unwrap_or(0);
    Some(Sdk {
        api_class,
        get_all,
        get_id,
        get_pos: Metadata::method_entry(&methods, "GetPosition"),
        get_objects: Metadata::method_entry(&methods, "GetPlayerObjects"),
        obj_name,
    })
}

fn sdk(metadata: &mut Metadata<'_>, cache: &mut Cache) -> Option<Sdk> {
    if cache.sdk.is_none() {
        cache.sdk = resolve_sdk(metadata);
    }
    cache.sdk
}

fn resolve_camera(metadata: &mut Metadata<'_>) -> Option<CamApi> {
    let camera_class = metadata.find("UnityEngine", "Camera", CLASS_MAX).first()?.1;
    let (_, get_transform) = metadata.method_in_chain(camera_class, "get_transform")?;
    // `get_transform` is owned by Component; its methods are on Transform.
    let transform_class = metadata
        .find("UnityEngine", "Transform", CLASS_MAX)
        .first()?
        .1;
    let transform_methods = metadata.methods(transform_class);
    let methods = metadata.methods(camera_class);
    let get_proj = Metadata::method_entry(&methods, "get_projectionMatrix");
    let get_view = Metadata::method_entry(&methods, "get_worldToCameraMatrix");
    let get_main = Metadata::method_entry(&methods, "get_main");
    if get_proj == 0 || get_view == 0 || get_main == 0 {
        return None;
    }
    Some(CamApi {
        get_main,
        get_transform,
        tr_position: Metadata::method_entry(&transform_methods, "get_position"),
        tr_forward: Metadata::method_entry(&transform_methods, "get_forward"),
        get_proj,
        get_view,
        get_pixw: Metadata::method_entry(&methods, "get_pixelWidth"),
        get_pixh: Metadata::method_entry(&methods, "get_pixelHeight"),
    })
}

fn camera(metadata: &mut Metadata<'_>, cache: &mut Cache) -> Option<CamApi> {
    if cache.camera.is_none() {
        cache.camera = resolve_camera(metadata);
    }
    cache.camera
}

fn il2cpp_string(metadata: &Metadata<'_>, pointer: usize) -> Option<String> {
    if pointer < 0x10000 {
        return None;
    }
    let len = metadata.host.read_u32(pointer.checked_add(0x10)?)? as usize;
    if len > 4096 {
        return None;
    }
    let bytes = metadata
        .host
        .read_bytes(pointer.checked_add(0x14)?, len.checked_mul(2)?)?;
    let (chunks, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = chunks
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn enumerate_players(
    metadata: &Metadata<'_>,
    sdk: &Sdk,
    with_name: bool,
) -> Result<Players, String> {
    let list = match metadata.host.call(sdk.get_all, &[]) {
        Ok(list) if list >= 0x10000 => list,
        Ok(_) => {
            return Ok(Players {
                list: 0,
                size: 0,
                items: 0,
                rows: Vec::new(),
            });
        }
        Err(status) => return Err(format!("get_AllPlayers faulted: host status {status}")),
    };
    // List<T>: _items@+0x10, _size@+0x18; Il2CppArray: length@+0x18, data@+0x20.
    let items = metadata.host.read_usize(list + 0x10).unwrap_or(0);
    let size = metadata.host.read_u32(list + 0x18).unwrap_or(0) as usize;
    let max_len = metadata.host.read_usize(items + 0x18).unwrap_or(0);
    let mut rows = Vec::new();
    for index in 0..size.min(max_len).min(256) {
        let player = metadata
            .host
            .read_usize(items + 0x20 + index * 8)
            .unwrap_or(0);
        if player < 0x10000 {
            continue;
        }
        let id = metadata
            .host
            .call(sdk.get_id, &[player])
            .unwrap_or(usize::MAX);
        let pos = call_vec3(metadata, sdk.get_pos, player);
        let mut name = None;
        if with_name
            && sdk.get_objects != 0
            && sdk.obj_name != 0
            && let Ok(array) = metadata.host.call(sdk.get_objects, &[player])
            && array >= 0x10000
        {
            let len = metadata.host.read_usize(array + 0x18).unwrap_or(0);
            let first = metadata.host.read_usize(array + 0x20).unwrap_or(0);
            if len > 0
                && first >= 0x10000
                && let Ok(string) = metadata.host.call(sdk.obj_name, &[first])
            {
                name = il2cpp_string(metadata, string).filter(|name| !name.is_empty());
            }
        }
        rows.push(Player {
            api: player,
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

fn who(metadata: &mut Metadata<'_>, cache: &mut Cache) -> (Vec<String>, i32) {
    let Some(sdk) = sdk(metadata, cache) else {
        return (
            vec!["who: VRCPlayerApi not resolved (no get_AllPlayers/GetPlayerId)".into()],
            STATUS_NOT_READY,
        );
    };
    let mut out = vec![format!(
        "who: VRCPlayerApi {:#x}, get_pos {}",
        sdk.api_class,
        sdk.get_pos != 0
    )];
    let players = match enumerate_players(metadata, &sdk, true) {
        Ok(players) => players,
        Err(error) => {
            out.push(format!("who: {error}"));
            return (out, STATUS_OK);
        }
    };
    out.push(format!(
        "who: list {:#x}, size {}, items {:#x}, cap {}",
        players.list,
        players.size,
        players.items,
        players.rows.len()
    ));
    for (index, player) in players.rows.iter().enumerate() {
        let pos = match player.pos {
            Some((x, y, z)) => format!("pos=({x:.2}, {y:.2}, {z:.2})"),
            None => "pos=?".to_string(),
        };
        let label = player
            .name
            .as_ref()
            .map_or_else(String::new, |name| format!(" go=\"{name}\""));
        out.push(format!(
            "  player #{index}: api {:#x} id={} {pos}{label}",
            player.api, player.id
        ));
    }
    out.push(format!("who: {} player(s)", players.rows.len()));
    (out, STATUS_OK)
}

fn pos(metadata: &mut Metadata<'_>) -> (Vec<String>, i32) {
    let mut out = Vec::new();
    let Some((index, camera_class)) = metadata
        .find("UnityEngine", "Camera", CLASS_MAX)
        .first()
        .copied()
    else {
        return (
            vec!["pos: Camera class not found (class table)".into()],
            STATUS_NOT_READY,
        );
    };
    out.push(format!(
        "pos: Camera class {camera_class:#x} ({}), #{index}",
        metadata.class_full_name(camera_class)
    ));
    let main = Metadata::method_entry(&metadata.methods(camera_class), "get_main");
    if main == 0 {
        out.push("pos: no Camera.get_main".into());
        return (out, STATUS_NOT_READY);
    }
    let camera_instance = match metadata.host.call(main, &[]) {
        Ok(instance) => instance,
        Err(status) => {
            out.push(format!("pos: Camera.main faulted: host status {status}"));
            return (out, STATUS_OK);
        }
    };
    out.push(format!("pos: Camera.main = {camera_instance:#x}"));
    if camera_instance < 0x10000 {
        out.push("pos: no main camera in this world".into());
        return (out, STATUS_OK);
    }
    let Some((owner, get_transform)) = metadata.method_in_chain(camera_class, "get_transform")
    else {
        out.push("pos: no get_transform in the class chain".into());
        return (out, STATUS_NOT_READY);
    };
    out.push(format!(
        "pos: get_transform from {}",
        metadata.class_full_name(owner)
    ));
    let transform = match metadata.host.call(get_transform, &[camera_instance]) {
        Ok(transform) => transform,
        Err(status) => {
            out.push(format!("pos: get_transform faulted: host status {status}"));
            return (out, STATUS_OK);
        }
    };
    let transform_class = metadata.host.read_usize(transform).unwrap_or(0);
    out.push(format!(
        "pos: transform = {transform:#x} ({})",
        metadata.class_full_name(transform_class)
    ));
    if transform < 0x10000 {
        return (out, STATUS_OK);
    }
    let Some(transform_api) = metadata
        .find("UnityEngine", "Transform", CLASS_MAX)
        .first()
        .map(|v| v.1)
    else {
        out.push("pos: Transform class not found".into());
        return (out, STATUS_NOT_READY);
    };
    let methods = metadata.methods(transform_api);
    for name in ["get_position", "get_localPosition"] {
        let entry = Metadata::method_entry(&methods, name);
        if entry == 0 {
            out.push(format!("pos: no Transform.{name}"));
            continue;
        }
        match call_vec3(metadata, entry, transform) {
            Some((x, y, z)) => out.push(format!("pos: {name} = ({x:.3}, {y:.3}, {z:.3})")),
            None => out.push(format!("pos: {name} faulted through host guard")),
        }
    }
    (out, STATUS_OK)
}

fn mul4(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0f32; 16];
    for column in 0..4 {
        for row in 0..4 {
            out[row + column * 4] = (0..4)
                .map(|index| a[row + index * 4] * b[index + column * 4])
                .sum();
        }
    }
    out
}

fn mul_vec4(matrix: &[f32; 16], vector: [f32; 4]) -> [f32; 4] {
    let mut out = [0f32; 4];
    for row in 0..4 {
        out[row] = matrix[row] * vector[0]
            + matrix[row + 4] * vector[1]
            + matrix[row + 8] * vector[2]
            + matrix[row + 12] * vector[3];
    }
    out
}

fn project(pv: &[f32; 16], width: f32, height: f32, point: Vec3) -> Option<(f32, f32)> {
    let clip = mul_vec4(pv, [point.0, point.1, point.2, 1.0]);
    if clip[3] <= 0.0001 {
        return None;
    }
    let ndc_x = clip[0] / clip[3];
    let ndc_y = clip[1] / clip[3];
    Some(((ndc_x * 0.5 + 0.5) * width, (0.5 - ndc_y * 0.5) * height))
}

fn esp(metadata: &mut Metadata<'_>, cache: &mut Cache) -> (Vec<String>, i32) {
    let (Some(camera), Some(sdk)) = (camera(metadata, cache), sdk(metadata, cache)) else {
        return (
            vec![format!(
                "esp: unresolved (camera {}, players {})",
                camera(metadata, cache).is_some(),
                sdk(metadata, cache).is_some()
            )],
            STATUS_NOT_READY,
        );
    };
    let mut out = Vec::new();
    let instance = match metadata.host.call(camera.get_main, &[]) {
        Ok(instance) if instance >= 0x10000 => instance,
        Ok(instance) => {
            out.push(format!("esp: no main camera ({instance:#x})"));
            return (out, STATUS_OK);
        }
        Err(status) => {
            out.push(format!("esp: Camera.main faulted: host status {status}"));
            return (out, STATUS_OK);
        }
    };
    let mut width = if camera.get_pixw != 0 {
        metadata
            .host
            .call(camera.get_pixw, &[instance])
            .unwrap_or(0) as u32
    } else {
        0
    };
    let mut height = if camera.get_pixh != 0 {
        metadata
            .host
            .call(camera.get_pixh, &[instance])
            .unwrap_or(0) as u32
    } else {
        0
    };
    if width == 0 || height == 0 {
        width = 1920;
        height = 1080;
    }
    let transform = metadata
        .host
        .call(camera.get_transform, &[instance])
        .unwrap_or(0);
    let camera_position = call_vec3(metadata, camera.tr_position, transform);
    let forward = call_vec3(metadata, camera.tr_forward, transform);
    let (Some(projection), Some(view)) = (
        call_matrix(metadata, camera.get_proj, instance),
        call_matrix(metadata, camera.get_view, instance),
    ) else {
        out.push("esp: projection/worldToCamera matrix faulted".into());
        return (out, STATUS_OK);
    };
    let pv = mul4(&projection, &view);
    let (float_width, float_height) = (width as f32, height as f32);
    let error = camera_position
        .and_then(|position| project(&pv, float_width, float_height, position))
        .map(|(x, y)| {
            (x - float_width / 2.0)
                .abs()
                .max((y - float_height / 2.0).abs())
        })
        .unwrap_or(-1.0);
    let players = match enumerate_players(metadata, &sdk, true) {
        Ok(players) => players.rows,
        Err(error) => {
            out.push(format!("esp: {error}"));
            return (out, STATUS_OK);
        }
    };
    out.push(format!(
        "esp: head w={width} h={height} err={error:.1} cam=({:.2}, {:.2}, {:.2}) fwd=({:.2}, {:.2}, {:.2})",
        camera_position.map_or(0.0, |p| p.0),
        camera_position.map_or(0.0, |p| p.1),
        camera_position.map_or(0.0, |p| p.2),
        forward.map_or(0.0, |p| p.0),
        forward.map_or(0.0, |p| p.1),
        forward.map_or(0.0, |p| p.2)
    ));
    for player in players {
        let Some(position) = player.pos else { continue };
        let Some((screen_x, screen_y)) = project(&pv, float_width, float_height, position) else {
            continue;
        };
        let head = (position.0, position.1 + 1.70, position.2);
        let Some((_, head_y)) = project(&pv, float_width, float_height, head) else {
            continue;
        };
        let distance = camera_position.map_or(-1.0, |camera| {
            let (dx, dy, dz) = (
                position.0 - camera.0,
                position.1 - camera.1,
                position.2 - camera.2,
            );
            (dx * dx + dy * dy + dz * dz).sqrt()
        });
        out.push(format!(
            "esp: id={} d={distance:.2} x={screen_x:.1} y={screen_y:.1} h={head_y:.1} w={:.2},{:.2},{:.2} name={}",
            player.id,
            position.0,
            position.1,
            position.2,
            player.name.unwrap_or_default()
        ));
    }
    (out, STATUS_OK)
}

pub fn dispatch(
    metadata: &mut Metadata<'_>,
    cache: &mut Cache,
    command: &str,
) -> (Vec<String>, i32) {
    match command {
        "pos" => pos(metadata),
        "who" => who(metadata, cache),
        "esp" => esp(metadata, cache),
        _ => unreachable!("only telemetry commands are routed here"),
    }
}

#[cfg(test)]
mod tests {
    use super::{mul_vec4, mul4, project};

    fn identity() -> [f32; 16] {
        let mut matrix = [0f32; 16];
        for index in 0..4 {
            matrix[index * 5] = 1.0;
        }
        matrix
    }

    fn perspective(fov_y_degrees: f32, aspect: f32) -> [f32; 16] {
        let focal = 1.0 / (fov_y_degrees.to_radians() / 2.0).tan();
        let mut matrix = [0f32; 16];
        matrix[0] = focal / aspect;
        matrix[5] = focal;
        matrix[10] = -1.0002;
        matrix[11] = -1.0;
        matrix[14] = -0.20002;
        matrix
    }

    #[test]
    fn projection_centres_view_rejects_behind_and_preserves_screen_sides() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        let centre = project(&pv, 1920.0, 1080.0, (0.0, 0.0, -5.0)).unwrap();
        assert!((centre.0 - 960.0).abs() < 0.5);
        assert!((centre.1 - 540.0).abs() < 0.5);
        assert!(project(&pv, 1920.0, 1080.0, (0.0, 0.0, 5.0)).is_none());
        assert!(project(&pv, 1920.0, 1080.0, (1.0, 0.0, -5.0)).unwrap().0 > 960.0);
    }

    #[test]
    fn column_major_matrix_multiplication_preserves_translation() {
        let mut matrix = identity();
        matrix[12] = 3.0;
        assert_eq!(mul_vec4(&matrix, [1.0, 0.0, 0.0, 1.0])[0], 4.0);
    }
}
