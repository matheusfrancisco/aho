#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use aho_display::{Desktop, DisplayError, KeyEvent, PointerEvent, PointerState, Screenshot};
use aho_vm::{ConsoleChunk, Disk, Machine, Recipe};
use std::sync::Mutex;

#[derive(Default)]
struct Lab {
    revoked: bool,
    console: Mutex<Vec<u8>>,
    screen: Arc<Screen>,
}
#[derive(Default)]
struct Screen {
    keys: Mutex<Vec<KeyEvent>>,
    pointer: Mutex<Vec<PointerEvent>>,
}
impl Desktop for Screen {
    fn screenshot(&self, _: &CaptureRequest) -> Result<Screenshot, DisplayError> {
        Ok(Screenshot { width: 2, height: 1, source_width: 4, source_height: 2, png: vec![0x89, b'P', b'N', b'G'] })
    }
    fn key(&self, e: KeyEvent) -> Result<(), DisplayError> {
        self.keys.lock().unwrap().push(e);
        Ok(())
    }
    fn pointer(&self, e: PointerEvent) -> Result<(), DisplayError> {
        self.pointer.lock().unwrap().push(e);
        Ok(())
    }
    fn state(&self) -> Result<PointerState, DisplayError> {
        Ok(PointerState { width: 4, height: 2, x: 0, y: 0, buttons: 0, position_known: false })
    }
}
impl Lab {
    fn gate(&self) -> Result<(), VmError> {
        if self.revoked { Err(VmError::RevocationEffective { lease: "7".into(), reason: "expired".into() }) } else { Ok(()) }
    }
}
impl VmLab for Lab {
    fn check_lease(&self) -> Result<(), VmError> {
        self.gate()
    }
    fn recipes(&self) -> Result<Vec<Recipe>, VmError> {
        self.gate().map(|()| vec![])
    }
    fn list(&self) -> Result<Vec<Machine>, VmError> {
        self.gate().map(|()| vec![])
    }
    fn status(&self, n: &str) -> Result<Machine, VmError> {
        self.gate()?;
        Err(VmError::NotFound(n.into()))
    }
    fn create(&self, n: &str, _: &str) -> Result<Machine, VmError> {
        self.gate()?;
        Err(VmError::NotFound(n.into()))
    }
    fn disk_create(&self, n: &str, _: &str, _: u64) -> Result<Disk, VmError> {
        self.gate()?;
        Err(VmError::NotFound(n.into()))
    }
    fn start(&self, n: &str) -> Result<Machine, VmError> {
        self.gate()?;
        Err(VmError::NotFound(n.into()))
    }
    fn stop(&self, n: &str, _: bool) -> Result<Machine, VmError> {
        self.gate()?;
        Err(VmError::NotFound(n.into()))
    }
    fn console_read(&self, _: &str, cursor: u64, _: usize) -> Result<ConsoleChunk, VmError> {
        self.gate()?;
        Ok(ConsoleChunk { text: String::from_utf8_lossy(&self.console.lock().unwrap()).into(), cursor, dropped: 0 })
    }
    fn console_write(&self, _: &str, bytes: &[u8]) -> Result<(), VmError> {
        self.gate()?;
        self.console.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn desktop(&self, _: &str) -> Result<Arc<dyn Desktop>, VmError> {
        self.gate()?;
        Ok(self.screen.clone())
    }
}

#[test]
fn console_keys_become_control_bytes_and_screen_keys_become_keysym_chords() {
    let lab = Lab::default();
    assert!(matches!(run(&lab, "vm_console_write", &json!({"name": "vm-db", "text": "ls\n"})), ExecOutcome::Done { .. }));
    assert!(matches!(run(&lab, "vm_console_send", &json!({"name": "vm-db", "keys": ["C-c", "Enter"]})), ExecOutcome::Done { .. }));
    assert_eq!(*lab.console.lock().unwrap(), b"ls\n\x03\r");

    assert!(matches!(run(&lab, "vm_keyboard", &json!({"name": "vm-db", "text": "a", "keys": ["ctrl+l"]})), ExecOutcome::Done { .. }));
    let keys: Vec<(u32, bool)> = lab.screen.keys.lock().unwrap().iter().map(|k| (k.keysym, k.down)).collect();
    assert_eq!(keys, vec![(0x61, true), (0x61, false), (0xffe3, true), (0x6c, true), (0x6c, false), (0xffe3, false)]);

    assert!(matches!(
        run(&lab, "vm_mouse", &json!({"name": "vm-db", "x": 3, "y": 1, "click": "left", "count": 2})),
        ExecOutcome::Done { .. }
    ));
    let masks: Vec<u8> = lab.screen.pointer.lock().unwrap().iter().map(|p| p.buttons).collect();
    assert_eq!(masks, vec![0, 1, 0, 1, 0], "move, then two press/release pairs at the same spot");

    match run(&lab, "vm_screenshot", &json!({"name": "vm-db", "max_width": 2})) {
        ExecOutcome::Done { content, .. } => {
            assert!(matches!(&content[0], ContentBlock::Image { media_type, .. } if media_type == "image/png"))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_dead_lease_refuses_every_tool_with_its_proof_code_and_nothing_reaches_the_machine() {
    let lab = Lab { revoked: true, ..Lab::default() };
    for (tool, args) in [
        ("vm_list", json!({})),
        ("vm_console_write", json!({"name": "vm-db", "text": "x"})),
        ("vm_keyboard", json!({"name": "vm-db", "text": "x"})),
        ("vm_start", json!({"name": "vm-db"})),
    ] {
        match run(&lab, tool, &args) {
            ExecOutcome::Err { code, reason } => {
                assert_eq!(code, "RevocationEffective", "{tool}");
                assert!(reason.contains("no VM effect was performed"), "{tool}: {reason}");
            }
            other => panic!("{tool}: {other:?}"),
        }
    }
    assert!(lab.console.lock().unwrap().is_empty() && lab.screen.keys.lock().unwrap().is_empty());
}

#[test]
fn sizes_take_binary_units_and_refuse_garbage() {
    assert_eq!(size("512M"), Some(512 << 20));
    assert_eq!(size("8G"), Some(8 << 30));
    assert_eq!(size("1048576"), Some(1 << 20));
    for bad in ["", "G", "8X", "-1", "99999999999999999999T"] {
        assert_eq!(size(bad), None, "{bad}");
    }
}
