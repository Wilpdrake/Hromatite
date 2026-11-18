//! Host-side fake services for exercising the real exported C ABI.

use hromatite_abi::{HostApiV1, MAX_CALL_ARGS};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

pub type Behavior = Box<dyn Fn(&[u64]) -> (i32, u64)>;

#[derive(Default)]
struct Fake {
    bytes: BTreeMap<u64, u8>,
    mapped: Vec<(u64, u64)>,
    logs: Vec<(String, String)>,
    calls: Vec<(u64, Vec<u64>)>,
    behaviors: BTreeMap<u64, Behavior>,
}

impl Fake {
    fn read(&self, address: u64, out: &mut [u8]) -> usize {
        let mut read = 0;
        for (index, slot) in out.iter_mut().enumerate() {
            let Some(current) = address.checked_add(index as u64) else {
                break;
            };
            let mapped = self
                .mapped
                .iter()
                .any(|(base, len)| current >= *base && current < base.saturating_add(*len));
            match self.bytes.get(&current).copied() {
                Some(byte) => *slot = byte,
                None if mapped => *slot = 0,
                None => break,
            }
            read += 1;
        }
        read
    }
}

thread_local! {
    static FAKE: RefCell<Fake> = RefCell::new(Fake::default());
}

static SERIAL: Mutex<()> = Mutex::new(());

pub fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn reset() {
    FAKE.with(|fake| *fake.borrow_mut() = Fake::default());
}

pub fn map_zeroed(address: u64, len: u64) {
    FAKE.with(|fake| fake.borrow_mut().mapped.push((address, len)));
}

pub fn put_bytes(address: u64, bytes: &[u8]) {
    FAKE.with(|fake| {
        let mut fake = fake.borrow_mut();
        for (index, byte) in bytes.iter().enumerate() {
            fake.bytes.insert(address + index as u64, *byte);
        }
    });
}

pub fn put_u16(address: u64, value: u16) {
    put_bytes(address, &value.to_le_bytes());
}

pub fn put_u32(address: u64, value: u32) {
    put_bytes(address, &value.to_le_bytes());
}

pub fn put_u64(address: u64, value: u64) {
    put_bytes(address, &value.to_le_bytes());
}

pub fn put_ascii(address: u64, value: &str) {
    put_bytes(address, value.as_bytes());
    put_bytes(address + value.len() as u64, &[0]);
}

pub fn put_utf16(address: u64, value: &str) {
    let units: Vec<u16> = value.encode_utf16().collect();
    for (index, unit) in units.iter().enumerate() {
        put_u16(address + index as u64 * 2, *unit);
    }
    put_u16(address + units.len() as u64 * 2, 0);
}

pub fn bytes(address: u64, len: usize) -> Vec<u8> {
    FAKE.with(|fake| {
        let mut out = vec![0; len];
        let read = fake.borrow().read(address, &mut out);
        out.truncate(read);
        out
    })
}

pub fn returning(value: u64) -> Behavior {
    Box::new(move |_| (0, value))
}

pub fn fault(status: i32) -> Behavior {
    Box::new(move |_| (status, 0))
}

pub fn vector3(x: f32, y: f32, z: f32) -> Behavior {
    Box::new(move |args| {
        let words = [
            u64::from(x.to_bits()) | (u64::from(y.to_bits()) << 32),
            u64::from(z.to_bits()),
        ];
        if let Some(output) = args.first().copied() {
            // SAFETY: this fake native getter simulates writes into the real
            // two-qword Vector3 output buffer supplied by the feature.
            unsafe {
                let output = output as *mut u64;
                output.write(words[0]);
                output.add(1).write(words[1]);
            }
        }
        (0, 0)
    })
}

