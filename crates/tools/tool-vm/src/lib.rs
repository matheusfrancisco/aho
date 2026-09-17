//! The 13 `vm_*` tools (00, 07) over the session's VM lab (`aho-vm::VmLab`, `vm-qemu`): the
//! agent creates machines from operator recipes inside its own network, starts and stops them,
//! talks to their serial console, and drives their screen. Machines are named, never paths;
//! recipes and media are the operator's (`aho add recipe`, `aho media import`). Every call is
//! checked against the session's lease by the lab before any effect (`RevocationEffective`).
//!
//! Classes: looking (list, status, console read, screenshot, pointer) is `ReadOnly`; everything
//! that changes a machine is `ControlsVM`, which presets below `lab` ask for or deny.
#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::*;
use aho_display::{CaptureRequest, input};
use aho_types::*;
use aho_vm::{VmControl, VmError, VmLab};
use serde_json::{Value, json};

pub struct VmToolDeps {
    pub lab: VmControl,
}
impl Deps for VmToolDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::VmControl];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(VmToolDeps { lab: r.handle()? })
    }
}

#[derive(Clone)]
pub struct ToolVm {
    lab: Arc<dyn VmLab>,
}

impl Capability for ToolVm {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = VmToolDeps;
    type Config = ();
    fn build(deps: VmToolDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolVm { lab: deps.lab.0 })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let lab = self.lab.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let lab = lab.clone();
                let args = req.view.args.clone();
                Box::pin(async move {
                    match tokio::task::spawn_blocking(move || run(&*lab, name, &args)).await {
                        Ok(outcome) => outcome,
                        Err(e) => ExecOutcome::err("VM_IO", format!("the VM call did not complete: {e}")),
                    }
                })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolVm,
    "tool-vm",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_vm::SEAM)],
    tools = TOOLS,
    doc = "vm_list vm_status vm_create vm_disk_create vm_start vm_stop vm_console_read vm_console_write vm_console_send vm_screenshot vm_pointer vm_mouse vm_keyboard: machines from operator recipes on the session network (`<name>.aho`), under the session's lease. Looking is ReadOnly; changing a machine is ControlsVM."
);

fn obj(props: Value, required: &[&str]) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": props, "required": required}))
}
fn name_prop() -> Value {
    json!({"type": "string", "description": "The machine's name (a DNS label; peers reach it as <name>.aho)."})
}
fn schema_none(_: &BoundWorld) -> RootSchema {
    obj(json!({}), &[])
}
fn schema_name(_: &BoundWorld) -> RootSchema {
    obj(json!({"name": name_prop()}), &["name"])
}
fn schema_create(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "recipe": {"type": "string", "description": "An operator recipe, as `vm_list` shows them."}}),
        &["name", "recipe"],
    )
}
fn schema_disk(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "disk": {"type": "string"}, "size": {"type": "string", "description": "Bytes, or with a unit: 512M, 8G."}}),
        &["name", "disk", "size"],
    )
}
fn schema_stop(_: &BoundWorld) -> RootSchema {
    obj(json!({"name": name_prop(), "force": {"type": "boolean", "description": "Terminate instead of an ACPI power-down."}}), &["name"])
}
fn schema_console_read(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "cursor": {"type": "integer", "description": "From the previous read's `cursor`; 0 = the whole scrollback."}, "max_bytes": {"type": "integer"}}),
        &["name"],
    )
}
fn schema_console_write(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "text": {"type": "string", "description": "Sent verbatim to the serial console (end lines with \\n)."}}),
        &["name", "text"],
    )
}
fn schema_console_send(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "keys": {"type": "array", "items": {"type": "string"}, "description": "Enter, Tab, Esc, Up, C-c, C-d, F1–F12, …"}}),
        &["name", "keys"],
    )
}
fn schema_screenshot(_: &BoundWorld) -> RootSchema {
    obj(json!({"name": name_prop(), "max_width": {"type": "integer"}, "max_height": {"type": "integer"}}), &["name"])
}
fn schema_mouse(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "x": {"type": "integer"}, "y": {"type": "integer"},
               "click": {"type": "string", "enum": ["left", "middle", "right", "wheel_up", "wheel_down"]},
               "count": {"type": "integer", "description": "1 = click, 2 = double click."}}),
        &["name", "x", "y"],
    )
}
fn schema_keyboard(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"name": name_prop(), "text": {"type": "string", "description": "Typed as-is."},
               "keys": {"type": "array", "items": {"type": "string"}, "description": "Chords after the text: Enter, ctrl+c, alt+F4, …"}}),
        &["name"],
    )
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "vm_list",
        "List the recipes you may create machines from and your machines (name, state, recipe, disks).",
        Consequence::ReadOnly,
    )
    .schema(schema_none)
    .safe(),
    ToolDecl::new("vm_status", "One machine's state (Running, Stopped, Paused), recipe, disks and CPU time used.", Consequence::ReadOnly)
        .schema(schema_name)
        .safe(),
    ToolDecl::new(
        "vm_create",
        "Create a stopped machine from an operator recipe; it joins your network as <name>.aho. Counts against the lease.",
        Consequence::ControlsVM,
    )
    .schema(schema_create),
    ToolDecl::new(
        "vm_disk_create",
        "Add a persistent disk to a stopped machine. Counts against the lease's disk quota.",
        Consequence::ControlsVM,
    )
    .schema(schema_disk),
    ToolDecl::new("vm_start", "Boot a machine. Watch it with vm_console_read.", Consequence::ControlsVM).schema(schema_name),
    ToolDecl::new("vm_stop", "Power a machine down (ACPI), or terminate it with force=true.", Consequence::ControlsVM).schema(schema_stop),
    ToolDecl::new(
        "vm_console_read",
        "Read a machine's serial console from `cursor`; returns the text and the next cursor.",
        Consequence::ReadOnly,
    )
    .schema(schema_console_read)
    .safe(),
    ToolDecl::new("vm_console_write", "Type text on a machine's serial console.", Consequence::ControlsVM).schema(schema_console_write),
    ToolDecl::new("vm_console_send", "Press keys on a machine's serial console (Enter, C-c, Up, …).", Consequence::ControlsVM)
        .schema(schema_console_send),
    ToolDecl::new(
        "vm_screenshot",
        "Capture a machine's screen as a PNG (optionally scaled down); coordinates stay native pixels.",
        Consequence::ReadOnly,
    )
    .schema(schema_screenshot)
    .safe(),
    ToolDecl::new("vm_pointer", "Where the pointer is on a machine's screen, and the screen size.", Consequence::ReadOnly)
        .schema(schema_name)
        .safe(),
    ToolDecl::new("vm_mouse", "Move the pointer on a machine's screen to (x, y), optionally clicking there.", Consequence::ControlsVM)
        .schema(schema_mouse),
    ToolDecl::new(
        "vm_keyboard",
        "Type text and press key chords on a machine's screen (graphical keyboard, not the serial console).",
        Consequence::ControlsVM,
    )
    .schema(schema_keyboard),
];

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ExecOutcome> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| ExecOutcome::err("TOOL_BAD_ARGS", format!("`{key}` (a string) is required")))
}

