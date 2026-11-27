#[cfg(target_arch = "wasm32")]
mod host_bridge;
#[cfg(target_arch = "wasm32")]
mod theme;

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

#[cfg(target_arch = "wasm32")]
fn main() {
    wasm_main();
}

#[cfg(target_arch = "wasm32")]
fn wasm_main() {
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};

    use gridcraft_engine::Session;
    use gridcraft_ui_egui::{ControlRequest, Inbox, Services, SheetApp};
    use serde_json::{Value, json};
    use web_sys::HtmlCanvasElement;

    use crate::host_bridge::{ActionQueue, ExportResult, HostAction, HostContext};

    const LOADING_ID: &str = "fogbreak_loading";

    struct WebShell {
        app: SheetApp,
        inbox: Inbox,
        actions: ActionQueue,
        canvas: HtmlCanvasElement,
        dark_state: Arc<Mutex<bool>>,
        last_dirty: bool,
    }

    fn query_string() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    fn export_mime(format: &str) -> &'static str {
        match format {
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsm" => "application/vnd.ms-excel.sheet.macroEnabled.12",
            "xltx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
            "xltm" => "application/vnd.ms-excel.template.macroEnabled.12",
            "pdf" => "application/pdf",
            "csv" => "text/csv",
            "tsv" | "txt" => "text/tab-separated-values",
            "html" => "text/html",
            "json" => "application/json",
            _ => "application/octet-stream",
        }
    }

    fn normalize_export_name(name: Option<String>, format: &str, title: &str) -> String {
        let stem = title.trim();
        let fallback_stem = if stem.is_empty() { "Workbook" } else { stem };
        let base = name.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("{fallback_stem}.{format}"));
        if base.ends_with(&format!(".{format}")) {
            base
        } else {
            format!("{base}.{format}")
        }
    }

    fn perform_export(app: &mut SheetApp, format: &str, name: Option<String>) -> Result<ExportResult, String> {
        app.store_view_state();
        let title = app
            .session
            .active()
            .map(|d| d.display_title())
            .unwrap_or_else(|| "Workbook".to_string());
        let doc_name = normalize_export_name(name, format, &title);
        let bytes = if format == "pdf" {
            let res = app.session.run("file.exportPdf", json!({}))?;
            let b64 = res
                .get("base64")
                .and_then(Value::as_str)
                .ok_or_else(|| "PDF export returned no payload".to_string())?;
            gridcraft_engine::io::base64_decode(b64).ok_or_else(|| "invalid base64 PDF payload".to_string())?
        } else {
            let doc = app.session.active().ok_or_else(|| "no active workbook".to_string())?;
            gridcraft_engine::io::save_bytes(&doc.wb, &doc_name).map_err(|e| e.to_string())?
        };
        Ok(ExportResult {
            name: doc_name,
            mime: export_mime(format).to_string(),
            bytes,
        })
    }

    impl WebShell {
        fn handle_actions(&mut self, ctx: &egui::Context) {
            for action in self.actions.drain() {
                match action {
                    HostAction::SetTheme(dark) => {
                        *self.dark_state.lock().unwrap_or_else(|e| e.into_inner()) = dark;
                        self.app.ui.dark = dark;
                        self.app.ui.system_theme = false;
                        theme::apply_palette(ctx, dark);
                        theme::sync_document_theme(dark);
                        host_bridge::emit_theme(&self.canvas, dark);
                    }
                    HostAction::Export {
                        format,
                        name,
                        deliver_save,
                        slot,
                    } => {
                        let result = perform_export(&mut self.app, &format, name);
                        if deliver_save && let Ok(ref res) = result {
                            host_bridge::deliver_save(&self.canvas, &res.name, false, &res.bytes);
                        }
                        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                    }
                }
            }
        }
    }

    impl eframe::App for WebShell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
            for f in dropped {
                let inbox = self.inbox.clone();
                let ctx = ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped.xlsx".into());
                    if let Ok(bytes) = f.bytes_async().await {
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                        ctx.request_repaint();
                    }
                });
            }
            self.handle_actions(ctx);
            self.app.logic(ctx);
            self.app.close_path(ctx);
            let dark = *self.dark_state.lock().unwrap_or_else(|e| e.into_inner());
            if ctx.style().visuals.dark_mode != dark {
                theme::apply_palette(ctx, dark);
            }
            let dirty = self.app.session.active().is_some_and(|d| d.is_dirty());
            if dirty != self.last_dirty {
                self.last_dirty = dirty;
                host_bridge::emit_dirty(&self.canvas, dirty);
            }
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            self.app.raw_input_hook(ctx, raw);
            self.app.before_input(ctx);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.app.ui(ui);
        }
    }

    fn build_services(inbox: Inbox, ctx: egui::Context, canvas: HtmlCanvasElement) -> Services {
        let open_inbox = inbox.clone();
        let open_ctx = ctx.clone();
        let picture_ctx = ctx;
        let save_canvas = canvas;
        Services {
            open_async: Some(Box::new(move || {
                let inbox = open_inbox.clone();
                let ctx = open_ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .add_filter("Spreadsheets", &["xlsx", "xlsm", "xls", "xlsb", "ods", "csv", "tsv", "txt", "slk", "json"])
                        .pick_file()
                        .await
                    else {
                        return;
                    };
                    let bytes = file.read().await;
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((file.file_name(), bytes));
                    ctx.request_repaint();
                });
            })),
            pick_picture_async: Some(Box::new(move |inbox| {
                let ctx = picture_ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .add_filter("Pictures (PNG, JPEG)", &["png", "jpg", "jpeg"])
                        .pick_file()
                        .await
                    else {
                        return;
                    };
                    let bytes = file.read().await;
                    inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((file.file_name(), bytes));
                    ctx.request_repaint();
                });
            })),
            download: Some(Box::new(move |name: &str, bytes: &[u8]| {
                host_bridge::deliver_save(&save_canvas, name, false, bytes);
            })),
            open_url: Some(Box::new(|url: &str| {
                if let Some(w) = web_sys::window()
                    && let gridcraft_engine::links::LinkKind::External(url) = gridcraft_engine::links::classify(url)
                {
                    let _ = w.open_with_url_and_target_and_features(&url, "_blank", "noopener,noreferrer");
                }
            })),
            inbox: Some(inbox),
            ..Default::default()
        }
    }

    fn finish_boot(document: &web_sys::Document, canvas: &HtmlCanvasElement, result: Result<(), eframe::Error>) {
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match &result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!("<p>Fogbreak Grid failed to start: {e:?}</p>")),
            }
        }
        match &result {
            Ok(()) => host_bridge::emit_ready(canvas),
            Err(e) => host_bridge::emit_failed(canvas, &format!("{e:?}")),
        }
    }

    fn web_options() -> eframe::WebOptions {
        let mut options = eframe::WebOptions::default();
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
            let default_descriptor = std::sync::Arc::clone(&setup.device_descriptor);
            setup.device_descriptor = std::sync::Arc::new(move |adapter| {
                let mut descriptor = default_descriptor(adapter);
                descriptor.required_limits = descriptor.required_limits.or_worse_values_from(&adapter.limits());
                descriptor
            });
        }
        if query_string().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        options
    }

    fn build_shell(
        cc: &eframe::CreationContext<'_>,
        canvas: HtmlCanvasElement,
        req_tx: std::sync::mpsc::Sender<ControlRequest>,
        req_rx: std::sync::mpsc::Receiver<ControlRequest>,
        actions: ActionQueue,
        dark_state: Arc<Mutex<bool>>,
    ) -> WebShell {
        gridcraft_ui_egui::framework::install_headless(&cc.egui_ctx);
        let dark = *dark_state.lock().unwrap_or_else(|e| e.into_inner());
        let inbox: Inbox = Inbox::default();
        let mut session = Session::new();
        let q = query_string();
        if let Some(name) = q.split(['?', '&']).find_map(|kv| kv.strip_prefix("sample=")) {
            let _ = session.execute("file.new", json!({ "sample": name }));
        } else if q.contains("sample") {
            let _ = session.execute("file.new", json!({ "sample": "sales" }));
        }
        let services = build_services(inbox.clone(), cc.egui_ctx.clone(), canvas.clone());
        let mut app = SheetApp::new(session, services);
        app.ui.dark = dark;
        app.ui.system_theme = false;
        let browser = gridcraft_ui_egui::i18n::Language::system();
        if !matches!(
            browser,
            gridcraft_ui_egui::i18n::Language::Zh | gridcraft_ui_egui::i18n::Language::Ja | gridcraft_ui_egui::i18n::Language::Ko
        ) {
            app.ui.language = browser;
        }
        SheetApp::setup_context_for_language(&cc.egui_ctx, dark, app.ui.language);
        theme::apply_palette(&cc.egui_ctx, dark);
        theme::sync_document_theme(dark);
        app.control_rx = Some(req_rx);

        let host_ctx = HostContext {
            canvas: canvas.clone(),
            ctx: cc.egui_ctx.clone(),
            requests: req_tx,
            inbox: inbox.clone(),
            actions: actions.clone(),
            dark_state: dark_state.clone(),
        };
        host_bridge::listen_open_and_theme(&host_ctx);
        host_bridge::install_host_rpc(host_ctx);

        let last_dirty = app.session.active().is_some_and(|d| d.is_dirty());
        WebShell {
            app,
            inbox,
            actions,
            canvas,
            dark_state,
            last_dirty,
        }
    }

    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async move {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let Some(canvas) = host_bridge::find_page_canvas() else {
            return;
        };
        let options = web_options();
        let dark = theme::initial_dark_mode();
        let dark_state = Arc::new(Mutex::new(dark));
        let actions = ActionQueue::default();
        let (req_tx, req_rx) = channel::<ControlRequest>();
        let canvas_for_boot = canvas.clone();
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    Ok(Box::new(build_shell(
                        cc,
                        canvas_for_boot.clone(),
                        req_tx.clone(),
                        req_rx,
                        actions,
                        dark_state,
                    )))
                }),
            )
            .await;
        finish_boot(&document, &canvas, result);
    });
}
