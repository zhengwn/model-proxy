use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

/// Creates the system tray icon with menu items.
///
/// Menu items:
/// - 显示窗口 (Show Window)
/// - 启动服务 (Start Service)
/// - 停止服务 (Stop Service)
/// - separator
/// - 退出 (Quit)
///
/// Tray icon states (future enhancement):
/// - Green: service running
/// - Gray: service stopped
/// - Red: service error
pub fn create_tray(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItemBuilder::with_id("show", "显示窗口").build(app)?;
    let start_item = MenuItemBuilder::with_id("start_service", "启动服务").build(app)?;
    let stop_item = MenuItemBuilder::with_id("stop_service", "停止服务").build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "退出").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&show_item)
        .separator()
        .item(&start_item)
        .item(&stop_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let mut builder = TrayIconBuilder::new().menu(&menu).tooltip("Model Proxy");

    #[cfg(target_os = "macos")]
    {
        let tray_icon = Image::new(include_bytes!("../icons/tray-template.rgba"), 32, 32);
        builder = builder.icon(tray_icon).icon_as_template(true);
    }

    #[cfg(not(target_os = "macos"))]
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder
        .on_menu_event(move |app, event| {
            match event.id().as_ref() {
                "show" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                }
                "start_service" => {
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let service = app_handle.state::<crate::service::ServiceManager>();
                        let app_state = app_handle.state::<crate::commands::AppState>();
                        // Invoke the same logic as the start_service command
                        if let Err(e) =
                            crate::commands::start_service(app_handle.clone(), app_state, service)
                                .await
                        {
                            tracing::error!("Tray: start service failed: {}", e);
                        }
                    });
                }
                "stop_service" => {
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let service = app_handle.state::<crate::service::ServiceManager>();
                        if let Err(e) = crate::commands::stop_service(service).await {
                            tracing::error!("Tray: stop service failed: {}", e);
                        }
                    });
                }
                "quit" => {
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let service = app_handle.state::<crate::service::ServiceManager>();
                        let _ = crate::commands::stop_service(service).await;
                        app_handle.exit(0);
                    });
                }
                _ => {}
            }
        })
        .build(app)?;

    Ok(())
}
