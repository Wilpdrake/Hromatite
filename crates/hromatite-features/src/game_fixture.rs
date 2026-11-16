//! Sparse Unity 6000 runtime fixture used by ABI-level command tests.

use crate::test_support as fake;

pub const CLASS_TABLE_RVA: u64 = 0x0e66_c870;
pub const IMAGE_GET_CLASS_RVA: u64 = 0x00c2_a330;
pub const CORLIB_GETTER_RVA: u64 = 0x00c2_7f00;
pub const CLASS_TABLE: u64 = 0x0000_0003_0000_0000;
pub const CORLIB_IMAGE: u64 = 0x0000_0005_0002_0000;
pub const CAMERA_CLASS: u64 = 0x0000_0004_0001_0000;
pub const COMPONENT_CLASS: u64 = 0x0000_0004_0002_0000;
pub const TRANSFORM_CLASS: u64 = 0x0000_0004_0003_0000;
pub const PLAYER_API_CLASS: u64 = 0x0000_0004_0004_0000;
pub const OBJECT_CLASS: u64 = 0x0000_0004_0005_0000;

pub const GET_MAIN: u64 = 0x0000_0000_6000_1000;
pub const GET_PROJECTION: u64 = 0x0000_0000_6000_2000;
pub const GET_VIEW: u64 = 0x0000_0000_6000_3000;
pub const GET_PIXEL_WIDTH: u64 = 0x0000_0000_6000_4000;
pub const GET_PIXEL_HEIGHT: u64 = 0x0000_0000_6000_5000;
pub const GET_TRANSFORM: u64 = 0x0000_0000_6000_6000;
pub const GET_POSITION: u64 = 0x0000_0000_6000_7000;
pub const GET_LOCAL_POSITION: u64 = 0x0000_0000_6000_8000;
pub const GET_FORWARD: u64 = 0x0000_0000_6000_9000;
pub const GET_ALL_PLAYERS: u64 = 0x0000_0000_6000_a000;
pub const GET_PLAYER_ID: u64 = 0x0000_0000_6000_b000;
pub const GET_PLAYER_POSITION: u64 = 0x0000_0000_6000_c000;
pub const GET_PLAYER_OBJECTS: u64 = 0x0000_0000_6000_d000;
pub const GET_OBJECT_NAME: u64 = 0x0000_0000_6000_e000;

fn image(address: u64, name: &str, name_address: u64) {
    fake::map_zeroed(address, 0x48);
    fake::put_ascii(name_address, name);
    fake::put_u64(address + 0x08, name_address);
}

fn class(index: usize, address: u64, namespace: &str, name: &str, methods: &[(&str, u64)]) {
    fake::put_u64(CLASS_TABLE + index as u64 * 8, address);
    fake::map_zeroed(address, 0x200);
    fake::put_u64(address, CORLIB_IMAGE);
    fake::put_ascii(address + 0x200, namespace);
    fake::put_ascii(address + 0x300, name);
    fake::put_u64(address + 0x18, address + 0x200);
    fake::put_u64(address + 0x98, address + 0x300);

    let array = address + 0x800;
    let infos = address + 0x1000;
    fake::map_zeroed(array, (methods.len() as u64 + 1) * 8);
    fake::map_zeroed(infos, methods.len() as u64 * 0x100);
    fake::put_u64(address + 0x50, array);
    for (method_index, (method_name, entry)) in methods.iter().enumerate() {
        let info = infos + method_index as u64 * 0x100;
        fake::put_u64(array + method_index as u64 * 8, info);
        fake::put_u64(info, *entry);
        fake::put_ascii(info + 0x80, method_name);
        fake::put_u64(info + 0x18, info + 0x80);
        fake::put_u64(info + 0x20, 0x0000_0004_2000_0000);
        fake::put_u64(info + 0x28, address);
        fake::put_u16(
            info + 0x4c,
            if *method_name == "get_main" { 0x10 } else { 0 },
        );
        fake::put_bytes(info + 0x52, &[0]);
    }
}

