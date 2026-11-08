#[allow(dead_code)]
mod auth;
mod download;
mod photos;
mod srp;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, SharedString, VecModel};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

slint::slint! {
    import { Button, LineEdit, ComboBox, ScrollView } from "std-widgets.slint";
    export struct PhotoItem { name: string, image: image }
    export component App inherits Window {
        title: "iCloud Download Helper";
        width: 1000px; height: 700px;
        in-out property <string> username;
        in-out property <string> region: "com";
        in-out property <string> secret;
        in-out property <string> directory: "./photos";
        in-out property <string> status: "输入账号并连接 iCloud";
        in-out property <string> selected: "选择相册";
        in-out property <[string]> albums;
        in-out property <[PhotoItem]> photos;
        in-out property <bool> connected: false;
        in-out property <bool> busy: false;
        in-out property <bool> downloading: false;
        in-out property <bool> needs-secret: false;
        in-out property <bool> code-mode: false;
        in-out property <bool> can-next: false;
        callback connect(string, string);
        callback submit-secret(string);
        callback select-album(int);
        callback next-page();
        callback download-album(string);
        VerticalLayout {
            padding: 16px; spacing: 10px;
            Text { text: "iCloud 照片"; font-size: 24px; font-weight: 700; }
            HorizontalLayout {
                spacing: 8px;
                LineEdit { text <=> root.username; placeholder-text: "Apple 账号"; enabled: !root.busy && !root.connected; }
                ComboBox { model: ["com", "cn"]; current-value <=> root.region; enabled: !root.busy && !root.connected; }
                Button { text: "连接"; enabled: !root.busy && !root.connected; clicked => { root.connect(root.username, root.region); } }
            }
            if root.needs-secret: HorizontalLayout {
                spacing: 8px;
                LineEdit { text <=> root.secret; placeholder-text: root.code-mode ? "六位验证码" : "Apple 账号密码"; input-type: root.code-mode ? InputType.number : InputType.password; accepted => { root.submit-secret(root.secret); root.secret = ""; } }
                Button { text: root.code-mode ? "验证" : "登录"; clicked => { root.submit-secret(root.secret); root.secret = ""; } }
            }
            Text { text: root.status; wrap: word-wrap; color: #526177; }
            HorizontalLayout {
                spacing: 12px;
                VerticalLayout {
                    width: 220px; spacing: 6px;
                    Text { text: "相册"; font-weight: 700; }
                    ScrollView {
                        VerticalLayout {
                            for album[index] in root.albums: Button {
                                text: album; enabled: root.connected && !root.downloading;
                                clicked => { root.select-album(index); }
                            }
                        }
                    }
                }
                VerticalLayout {
                    spacing: 8px;
                    Text { text: root.selected; font-size: 18px; font-weight: 700; }
                    ScrollView {
                        VerticalLayout {
                            spacing: 8px;
                            for photo in root.photos: HorizontalLayout {
                                height: 112px; spacing: 12px;
                                Image { source: photo.image; width: 150px; height: 108px; image-fit: contain; }
                                Text { text: photo.name; vertical-alignment: center; wrap: word-wrap; }
                            }
                        }
                    }
                    Button { text: "下一页"; enabled: root.can-next && !root.busy; clicked => { root.next-page(); } }
                    HorizontalLayout {
                        spacing: 8px;
                        LineEdit { text <=> root.directory; placeholder-text: "下载目录"; }
                        Button { text: "下载此相册"; enabled: root.connected && !root.busy && root.selected != "选择相册"; clicked => { root.download-album(root.directory); } }
                    }
                }
            }
        }
    }
}

enum Command {
    Select(usize, u64),
    Next(u64),
    Download(PathBuf),
}

fn status(ui: &slint::Weak<App>, message: String, busy: bool) {
    let _ = ui.upgrade_in_event_loop(move |app| {
        app.set_status(message.into());
        app.set_busy(busy);
    });
}

fn main() -> Result<()> {
    let app = App::new()?;
    let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
    let generation = Arc::new(AtomicU64::new(0));
    let (secret_tx, secret_rx) = mpsc::channel::<String>();
    let weak = app.as_weak();
    app.on_submit_secret(move |value| {
        let _ = secret_tx.send(value.to_string());
    });
    let worker_ui = weak.clone();
    let worker_generation = generation.clone();
    app.on_connect(move |username, region| {
        let username = username.to_string();
        if username.trim().is_empty() {
            status(&worker_ui, "请输入 Apple 账号".into(), false);
            return;
        }
        let region = region.to_string();
        let ui = worker_ui.clone();
        let generation = worker_generation.clone();
        // A single worker owns the session and receives subsequent UI actions.
        // The receiver is consumed once because the UI permits one connection per window.
        let rx = COMMAND_RECEIVER.with(|slot| slot.borrow_mut().take());
        let Some((commands, secrets)) = rx else {
            return;
        };
        status(&ui, "正在连接 iCloud…".into(), true);
        std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new()
                .context("无法创建后台运行时")
                .and_then(|rt| {
                    rt.block_on(worker(
                        ui.clone(),
                        username,
                        region,
                        commands,
                        secrets,
                        generation,
                    ))
                });
            if let Err(e) = result {
                let message = format!("错误：{e:#}。请关闭窗口后重试。");
                let _ = ui.upgrade_in_event_loop(move |app| {
                    app.set_needs_secret(false);
                    app.set_busy(true);
                    app.set_status(message.into());
                });
            }
        });
    });
    COMMAND_RECEIVER.with(|slot| *slot.borrow_mut() = Some((commands_rx, secret_rx)));
    let tx = commands_tx.clone();
    let ui = weak.clone();
    let selection_generation = generation.clone();
    app.on_select_album(move |index| {
        let current = selection_generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(app) = ui.upgrade() {
            if let Some(name) = app.get_albums().row_data(index as usize) {
                app.set_selected(name);
            }
            app.set_photos(ModelRc::new(VecModel::<PhotoItem>::default()));
            app.set_can_next(false);
            app.set_busy(true);
            app.set_status("正在读取相册…".into());
        }
        let _ = tx.send(Command::Select(index as usize, current));
    });
    let tx = commands_tx.clone();
    let ui = weak.clone();
    app.on_next_page(move || {
        let current = generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(app) = ui.upgrade() {
            app.set_status("正在读取下一页…".into());
            app.set_busy(true);
        }
        let _ = tx.send(Command::Next(current));
    });
    let ui = weak.clone();
    app.on_download_album(move |dir| {
        if let Some(app) = ui.upgrade() {
            app.set_downloading(true);
        }
        status(&ui, "正在下载相册…".into(), true);
        let _ = commands_tx.send(Command::Download(PathBuf::from(dir.as_str())));
    });
    app.run()?;
    Ok(())
}

