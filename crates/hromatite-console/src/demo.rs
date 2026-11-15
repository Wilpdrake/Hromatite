//! Clearly separated deterministic fixture; it is never read from a live path.

use crate::model::ConsoleModel;

pub const DEMO_LINES: &[&str] = &[
    "[1777777701] [dll] DllMain(PROCESS_ATTACH), inline (no extra thread)",
    "[1777777702] [con] console ready; file C:\\users\\steamuser\\hromatite-cmd.txt, 'help' for commands",
    "[1777777703] [late] LateUpdate configured: source=VRCPlayerApi, hits=128 tid=0x1a4",
    "[1777777704] [gct] 17 live thread(s) in this process, our tid 0x1a4",
    "[1777777705] [con] pos: Camera.main = 0x3027c1c0",
    "[1777777706] [con] pos: get_position = (12.500, 1.620, -4.250)  [raw 0x41480000 0x3fcf5c29]",
    "[1777777707] [con]   player #0: api 0xaa id=7 pos=(14.00, 1.10, -3.00) go=\"Nyx\"",
    "[1777777708] [con] esp: head w=1920 h=1080 err=0.3 cam=(12.50, 1.62, -4.25) fwd=(0.00, 0.00, 1.00)",
    "[1777777709] [con] esp: id=7 d=2.32 x=1034.0 y=580.0 h=305.0 w=14.00,1.10,-3.00 name=Nyx",
    "[1777777710] [late] hits=640 hz=90.0 tid=0x1a4",
    "[1777777711] [gct] gctfix: stale record id=0x91 unlinked",
    "[1777777712] [dll] guard[q]: simulated failed read caught, continuing",
    "legacy unframed compatibility line",
];

pub fn model() -> ConsoleModel {
    let mut model = ConsoleModel::new(true);
    model.ingest(DEMO_LINES);
    model.set_notice("DEMO fixture only — commands are not written anywhere");
    model
}

#[cfg(test)]
mod tests {
    #[test]
    fn demo_is_explicit_and_exercises_legacy_and_telemetry() {
        let model = super::model();
        assert!(model.demo());
        assert!(model.camera.is_some());
        assert!(model.player.as_deref().unwrap().contains("Nyx"));
        assert!(model.lines().any(|line| line.tag == "raw"));
    }
}
