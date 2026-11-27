use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use gridcraft_engine::cmd::{CommandSpec, command_specs, find_command};
use gridcraft_ui_egui::{ControlRequest, Inbox};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{CustomEvent, CustomEventInit, HtmlCanvasElement};

use crate::theme;

pub const ENGINE_ID: &str = "fogbreak-grid";
pub const DEFAULT_HOST_KEY: &str = "fogbreakGridHost";
pub const MAX_OPEN_BYTES: u64 = 512 << 20;
const REPLY_DEADLINE_MS: f64 = 30_000.0;
const POLL_MS: i32 = 16;

#[derive(Clone)]
pub struct ExportResult {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub type ExportSlot = Arc<Mutex<Option<Result<ExportResult, String>>>>;

pub enum HostAction {
    SetTheme(bool),
    Export {
        format: String,
        name: Option<String>,
        deliver_save: bool,
        slot: ExportSlot,
    },
}

#[derive(Clone, Default)]
pub struct ActionQueue(Rc<RefCell<VecDeque<HostAction>>>);

impl ActionQueue {
    pub fn push(&self, action: HostAction) {
        self.0.borrow_mut().push_back(action);
    }

    pub fn drain(&self) -> Vec<HostAction> {
        self.0.borrow_mut().drain(..).collect()
    }
}

#[derive(Clone)]
pub struct HostContext {
    pub canvas: HtmlCanvasElement,
    pub ctx: egui::Context,
    pub requests: Sender<ControlRequest>,
    pub inbox: Inbox,
    pub actions: ActionQueue,
    pub dark_state: Arc<Mutex<bool>>,
}

pub fn find_page_canvas() -> Option<HtmlCanvasElement> {
    let doc = web_sys::window()?.document()?;
    let sel = format!("canvas[data-host=\"page\"][data-engine=\"{ENGINE_ID}\"]");
    if let Ok(Some(el)) = doc.query_selector(&sel)
        && let Ok(c) = el.dyn_into::<HtmlCanvasElement>()
    {
        return Some(c);
    }
    doc.get_element_by_id("fogbreak_canvas")
        .and_then(|e| e.dyn_into::<HtmlCanvasElement>().ok())
}

fn event_prefix(canvas: &HtmlCanvasElement) -> String {
    canvas
        .get_attribute("data-engine")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| ENGINE_ID.to_string())
}

fn rpc_key(canvas: &HtmlCanvasElement) -> String {
    canvas
        .get_attribute("data-rpc-key")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HOST_KEY.to_string())
}

fn js_from_json(value: &Value) -> JsValue {
    js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}

fn json_from_js(value: &JsValue) -> Value {
    if value.is_undefined() || value.is_null() {
        return Value::Null;
    }
    js_sys::JSON::stringify(value)
        .ok()
        .and_then(|s| s.as_string())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn dispatch_event(canvas: &HtmlCanvasElement, suffix: &str, detail: &JsValue) {
    let name = format!("{}:{suffix}", event_prefix(canvas));
    let init = CustomEventInit::new();
    init.set_detail(detail);
    if let Ok(ev) = CustomEvent::new_with_event_init_dict(&name, &init) {
        let _ = canvas.dispatch_event(&ev);
    }
}

pub fn emit_ready(canvas: &HtmlCanvasElement) {
    let detail = js_from_json(&json!({
        "engine": ENGINE_ID,
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "execute": true,
            "commands": true,
            "listCommands": true,
            "describeCommand": true,
            "docInspect": true,
            "uiInspect": true,
            "screenshot": true,
            "open": true,
            "theme": true,
            "io": true,
        }
    }));
    dispatch_event(canvas, "ready", &detail);
}

pub fn emit_failed(canvas: &HtmlCanvasElement, error: &str) {
    let detail = js_from_json(&json!({ "error": error }));
    dispatch_event(canvas, "failed", &detail);
}

pub fn emit_dirty(canvas: &HtmlCanvasElement, dirty: bool) {
    let detail = js_from_json(&json!({ "dirty": dirty }));
    dispatch_event(canvas, "dirty", &detail);
}