thread_local! { static COMMAND_RECEIVER: std::cell::RefCell<Option<(tokio::sync::mpsc::UnboundedReceiver<Command>, mpsc::Receiver<String>)>> = const { std::cell::RefCell::new(None) }; }

async fn worker(
    ui: slint::Weak<App>,
    username: String,
    region: String,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    secrets: mpsc::Receiver<String>,
    generation: Arc<AtomicU64>,
) -> Result<()> {
    let dir = directories::ProjectDirs::from("", "", "icloud-download-helper")
        .context("无法定位配置目录")?
        .config_dir()
        .join("sessions");
    let mut session = auth::Session::new(&username, &region, &dir)?;
    let account = session
        .login_with(false, true, |kind| {
            let code = matches!(kind, auth::AuthInput::Code);
            let _ = ui.upgrade_in_event_loop(move |app| {
                app.set_code_mode(code);
                app.set_needs_secret(true);
                app.set_status(
                    if code {
                        "请在受信任的设备上允许登录，然后输入验证码"
                    } else {
                        "请输入 Apple 账号密码"
                    }
                    .into(),
                );
            });
            let value = secrets.recv().context("窗口已关闭")?;
            let _ = ui.upgrade_in_event_loop(|app| {
                app.set_needs_secret(false);
                app.set_status("正在验证…".into());
            });
            Ok(value)
        })
        .await?;
    let photos = photos::Photos::new(&session, &account)?;
    photos.check_index().await?;
    let albums = photos.albums().await?;
    let names: Vec<SharedString> = albums.iter().map(|a| a.name.clone().into()).collect();
    let _ = ui.upgrade_in_event_loop(move |app| {
        app.set_albums(ModelRc::new(VecModel::from(names)));
        app.set_connected(true);
        app.set_busy(false);
        app.set_status("已连接。请选择相册。".into());
    });
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .build()?;
    let mut selected: Option<usize> = None;
    let mut offset = 0;
    let mut pending = None;
    loop {
        let command = match pending.take() {
            Some(command) => command,
            None => match commands.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            Command::Select(_, current) | Command::Next(current)
                if generation.load(Ordering::SeqCst) != current =>
            {
                continue;
            }
            Command::Select(index, current) => {
                if index >= albums.len() {
                    page_status(&ui, &generation, current, "无效的相册".into(), false);
                    continue;
                }
                selected = Some(index);
                offset = 0;
                tokio::select! {
                    next = commands.recv() => { if let Some(next) = next { pending = Some(next); } else { break; } }
                    result = show_page(&ui, &photos, &client, &albums[index], &mut offset, &generation, current) => {
                        if let Err(e) = result { page_status(&ui, &generation, current, format!("错误：{e:#}"), false); }
                    }
                }
            }
            Command::Next(current) => {
                let Some(index) = selected else {
                    page_status(&ui, &generation, current, "请选择相册".into(), false);
                    continue;
                };
                tokio::select! {
                    next = commands.recv() => { if let Some(next) = next { pending = Some(next); } else { break; } }
                    result = show_page(&ui, &photos, &client, &albums[index], &mut offset, &generation, current) => {
                        if let Err(e) = result { page_status(&ui, &generation, current, format!("错误：{e:#}"), false); }
                    }
                }
            }
            Command::Download(dir) => {
                let result = match selected {
                    Some(index) => download_album(&ui, &photos, &client, &albums[index], dir).await,
                    None => Err(anyhow::anyhow!("请选择相册")),
                };
                let _ = ui.upgrade_in_event_loop(|app| app.set_downloading(false));
                if let Err(e) = result {
                    status(&ui, format!("错误：{e:#}"), false);
                }
            }
        }
    }
    Ok(())
}