fn vm(e: VmError) -> ExecOutcome {
    ExecOutcome::err(e.code(), e.to_string())
}

fn done(value: Value, text: String) -> ExecOutcome {
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
}

/// `512M`, `8G`, `1048576`: bytes.
pub fn size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits.parse().ok()?;
    let mul: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        _ => return None,
    };
    n.checked_mul(mul)
}

fn machine_line(m: &aho_vm::Machine) -> String {
    let disks: u64 = m.disks.iter().map(|d| d.bytes).sum();
    format!(
        "{} {:?} recipe {} · {} disk(s), {} MiB · cpu {} s used",
        m.name,
        m.state,
        m.recipe.name,
        m.disks.len(),
        disks >> 20,
        m.cpu_budget_used_ms / 1000
    )
}

/// One tool call against the lab (blocking: QMP, the console and RFB are bounded socket I/O).
pub fn run(lab: &dyn VmLab, tool: &str, args: &Value) -> ExecOutcome {
    match run_inner(lab, tool, args) {
        Ok(o) | Err(o) => o,
    }
}

fn run_inner(lab: &dyn VmLab, tool: &str, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
    let name = || text_arg(args, "name");
    Ok(match tool {
        "vm_list" => {
            let recipes = lab.recipes().map_err(vm)?;
            let machines = lab.list().map_err(vm)?;
            let mut text = format!(
                "recipes: {}\n",
                if recipes.is_empty() {
                    "none (the operator adds them with `aho add recipe`)".into()
                } else {
                    recipes.iter().map(|r| r.name.clone()).collect::<Vec<_>>().join(", ")
                }
            );
            text += &if machines.is_empty() {
                "machines: none".into()
            } else {
                machines.iter().map(machine_line).collect::<Vec<_>>().join("\n")
            };
            done(json!({"recipes": recipes, "machines": machines}), text)
        }
        "vm_status" => {
            let m = lab.status(name()?).map_err(vm)?;
            done(json!(m), machine_line(&m))
        }
        "vm_create" => {
            let m = lab.create(name()?, text_arg(args, "recipe")?).map_err(vm)?;
            done(json!(m), format!("created {} (stopped); start it with vm_start. It is {}.aho on your network.", m.name, m.name))
        }
        "vm_disk_create" => {
            let bytes = size(text_arg(args, "size")?)
                .ok_or_else(|| ExecOutcome::err("TOOL_BAD_ARGS", "`size` is bytes or a number with K, M, G or T"))?;
            let d = lab.disk_create(name()?, text_arg(args, "disk")?, bytes).map_err(vm)?;
            done(json!(d), format!("added disk {} ({} MiB) to {}", d.name, d.bytes >> 20, name()?))
        }
        "vm_start" => {
            let m = lab.start(name()?).map_err(vm)?;
            done(json!(m), format!("{} is {:?}; vm_console_read shows its boot", m.name, m.state))
        }
        "vm_stop" => {
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let m = lab.stop(name()?, force).map_err(vm)?;
            done(json!(m), format!("{} is {:?}", m.name, m.state))
        }
        "vm_console_read" => {
            let cursor = args.get("cursor").and_then(Value::as_u64).unwrap_or(0);
            let max = args.get("max_bytes").and_then(Value::as_u64).unwrap_or(64 << 10) as usize;
            let c = lab.console_read(name()?, cursor, max).map_err(vm)?;
            let gap =
                if c.dropped > 0 { format!("[… {} earlier bytes dropped from the scrollback …]\n", c.dropped) } else { String::new() };
            done(json!(c), format!("{gap}{}\n[cursor {}]", c.text, c.cursor))
        }
        "vm_console_write" => {
            let text = text_arg(args, "text")?;
            lab.console_write(name()?, text.as_bytes()).map_err(vm)?;
            done(json!({"written": text.len()}), format!("sent {} bytes to {}'s console", text.len(), name()?))
        }
        "vm_console_send" => {
            let keys: Vec<String> = args
                .get("keys")
                .and_then(|k| serde_json::from_value(k.clone()).ok())
                .ok_or_else(|| ExecOutcome::err("TOOL_BAD_ARGS", "`keys` is a list of key names"))?;
            let bytes = tool_terminal::keys::encode(&keys, false).map_err(|e| ExecOutcome::err("TOOL_BAD_ARGS", e))?;
            lab.console_write(name()?, &bytes).map_err(vm)?;
            done(json!({"keys": keys}), format!("pressed {} on {}'s console", keys.join(" "), name()?))
        }
        "vm_screenshot" => {
            let desk = lab.desktop(name()?).map_err(vm)?;
            let req = CaptureRequest {
                max_width: args.get("max_width").and_then(Value::as_u64).map(|v| v as u32),
                max_height: args.get("max_height").and_then(Value::as_u64).map(|v| v as u32),
            };
            let shot = desk.screenshot(&req).map_err(display)?;
            let note = format!(
                "{}×{} (screen {}×{}; coordinates are screen pixels)",
                shot.width, shot.height, shot.source_width, shot.source_height
            );
            ExecOutcome::Done {
                value: json!({"width": shot.width, "height": shot.height, "source_width": shot.source_width, "source_height": shot.source_height}),
                content: vec![ContentBlock::Image { media_type: "image/png".into(), data: shot.png }, ContentBlock::text(note)],
                meta: None,
            }
        }
        "vm_pointer" => {
            let st = lab.desktop(name()?).map_err(vm)?.state().map_err(display)?;
            done(
                json!(st),
                format!(
                    "pointer at ({}, {}) on a {}×{} screen{}",
                    st.x,
                    st.y,
                    st.width,
                    st.height,
                    if st.position_known { "" } else { " (position not yet known)" }
                ),
            )
        }
        "vm_mouse" => {
            let (x, y) = coords(args)?;
            let click = args.get("click").and_then(Value::as_str);
            let count = args.get("count").and_then(Value::as_u64).unwrap_or(1) as u8;
            let desk = lab.desktop(name()?).map_err(vm)?;
            input::mouse(&*desk, x, y, click, count).map_err(display)?;
            done(
                json!({"x": x, "y": y, "click": click}),
                format!("pointer to ({x}, {y}){}", click.map(|c| format!(", {c} click")).unwrap_or_default()),
            )
        }
        "vm_keyboard" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let keys: Vec<String> = args.get("keys").and_then(|k| serde_json::from_value(k.clone()).ok()).unwrap_or_default();
            let events = input::key_events(text, &keys).map_err(display)?;
            let desk = lab.desktop(name()?).map_err(vm)?;
            input::type_keys(&*desk, &events).map_err(display)?;
            done(json!({"events": events.len()}), format!("sent {} key events to {}", events.len(), name()?))
        }
        other => ExecOutcome::err("UNKNOWN_TOOL", format!("tool-vm has no tool `{other}`")),
    })
}

fn coords(args: &Value) -> Result<(u16, u16), ExecOutcome> {
    let c = |k: &str| args.get(k).and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok());
    match (c("x"), c("y")) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(ExecOutcome::err("TOOL_BAD_ARGS", "`x` and `y` are screen pixels (0..65535)")),
    }
}

fn display(e: aho_display::DisplayError) -> ExecOutcome {
    ExecOutcome::err(e.code(), e.to_string())
}

#[cfg(test)]
mod tests;