pub fn deliver_save(canvas: &HtmlCanvasElement, name: &str, save_as: bool, bytes: &[u8]) {
    let detail = js_sys::Object::new();
    let arr = js_sys::Uint8Array::from(bytes);
    let _ = js_sys::Reflect::set(&detail, &"name".into(), &name.into());
    let _ = js_sys::Reflect::set(&detail, &"saveAs".into(), &save_as.into());
    let _ = js_sys::Reflect::set(&detail, &"data".into(), &arr);
    let _ = js_sys::Reflect::set(&detail, &"bytes".into(), &arr);
    dispatch_event(canvas, "save", &detail.into());
}

pub fn emit_handoff(canvas: &HtmlCanvasElement, target: &str, payload: Value) {
    let detail = js_from_json(&json!({ "target": target, "payload": payload }));
    dispatch_event(canvas, "handoff", &detail);
}

pub fn emit_theme(canvas: &HtmlCanvasElement, dark: bool) {
    let mode = if dark { "dark" } else { "light" };
    let detail = js_from_json(&json!({ "theme": mode, "dark": dark }));
    dispatch_event(canvas, "theme", &detail);
}

fn spec_to_json(spec: &CommandSpec) -> Value {
    json!({
        "id": spec.id,
        "label": spec.label,
        "menu": spec.menu,
        "shortcut": spec.shortcut,
        "params": spec.params,
        "journal": spec.journal,
        "undoable": spec.undoable,
    })
}

fn ui_command_descriptors() -> Vec<Value> {
    [
        ("ui.ribbonTab", "Ribbon Tab", ""),
        ("app.checkForUpdates", "Check for Updates…", ""),
        ("view.collapseRibbon", "Collapse Ribbon", ""),
        ("view.expandFormulaBar", "Expand Formula Bar", "{on?: bool}"),
        ("ui.pythonMode", "Python Mode", "{on?: bool}"),
        ("view.darkMode", "Dark Mode", ""),
        ("view.theme", "Display Theme", ""),
        ("view.animations", "Animations", "{on?: bool}"),
        ("options.autoComplete", "Enable AutoComplete for cell values", "{on?: bool}"),
        ("options.highlightActiveRow", "Highlight the active cell's row", "{on?: bool}"),
        ("view.interfaceScale", "Interface Size", "{percent: 75..=200 | \"auto\"}"),
        ("ui.window.minimize", "Minimize Window", ""),
        ("ui.window.maximize", "Maximize Window", "{on: bool}"),
        ("ui.window.close", "Close Window", ""),
        ("ui.window.beginResize", "Resize Window", "{direction: string}"),
        ("ui.dialog", "Open Dialog", "{name: string}"),
        ("ui.selectionMode", "Extend or Add to Selection", "{mode?: string}"),
        ("ui.backstage", "File (Backstage view)", "{page?: string}"),
        ("ui.printSettings", "Print Settings", "{}"),
        ("ui.printSubmit", "Submit Print Job", "{}"),
        ("app.language.set", "Interface Language", "{language: string}"),
        ("app.language.english", "Interface Language: English", ""),
        ("app.language.japanese", "Interface Language: Japanese", ""),
        ("document.inspect", "Inspect Workbook", "{}"),
        ("sheet.read", "Read Sheet", "{range?: string}"),
        ("cell.get", "Get Cell", "{cell?: string}"),
        ("selection.stats", "Selection Stats", "{}"),
        ("ui.inspect", "Inspect UI", "{}"),
        ("ui.screenshot", "Screenshot Canvas", "{}"),
        ("ui.set", "Set UI Options", "{dark?: bool, formulaBar?: bool, ribbonCollapsed?: bool, animations?: bool}"),
        ("engine.journal", "Command Journal", "{}"),
    ]
    .into_iter()
    .map(|(id, label, params)| json!({"id": id, "label": label, "menu": [], "shortcut": Value::Null, "params": params, "ui": true}))
    .collect()
}

pub fn all_command_descriptors() -> Vec<Value> {
    let mut out: Vec<Value> = command_specs().iter().map(spec_to_json).collect();
    for ui in ui_command_descriptors() {
        let id = ui.get("id").and_then(Value::as_str).unwrap_or_default();
        if !out.iter().any(|c| c.get("id").and_then(Value::as_str) == Some(id)) {
            out.push(ui);
        }
    }
    out
}