fn page_update(
    ui: &slint::Weak<App>,
    generation: &Arc<AtomicU64>,
    current: u64,
    update: impl FnOnce(App) + Send + 'static,
) {
    let generation = generation.clone();
    let _ = ui.upgrade_in_event_loop(move |app| {
        if generation.load(Ordering::SeqCst) == current {
            update(app);
        }
    });
}

fn page_status(
    ui: &slint::Weak<App>,
    generation: &Arc<AtomicU64>,
    current: u64,
    message: String,
    busy: bool,
) {
    page_update(ui, generation, current, move |app| {
        app.set_status(message.into());
        app.set_busy(busy);
    });
}

async fn show_page(
    ui: &slint::Weak<App>,
    photos: &photos::Photos<'_>,
    client: &reqwest::Client,
    album: &photos::Album,
    offset: &mut u64,
    generation: &Arc<AtomicU64>,
    current: u64,
) -> Result<()> {
    let page = photos.page(Some(album), *offset, false).await?;
    if page.empty {
        page_update(ui, generation, current, |app| {
            app.set_can_next(false);
            app.set_busy(false);
            app.set_status("已到相册末尾".into());
        });
        return Ok(());
    }
    *offset += page.masters as u64;
    let count = page.media.len();
    let name = album.name.clone();
    page_update(ui, generation, current, move |app| {
        app.set_selected(name.into());
        app.set_photos(ModelRc::new(VecModel::<PhotoItem>::default()));
        app.set_status(format!("正在加载 {count} 张预览…").into());
    });
    for media in page.media {
        let image = match media.preview_url {
            Some(url) => preview(client, &url).await.ok(),
            None => None,
        };
        let name = media.filename;
        page_update(ui, generation, current, move |app| {
            if let Some(model) = app
                .get_photos()
                .as_any()
                .downcast_ref::<VecModel<PhotoItem>>()
            {
                let image = image
                    .map(|(pixels, width, height)| {
                        slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(
                            &pixels, width, height,
                        ))
                    })
                    .unwrap_or_default();
                model.push(PhotoItem {
                    name: name.into(),
                    image,
                });
            }
        });
    }
    let position = *offset;
    page_update(ui, generation, current, move |app| {
        app.set_busy(false);
        app.set_can_next(true);
        app.set_status(format!("已读取 {position} 项；没有缩略图的项目仅显示文件名").into());
    });
    Ok(())
}

async fn preview(client: &reqwest::Client, url: &str) -> Result<(Vec<u8>, u32, u32)> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.without_url())?
        .error_for_status()
        .map_err(|e| e.without_url())?;
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.without_url())?;
        ensure!(bytes.len() + chunk.len() <= 8 * 1024 * 1024, "预览过大");
        bytes.extend_from_slice(&chunk);
    }
    let decoded = image::load_from_memory(&bytes)?
        .thumbnail(320, 320)
        .to_rgba8();
    let width = decoded.width();
    let height = decoded.height();
    Ok((decoded.into_raw(), width, height))
}

async fn download_album(
    ui: &slint::Weak<App>,
    photos: &photos::Photos<'_>,
    client: &reqwest::Client,
    album: &photos::Album,
    directory: PathBuf,
) -> Result<()> {
    ensure!(!directory.as_os_str().is_empty(), "请输入下载目录");
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".icloud-dl.lock"))?;
    lock.try_lock().context("另一个任务正在使用此下载目录")?;
    let dir = directory.join(format!(
        "{}_{}",
        download::safe_name(&album.name),
        download::identity(&album.id)
    ));
    let mut offset = 0;
    let mut seen = HashSet::new();
    let mut done = 0;
    let mut failed = 0;
    let mut last_error = String::new();
    loop {
        let page = photos.page(Some(album), offset, false).await?;
        if page.empty {
            break;
        }
        offset += page.masters as u64;
        let media: Vec<_> = page
            .media
            .into_iter()
            .filter(|m| seen.insert(m.id.clone()))
            .collect();
        ensure!(!media.is_empty(), "Apple 分页没有前进");
        for item in media {
            match download::download(client, &dir, &item, 3).await {
                Ok(_) => done += 1,
                Err(e) => {
                    failed += 1;
                    last_error = format!("；最近失败 {}：{e:#}", item.filename);
                }
            }
            status(
                ui,
                format!("下载中：完成或已存在 {done}，失败 {failed}"),
                true,
            );
        }
    }
    status(
        ui,
        format!(
            "下载结束：完成或已存在 {done}，失败 {failed}；目录：{}",
            dir.display()
        ) + &last_error,
        false,
    );
    Ok(())
}
