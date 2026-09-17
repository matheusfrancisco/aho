//! The session desktop as tools (00, 07): `screen_capture`, `pointer`, `mouse`, `keyboard` over
//! the bound world's `Desktop` (the sandbox VM's screen under `world = vm`). Coordinates are the
//! screen's own pixels even when a capture is scaled down. Looking is `ReadOnly`; moving the
//! pointer or typing is `ControlsDesktop`, which presets below `lab` ask for or deny.
#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::*;
use aho_display::{CaptureRequest, Desktop, DesktopHandle, DisplayError, input};
use aho_types::*;
use serde_json::{Value, json};

pub struct DisplayDeps {
    pub desktop: DesktopHandle,
}
impl Deps for DisplayDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Desktop];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(DisplayDeps { desktop: r.handle()? })
    }
}

#[derive(Clone)]
pub struct ToolDisplay {
    desktop: Arc<dyn Desktop>,
}

impl Capability for ToolDisplay {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = DisplayDeps;
    type Config = ();
    fn build(deps: DisplayDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolDisplay { desktop: deps.desktop.0 })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let desktop = self.desktop.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let desktop = desktop.clone();
                let args = req.view.args.clone();
                Box::pin(async move {
                    match tokio::task::spawn_blocking(move || run(&*desktop, name, &args)).await {
                        Ok(outcome) => outcome,
                        Err(e) => ExecOutcome::err("DISPLAY_IO", format!("the desktop call did not complete: {e}")),
                    }
                })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolDisplay,
    "tool-display",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_display::SEAM)],
    tools = TOOLS,
    doc = "screen_capture / pointer / mouse / keyboard on the session desktop (the sandbox VM's screen under world = vm). Coordinates are native screen pixels. Looking is ReadOnly; mouse and keyboard are ControlsDesktop."
);

fn obj(props: Value, required: &[&str]) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": props, "required": required}))
}
fn schema_capture(_: &BoundWorld) -> RootSchema {
    obj(json!({"max_width": {"type": "integer"}, "max_height": {"type": "integer"}}), &[])
}
fn schema_none(_: &BoundWorld) -> RootSchema {
    obj(json!({}), &[])
}
fn schema_mouse(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"x": {"type": "integer"}, "y": {"type": "integer"},
               "click": {"type": "string", "enum": ["left", "middle", "right", "wheel_up", "wheel_down"]},
               "count": {"type": "integer", "description": "1 = click, 2 = double click."}}),
        &["x", "y"],
    )
}
fn schema_keyboard(_: &BoundWorld) -> RootSchema {
    obj(
        json!({"text": {"type": "string", "description": "Typed as-is."},
               "keys": {"type": "array", "items": {"type": "string"}, "description": "Chords after the text: Enter, ctrl+c, alt+F4, …"}}),
        &[],
    )
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "screen_capture",
        "Capture the session desktop as a PNG (optionally scaled down); coordinates stay screen pixels.",
        Consequence::ReadOnly,
    )
    .schema(schema_capture)
    .safe(),
    ToolDecl::new("pointer", "Where the pointer is on the session desktop, and the screen size.", Consequence::ReadOnly)
        .schema(schema_none)
        .safe(),
    ToolDecl::new("mouse", "Move the pointer on the session desktop to (x, y), optionally clicking there.", Consequence::ControlsDesktop)
        .schema(schema_mouse),
    ToolDecl::new("keyboard", "Type text and press key chords on the session desktop.", Consequence::ControlsDesktop)
        .schema(schema_keyboard),
];

fn err(e: DisplayError) -> ExecOutcome {
    ExecOutcome::err(e.code(), e.to_string())
}

/// One call against the desktop (blocking, bounded RFB I/O).
pub fn run(desktop: &dyn Desktop, tool: &str, args: &Value) -> ExecOutcome {
    let done = |value: Value, text: String| ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None };
    match tool {
        "screen_capture" => {
            let req = CaptureRequest {
                max_width: args.get("max_width").and_then(Value::as_u64).map(|v| v as u32),
                max_height: args.get("max_height").and_then(Value::as_u64).map(|v| v as u32),
            };
            match desktop.screenshot(&req) {
                Ok(shot) => ExecOutcome::Done {
                    value: json!({"width": shot.width, "height": shot.height, "source_width": shot.source_width, "source_height": shot.source_height}),
                    content: vec![
                        ContentBlock::Image { media_type: "image/png".into(), data: shot.png },
                        ContentBlock::text(format!(
                            "{}×{} (screen {}×{}; coordinates are screen pixels)",
                            shot.width, shot.height, shot.source_width, shot.source_height
                        )),
                    ],
                    meta: None,
                },
                Err(e) => err(e),
            }
        }
        "pointer" => match desktop.state() {
            Ok(st) => done(
                json!(st),
                format!(
                    "pointer at ({}, {}) on a {}×{} screen{}",
                    st.x,
                    st.y,
                    st.width,
                    st.height,
                    if st.position_known { "" } else { " (position not yet known)" }
                ),
            ),
            Err(e) => err(e),
        },
        "mouse" => {
            let c = |k: &str| args.get(k).and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok());
            let (Some(x), Some(y)) = (c("x"), c("y")) else {
                return ExecOutcome::err("TOOL_BAD_ARGS", "`x` and `y` are screen pixels (0..65535)");
            };
            let click = args.get("click").and_then(Value::as_str);
            let count = args.get("count").and_then(Value::as_u64).unwrap_or(1) as u8;
            match input::mouse(desktop, x, y, click, count) {
                Ok(()) => done(
                    json!({"x": x, "y": y, "click": click}),
                    format!("pointer to ({x}, {y}){}", click.map(|c| format!(", {c} click")).unwrap_or_default()),
                ),
                Err(e) => err(e),
            }
        }
        "keyboard" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let keys: Vec<String> = args.get("keys").and_then(|k| serde_json::from_value(k.clone()).ok()).unwrap_or_default();
            match input::key_events(text, &keys).and_then(|events| input::type_keys(desktop, &events).map(|()| events.len())) {
                Ok(n) => done(json!({"events": n}), format!("sent {n} key events")),
                Err(e) => err(e),
            }
        }
        other => ExecOutcome::err("UNKNOWN_TOOL", format!("tool-display has no tool `{other}`")),
    }
}