pub fn describe_command(id: &str) -> Option<Value> {
    if let Some(spec) = find_command(id) {
        return Some(spec_to_json(spec));
    }
    ui_command_descriptors()
        .into_iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
}

fn io_formats_json() -> Value {
    json!({
        "import": ["xlsx", "xlsm", "xltx", "xltm", "xls", "xlsb", "ods", "csv", "tsv", "txt", "slk", "json"],
        "export": ["xlsx", "xlsm", "xltx", "xltm", "pdf", "csv", "tsv", "txt", "json", "html"],
    })
}

fn parse_bytes_arg(val: &JsValue) -> Option<Vec<u8>> {
    if let Some(u8a) = val.dyn_ref::<js_sys::Uint8Array>() {
        return Some(u8a.to_vec());
    }
    if let Some(ab) = val.dyn_ref::<js_sys::ArrayBuffer>() {
        return Some(js_sys::Uint8Array::new(ab).to_vec());
    }
    if let Some(b64) = val.as_string() {
        let raw = b64.split_once("base64,").map_or(b64.as_str(), |(_, rhs)| rhs);
        return gridcraft_engine::io::base64_decode(raw.trim());
    }
    if js_sys::Array::is_array(val) {
        let arr = js_sys::Array::from(val);
        let mut out = Vec::with_capacity(arr.length() as usize);
        for v in arr.iter() {
            let n = v.as_f64()?;
            out.push(n.clamp(0.0, 255.0) as u8);
        }
        return Some(out);
    }
    None
}

fn decode_open_payload(detail: &JsValue) -> Result<(String, Vec<u8>), String> {
    if let Some(bytes) = parse_bytes_arg(detail) {
        return Ok(("workbook.xlsx".to_string(), bytes));
    }
    let name = js_sys::Reflect::get(detail, &"name".into())
        .ok()
        .and_then(|v| v.as_string())
        .filter(|s| !s.trim().is_empty() && s != "document")
        .unwrap_or_else(|| "workbook.xlsx".to_string());
    let data = js_sys::Reflect::get(detail, &"bytes".into())
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .or_else(|| js_sys::Reflect::get(detail, &"data".into()).ok().filter(|v| !v.is_undefined() && !v.is_null()))
        .or_else(|| js_sys::Reflect::get(detail, &"base64".into()).ok().filter(|v| !v.is_undefined() && !v.is_null()))
        .ok_or_else(|| "missing `bytes`, `data`, or `base64` payload".to_string())?;
    let bytes = parse_bytes_arg(&data).ok_or_else(|| "expected Uint8Array, ArrayBuffer, byte array, or base64 string".to_string())?;
    Ok((name, bytes))
}

fn open_from_js_detail(detail: &JsValue, inbox: &Inbox, ctx: &egui::Context) -> Result<String, String> {
    let (name, bytes) = decode_open_payload(detail)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_OPEN_BYTES {
        return Err(format!("file exceeds {} MiB host open limit", MAX_OPEN_BYTES >> 20));
    }
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name.clone(), bytes));
    ctx.request_repaint();
    Ok(name)
}

fn parse_dark_flag(arg: &JsValue) -> Option<bool> {
    if let Some(b) = arg.as_bool() {
        return Some(b);
    }
    if let Some(s) = arg.as_string() {
        return match s.to_ascii_lowercase().as_str() {
            "dark" | "true" => Some(true),
            "light" | "false" => Some(false),
            _ => None,
        };
    }
    let json = json_from_js(arg);
    json.get("dark")
        .and_then(Value::as_bool)
        .or_else(|| {
            json.get("theme")
                .or_else(|| json.get("value"))
                .and_then(Value::as_str)
                .and_then(|s| match s.to_ascii_lowercase().as_str() {
                    "dark" => Some(true),
                    "light" => Some(false),
                    _ => None,
                })
        })
}