pub fn matrix4(values: [f32; 16]) -> Behavior {
    Box::new(move |args| {
        let mut words = [0u64; 8];
        for (index, value) in values.iter().enumerate() {
            words[index / 2] |= u64::from(value.to_bits()) << ((index % 2) * 32);
        }
        if let Some(output) = args.first().copied() {
            // SAFETY: this fake native getter simulates writes into the real
            // eight-qword Matrix4x4 output buffer supplied by the feature.
            unsafe {
                let output = output as *mut u64;
                for (index, word) in words.iter().enumerate() {
                    output.add(index).write(*word);
                }
            }
        }
        (0, 0)
    })
}

pub fn on_call(address: u64, behavior: Behavior) {
    FAKE.with(|fake| {
        fake.borrow_mut().behaviors.insert(address, behavior);
    });
}

pub fn logs() -> Vec<(String, String)> {
    FAKE.with(|fake| fake.borrow().logs.clone())
}

pub fn message_log() -> String {
    logs()
        .into_iter()
        .map(|(_, message)| message)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn calls() -> Vec<(u64, Vec<u64>)> {
    FAKE.with(|fake| fake.borrow().calls.clone())
}

extern "system" fn log(tag: *const i8, tag_len: usize, message: *const i8, message_len: usize) {
    // SAFETY: the feature ABI borrows both byte slices only for this callback.
    let (tag, message) = unsafe {
        (
            std::slice::from_raw_parts(tag.cast::<u8>(), tag_len),
            std::slice::from_raw_parts(message.cast::<u8>(), message_len),
        )
    };
    FAKE.with(|fake| {
        fake.borrow_mut().logs.push((
            String::from_utf8_lossy(tag).into_owned(),
            String::from_utf8_lossy(message).into_owned(),
        ));
    });
}

extern "system" fn read(address: u64, out: *mut u8, out_len: usize) -> usize {
    if out.is_null() {
        return 0;
    }
    // SAFETY: the caller supplies writable storage for exactly `out_len` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(out, out_len) };
    FAKE.with(|fake| fake.borrow().read(address, out))
}

extern "system" fn write(address: u64, data: *const u8, data_len: usize) -> usize {
    if data.is_null() {
        return 0;
    }
    // SAFETY: the caller supplies readable storage for exactly `data_len` bytes.
    let data = unsafe { std::slice::from_raw_parts(data, data_len) };
    FAKE.with(|fake| {
        let mut fake = fake.borrow_mut();
        let mut written = 0;
        for (index, byte) in data.iter().enumerate() {
            let current = address + index as u64;
            let mapped = fake
                .mapped
                .iter()
                .any(|(base, len)| current >= *base && current < base.saturating_add(*len));
            if !mapped && !fake.bytes.contains_key(&current) {
                break;
            }
            fake.bytes.insert(current, *byte);
            written += 1;
        }
        written
    })
}

extern "system" fn guarded_call(
    address: u64,
    args: *const u64,
    arg_count: usize,
    result: *mut u64,
) -> i32 {
    if arg_count > MAX_CALL_ARGS || (arg_count != 0 && args.is_null()) || result.is_null() {
        return -2;
    }
    // SAFETY: the ABI guarantees `arg_count` readable arguments and one output.
    let args = unsafe {
        if arg_count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(args, arg_count)
        }
    };
    let (status, value) = FAKE.with(|fake| {
        let mut fake = fake.borrow_mut();
        fake.calls.push((address, args.to_vec()));
        match fake.behaviors.get(&address) {
            Some(behavior) => behavior(args),
            None => (-100, 0),
        }
    });
    // SAFETY: checked non-null above and borrowed only for this callback.
    unsafe { *result = value };
    status
}

pub fn api() -> HostApiV1 {
    HostApiV1 {
        struct_size: std::mem::size_of::<HostApiV1>() as u32,
        abi_version: hromatite_abi::ABI_VERSION,
        game_assembly_base: 0x0000_0001_8000_0000,
        game_assembly_size: 0x0f44_0000,
        unity_player_base: 0x0000_0002_8000_0000,
        unity_player_size: 0x0215_3000,
        game_assembly_sha256: hromatite_abi::build::newest_sha_bytes(),
        log,
        read,
        write,
        guarded_call,
    }
}