pub fn install() {
    let ga = fake::api().game_assembly_base;
    fake::map_zeroed(ga + CLASS_TABLE_RVA, 8);
    fake::map_zeroed(CLASS_TABLE, 120_000 * 8);
    fake::put_u64(ga + CLASS_TABLE_RVA, CLASS_TABLE);
    fake::map_zeroed(0x0000_0004_2000_0000, 0x10);
    fake::put_u32(0x0000_0004_2000_0008, 0x8001_0000); // IL2CPP_TYPE_VOID

    image(
        CORLIB_IMAGE,
        "UnityEngine.CoreModule",
        0x0000_0005_1000_0000,
    );
    image(
        CORLIB_IMAGE - 4 * 0x48,
        "Assembly-CSharp",
        0x0000_0005_1000_0100,
    );
    image(CORLIB_IMAGE - 3 * 0x48, "VRCSDKBase", 0x0000_0005_1000_0200);

    class(
        167,
        CAMERA_CLASS,
        "UnityEngine",
        "Camera",
        &[
            ("get_main", GET_MAIN),
            ("get_projectionMatrix", GET_PROJECTION),
            ("get_worldToCameraMatrix", GET_VIEW),
            ("get_pixelWidth", GET_PIXEL_WIDTH),
            ("get_pixelHeight", GET_PIXEL_HEIGHT),
        ],
    );
    class(
        168,
        COMPONENT_CLASS,
        "UnityEngine",
        "Component",
        &[("get_transform", GET_TRANSFORM)],
    );
    class(
        169,
        TRANSFORM_CLASS,
        "UnityEngine",
        "Transform",
        &[
            ("get_position", GET_POSITION),
            ("get_localPosition", GET_LOCAL_POSITION),
            ("get_forward", GET_FORWARD),
        ],
    );
    class(
        170,
        PLAYER_API_CLASS,
        "VRC.SDKBase",
        "VRCPlayerApi",
        &[
            ("get_AllPlayers", GET_ALL_PLAYERS),
            ("GetPlayerId", GET_PLAYER_ID),
            ("GetPosition", GET_PLAYER_POSITION),
            ("GetPlayerObjects", GET_PLAYER_OBJECTS),
        ],
    );
    class(
        171,
        OBJECT_CLASS,
        "UnityEngine",
        "Object",
        &[("get_name", GET_OBJECT_NAME)],
    );
    fake::put_u64(CAMERA_CLASS + 0x10, COMPONENT_CLASS);
    fake::put_u64(TRANSFORM_CLASS + 0x10, COMPONENT_CLASS);

    // One complete measured FieldInfo record at class+0x80, count at +0x124.
    let fields = CAMERA_CLASS + 0x2000;
    fake::map_zeroed(fields, 0x28);
    fake::put_u64(CAMERA_CLASS + 0x80, fields);
    fake::put_u16(CAMERA_CLASS + 0x124, 1);
    fake::put_u64(fields, 0x0000_0004_2000_1000);
    fake::put_u32(fields + 0x08, 0x40);
    fake::put_ascii(CAMERA_CLASS + 0x2100, "m_FieldOfView");
    fake::put_u64(fields + 0x10, CAMERA_CLASS + 0x2100);
    fake::put_u32(fields + 0x18, 0x0400_002a);
    fake::put_u64(fields + 0x20, CAMERA_CLASS);

    fake::on_call(ga + CORLIB_GETTER_RVA, fake::returning(CORLIB_IMAGE));
    fake::on_call(
        ga + IMAGE_GET_CLASS_RVA,
        Box::new(|args| {
            let result = match args.get(1).copied() {
                Some(0) => CAMERA_CLASS,
                Some(1) => TRANSFORM_CLASS,
                _ => 0,
            };
            (0, result)
        }),
    );
}

pub const CAMERA_INSTANCE: u64 = 0x0000_0007_0001_0000;
pub const TRANSFORM_INSTANCE: u64 = 0x0000_0007_0002_0000;
const PLAYER_LIST: u64 = 0x0000_0007_1000_0000;
const PLAYER_ITEMS: u64 = 0x0000_0007_1000_1000;
const PLAYER_ONE: u64 = 0x0000_0007_1000_2000;
const PLAYER_TWO: u64 = 0x0000_0007_1000_3000;

fn player_objects(player: u64) -> u64 {
    if player == PLAYER_ONE {
        PLAYER_ONE + 0x4000
    } else {
        PLAYER_TWO + 0x4000
    }
}