pub fn listen_open_and_theme(host: &HostContext) {
    let prefix = event_prefix(&host.canvas);
    let inbox = host.inbox.clone();
    let open_ctx = host.ctx.clone();
    let on_open = Closure::<dyn FnMut(CustomEvent)>::new(move |ev: CustomEvent| {
        if let Err(err) = open_from_js_detail(&ev.detail(), &inbox, &open_ctx) {
            log::warn!("fogbreak-grid host open failed: {err}");
        }
    });
    let _ = host
        .canvas
        .add_event_listener_with_callback(&format!("{prefix}:open"), on_open.as_ref().unchecked_ref());
    on_open.forget();

    let actions = host.actions.clone();
    let theme_ctx = host.ctx.clone();
    let dark_state = host.dark_state.clone();
    let on_theme = Closure::<dyn FnMut(CustomEvent)>::new(move |ev: CustomEvent| {
        if let Some(dark) = parse_dark_flag(&ev.detail()) {
            *dark_state.lock().unwrap_or_else(|e| e.into_inner()) = dark;
            actions.push(HostAction::SetTheme(dark));
            theme_ctx.request_repaint();
        }
    });
    let _ = host
        .canvas
        .add_event_listener_with_callback(&format!("{prefix}:set-theme"), on_theme.as_ref().unchecked_ref());
    on_theme.forget();
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

async fn wait_reply(reply: Receiver<Value>, state: &Arc<std::sync::atomic::AtomicU8>) -> Result<Value, String> {
    let start = js_sys::Date::now();
    loop {
        match reply.try_recv() {
            Ok(val) => return Ok(val),
            Err(TryRecvError::Empty) => {
                if js_sys::Date::now() - start > REPLY_DEADLINE_MS {
                    ControlRequest::abandon(state);
                    return Err("control request timed out".to_string());
                }
                sleep_ms(POLL_MS).await;
            }
            Err(TryRecvError::Disconnected) => {
                ControlRequest::abandon(state);
                return Err("the app dropped the control request".to_string());
            }
        }
    }
}

async fn send_control(host: &HostContext, method: String, params: Value) -> Result<Value, String> {
    let (control, reply) = ControlRequest::new(method, params);
    let state = control.state.clone();
    host.requests
        .send(control)
        .map_err(|_| "application is not running".to_string())?;
    host.ctx.request_repaint();
    let raw = wait_reply(reply, &state).await?;
    if raw.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = raw.get("error").and_then(Value::as_str).unwrap_or("request failed");
        return Err(msg.to_string());
    }
    if raw.get("ok").and_then(Value::as_bool) == Some(true)
        && let Some(res) = raw.get("result")
    {
        return Ok(res.clone());
    }
    Ok(raw)
}

async fn wait_export(slot: ExportSlot, ctx: egui::Context) -> Result<ExportResult, String> {
    let start = js_sys::Date::now();
    loop {
        if let Some(res) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return res;
        }
        if js_sys::Date::now() - start > REPLY_DEADLINE_MS {
            return Err("export timed out".to_string());
        }
        ctx.request_repaint();
        sleep_ms(POLL_MS).await;
    }
}

fn export_result_to_js(res: ExportResult) -> JsValue {
    let obj = js_sys::Object::new();
    let arr = js_sys::Uint8Array::from(res.bytes.as_slice());
    let _ = js_sys::Reflect::set(&obj, &"name".into(), &res.name.into());
    let _ = js_sys::Reflect::set(&obj, &"mime".into(), &res.mime.into());
    let _ = js_sys::Reflect::set(&obj, &"size".into(), &(res.bytes.len() as f64).into());
    let _ = js_sys::Reflect::set(&obj, &"bytes".into(), &arr);
    obj.into()
}

fn capture_canvas_png(canvas: &HtmlCanvasElement) -> Result<Value, String> {
    let data_url = canvas
        .to_data_url_with_type("image/png")
        .map_err(|e| format!("canvas screenshot failed: {e:?}"))?;
    let base64 = data_url.strip_prefix("data:image/png;base64,").unwrap_or(&data_url);
    Ok(json!({
        "mime": "image/png",
        "dataUrl": data_url,
        "pngBase64": base64,
    }))
}

fn apply_theme_now(host: &HostContext, dark: bool) -> Value {
    *host.dark_state.lock().unwrap_or_else(|e| e.into_inner()) = dark;
    theme::apply_palette(&host.ctx, dark);
    theme::sync_document_theme(dark);
    emit_theme(&host.canvas, dark);
    host.actions.push(HostAction::SetTheme(dark));
    host.ctx.request_repaint();
    let mode = if dark { "dark" } else { "light" };
    json!({ "theme": mode, "dark": dark })
}

