//! Stand-in for VRChat.exe: a long-lived wine process that the manual mapper
//! can be tested against without launching the game or touching EAC.

#![forbid(unsafe_code)]

fn main() {
    println!("test-target pid {}", std::process::id());
    let mut n: u64 = 0;
    loop {
        n += 1;
        if n.is_multiple_of(10) {
            println!("tick {n}");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