fn il2cpp_string(address: u64, value: &str) {
    let units: Vec<u16> = value.encode_utf16().collect();
    fake::put_u32(address + 0x10, units.len() as u32);
    fake::put_utf16(address + 0x14, value);
}

pub fn install_telemetry() {
    fake::map_zeroed(CAMERA_INSTANCE, 8);
    fake::put_u64(CAMERA_INSTANCE, CAMERA_CLASS);
    fake::map_zeroed(TRANSFORM_INSTANCE, 8);
    fake::put_u64(TRANSFORM_INSTANCE, TRANSFORM_CLASS);
    fake::on_call(GET_MAIN, fake::returning(CAMERA_INSTANCE));
    fake::on_call(
        GET_TRANSFORM,
        Box::new(|args| {
            (
                0,
                if args.first().copied() == Some(CAMERA_INSTANCE) {
                    TRANSFORM_INSTANCE
                } else {
                    0
                },
            )
        }),
    );
    fake::on_call(
        GET_POSITION,
        Box::new(|args| {
            if args.get(1).copied() == Some(TRANSFORM_INSTANCE) {
                fake::vector3(0.0, 0.0, 0.0)(args)
            } else {
                (-100, 0)
            }
        }),
    );
    fake::on_call(GET_LOCAL_POSITION, fake::vector3(1.0, 2.0, 3.0));
    fake::on_call(GET_FORWARD, fake::vector3(0.0, 0.0, 1.0));
    fake::on_call(GET_PIXEL_WIDTH, fake::returning(1920));
    fake::on_call(GET_PIXEL_HEIGHT, fake::returning(1080));
    let mut projection = [0f32; 16];
    for index in 0..4 {
        projection[index * 5] = 1.0;
    }
    // Orthographic projection with horizontal half-scale: world x=1 projects
    // to NDC x=0.5, or 1440 px on the 1920 px fixture viewport.
    projection[0] = 0.5;
    let mut identity = [0f32; 16];
    for index in 0..4 {
        identity[index * 5] = 1.0;
    }
    fake::on_call(GET_PROJECTION, fake::matrix4(projection));
    fake::on_call(GET_VIEW, fake::matrix4(identity));

    fake::map_zeroed(PLAYER_LIST, 0x20);
    fake::put_u64(PLAYER_LIST + 0x10, PLAYER_ITEMS);
    fake::put_u32(PLAYER_LIST + 0x18, 2);
    fake::map_zeroed(PLAYER_ITEMS, 0x30);
    fake::put_u64(PLAYER_ITEMS + 0x18, 2);
    fake::put_u64(PLAYER_ITEMS + 0x20, PLAYER_ONE);
    fake::put_u64(PLAYER_ITEMS + 0x28, PLAYER_TWO);
    fake::on_call(GET_ALL_PLAYERS, fake::returning(PLAYER_LIST));
    fake::on_call(
        GET_PLAYER_ID,
        Box::new(|args| {
            (
                0,
                if args.first().copied() == Some(PLAYER_ONE) {
                    11
                } else {
                    12
                },
            )
        }),
    );
    fake::on_call(
        GET_PLAYER_POSITION,
        Box::new(|args| match args.get(1).copied() {
            Some(PLAYER_ONE) => fake::vector3(0.0, 0.0, 0.0)(args),
            Some(PLAYER_TWO) => fake::vector3(1.0, 0.0, 0.0)(args),
            _ => (-100, 0),
        }),
    );
    fake::on_call(
        GET_PLAYER_OBJECTS,
        Box::new(|args| (0, args.first().copied().map_or(0, player_objects))),
    );
    for (player, name) in [(PLAYER_ONE, "AvatarOne"), (PLAYER_TWO, "AvatarTwo")] {
        let array = player_objects(player);
        fake::map_zeroed(array, 0x28);
        fake::put_u64(array + 0x18, 1);
        fake::put_u64(array + 0x20, player + 0x8000);
        il2cpp_string(player + 0x8000, name);
    }
    fake::on_call(
        GET_OBJECT_NAME,
        Box::new(|args| (0, args.first().copied().unwrap_or(0))),
    );
}