async fn run_export_rpc(host: &HostContext, args: &JsValue) -> Result<JsValue, String> {
    let json_args = json_from_js(args);
    let format = json_args
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("xlsx")
        .trim()
        .to_ascii_lowercase();
    let name = json_args.get("name").and_then(Value::as_str).map(str::to_string);
    let deliver_save = json_args.get("deliverSave").and_then(Value::as_bool).unwrap_or(false);
    let slot: ExportSlot = Arc::new(Mutex::new(None));
    host.actions.push(HostAction::Export {
        format,
        name,
        deliver_save,
        slot: slot.clone(),
    });
    host.ctx.request_repaint();
    let res = wait_export(slot, host.ctx.clone()).await?;
    Ok(export_result_to_js(res))
}

fn resolve_command_id(args: &Value) -> Result<String, String> {
    args.get("command")
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "missing `command`".to_string())
}

async fn dispatch_rpc_json(host: &HostContext, method: String, params: Value, raw_params: JsValue) -> Result<JsValue, String> {
    match method.as_str() {
        "engine.commands" | "commands" | "listCommands" => Ok(js_from_json(&Value::Array(all_command_descriptors()))),
        "describeCommand" => {
            let id = resolve_command_id(&params).unwrap_or_default();
            let desc = describe_command(&id).unwrap_or(Value::Null);
            Ok(js_from_json(&desc))
        }
        "execute" | "executeCommand" | "engine.execute" | "command" => {
            let id = resolve_command_id(&params)?;
            let cmd_params = params.get("params").cloned().unwrap_or_else(|| json!({}));
            let res = send_control(host, "engine.execute".to_string(), json!({ "command": id, "params": cmd_params })).await?;
            Ok(js_from_json(&res))
        }
        "docInspect" | "document.inspect" => Ok(js_from_json(&send_control(host, "document.inspect".to_string(), params).await?)),
        "uiInspect" | "ui.inspect" => Ok(js_from_json(&send_control(host, "ui.inspect".to_string(), params).await?)),
        "inspect" => {
            let ui = send_control(host, "ui.inspect".to_string(), json!({})).await?;
            let doc = send_control(host, "document.inspect".to_string(), json!({})).await.ok();
            Ok(js_from_json(&json!({ "document": doc, "ui": ui })))
        }
        "screenshot" | "ui.screenshot" => Ok(js_from_json(&capture_canvas_png(&host.canvas)?)),
        "open" | "io.import" => {
            let opened = open_from_js_detail(&raw_params, &host.inbox, &host.ctx)?;
            Ok(js_from_json(&json!({ "opened": true, "name": opened })))
        }
        "emitHandoff" => {
            let target = params.get("target").and_then(Value::as_str).unwrap_or("");
            let payload = params.get("payload").cloned().unwrap_or(Value::Null);
            emit_handoff(&host.canvas, target, payload);
            Ok(js_from_json(&json!({ "ok": true })))
        }
        "theme.get" => {
            let dark = *host.dark_state.lock().unwrap_or_else(|e| e.into_inner());
            let mode = if dark { "dark" } else { "light" };
            Ok(js_from_json(&json!({ "theme": mode, "dark": dark })))
        }
        "setTheme" | "theme.set" => {
            let dark = parse_dark_flag(&raw_params).unwrap_or_else(theme::initial_dark_mode);
            Ok(js_from_json(&apply_theme_now(host, dark)))
        }
        "io.formats" => Ok(js_from_json(&io_formats_json())),
        "io.export" => run_export_rpc(host, &raw_params).await,
        other => Ok(js_from_json(&send_control(host, other.to_string(), params).await?)),
    }
}

fn spawn_rpc_promise(host: HostContext, method: String, params: Value, raw_params: JsValue) -> js_sys::Promise {
    wasm_bindgen_futures::future_to_promise(async move {
        dispatch_rpc_json(&host, method, params, raw_params)
            .await
            .map_err(|e| js_sys::Error::new(&e).into())
    })
}

fn set_fn0(target: &js_sys::Object, name: &str, f: impl Fn() -> JsValue + 'static) {
    let closure = Closure::wrap(Box::new(f) as Box<dyn Fn() -> JsValue>);
    let _ = js_sys::Reflect::set(target, &name.into(), closure.as_ref());
    closure.forget();
}

fn set_fn1(target: &js_sys::Object, name: &str, f: impl Fn(JsValue) -> JsValue + 'static) {
    let closure = Closure::wrap(Box::new(f) as Box<dyn Fn(JsValue) -> JsValue>);
    let _ = js_sys::Reflect::set(target, &name.into(), closure.as_ref());
    closure.forget();
}

fn set_fn2(target: &js_sys::Object, name: &str, f: impl Fn(JsValue, JsValue) -> JsValue + 'static) {
    let closure = Closure::wrap(Box::new(f) as Box<dyn Fn(JsValue, JsValue) -> JsValue>);
    let _ = js_sys::Reflect::set(target, &name.into(), closure.as_ref());
    closure.forget();
}

fn install_rpc_commands(api: &js_sys::Object, host: &HostContext) {
    set_fn0(api, "commands", || js_from_json(&Value::Array(all_command_descriptors())));
    set_fn0(api, "listCommands", || js_from_json(&Value::Array(all_command_descriptors())));
    set_fn1(api, "describeCommand", |id| {
        let key = id.as_string().unwrap_or_default();
        js_from_json(&describe_command(&key).unwrap_or(Value::Null))
    });

    let h = host.clone();
    set_fn2(api, "request", move |method, params| {
        let m = method.as_string().unwrap_or_default();
        let p = json_from_js(&params);
        spawn_rpc_promise(h.clone(), m, p, params).into()
    });

    for fn_name in ["execute", "executeCommand"] {
        let h = host.clone();
        set_fn2(api, fn_name, move |cmd, params| {
            let p = json!({
                "command": cmd.as_string().unwrap_or_default(),
                "params": json_from_js(&params),
            });
            spawn_rpc_promise(h.clone(), "execute".to_string(), p, JsValue::NULL).into()
        });
    }
}

fn install_rpc_inspect_and_actions(api: &js_sys::Object, host: &HostContext) {
    for (fn_name, method) in [
        ("docInspect", "document.inspect"),
        ("uiInspect", "ui.inspect"),
        ("inspect", "inspect"),
        ("screenshot", "screenshot"),
    ] {
        let h = host.clone();
        let m = method.to_string();
        set_fn1(api, fn_name, move |params| {
            let p = json_from_js(&params);
            spawn_rpc_promise(h.clone(), m.clone(), p, params).into()
        });
    }

    let h = host.clone();
    set_fn1(api, "open", move |detail| {
        let p = json_from_js(&detail);
        spawn_rpc_promise(h.clone(), "open".to_string(), p, detail).into()
    });

    let h = host.clone();
    set_fn2(api, "emitHandoff", move |target, payload| {
        let p = json!({
            "target": target.as_string().unwrap_or_default(),
            "payload": json_from_js(&payload),
        });
        spawn_rpc_promise(h.clone(), "emitHandoff".to_string(), p, JsValue::NULL).into()
    });

    let h = host.clone();
    set_fn1(api, "setTheme", move |arg| {
        let p = json_from_js(&arg);
        spawn_rpc_promise(h.clone(), "setTheme".to_string(), p, arg).into()
    });
}

fn install_rpc_io(api: &js_sys::Object, host: &HostContext) {
    let io_obj = js_sys::Object::new();
    set_fn0(&io_obj, "formats", || js_from_json(&io_formats_json()));

    let h = host.clone();
    set_fn1(&io_obj, "import", move |arg| {
        let p = json_from_js(&arg);
        spawn_rpc_promise(h.clone(), "io.import".to_string(), p, arg).into()
    });

    let h = host.clone();
    set_fn1(&io_obj, "export", move |arg| {
        let p = json_from_js(&arg);
        spawn_rpc_promise(h.clone(), "io.export".to_string(), p, arg).into()
    });

    let _ = js_sys::Reflect::set(api, &"io".into(), &io_obj);
}

pub fn install_host_rpc(host: HostContext) {
    let Some(win) = web_sys::window() else { return };
    let key = rpc_key(&host.canvas);
    let api = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&api, &"engine".into(), &ENGINE_ID.into());
    install_rpc_commands(&api, &host);
    install_rpc_inspect_and_actions(&api, &host);
    install_rpc_io(&api, &host);
    let _ = js_sys::Reflect::set(&win, &key.into(), &api);
}
