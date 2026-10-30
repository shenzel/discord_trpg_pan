#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use dpan::{
    controller::{self, Dual, DualShared, Side, Snapshot, Status, Timing, UiCommand, VoiceStatus},
    rpc,
    session::{Phase, Role},
    voice::{AccountVoice, NoopVoice, SdkHost, Voice, VoiceEvent, VoiceMsg},
};
use serde::Deserialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, State, WindowEvent,
};
use tokio::sync::mpsc;

/// メイン垢（必ずある）とサブ垢（2垢モードのときだけ）の制御。
struct Accounts {
    dir: PathBuf,
    base: controller::Config,
    host: Option<Arc<SdkHost>>,
    main_ui: mpsc::UnboundedSender<UiCommand>,
    main_voice: Arc<dyn Voice>,
    sub: Option<SubAccount>,
    /// ボイスのイベントの振り分け先（アカウント番号 → 制御）
    routes: Arc<Mutex<HashMap<usize, mpsc::UnboundedSender<VoiceEvent>>>>,
}

struct SubAccount {
    ui: mpsc::UnboundedSender<UiCommand>,
    voice: AccountVoice,
}

struct AppState {
    accounts: Mutex<Accounts>,
    /// 画面に出す状態（メイン、サブ）
    snaps: Mutex<(Snapshot, Option<Snapshot>)>,
}

const SUB_TOKEN: &str = "sdk_token_b.txt";

impl AppState {
    /// 役職・フェーズ・設定などは両方の制御に送る（それぞれ自分の VC の人だけに効く）
    fn broadcast(&self, cmd: UiCommand) {
        let acc = self.accounts.lock().unwrap();
        let _ = acc.main_ui.send(cmd.clone());
        if let Some(sub) = &acc.sub {
            let _ = sub.ui.send(cmd);
        }
    }

    fn merged(&self) -> Snapshot {
        let snaps = self.snaps.lock().unwrap();
        let mut s = snaps.0.clone();
        s.sub = snaps.1.clone().map(Box::new);
        s
    }
}

#[tauri::command]
fn get_state(state: State<'_, AppState>) -> Snapshot {
    state.merged()
}

#[tauri::command]
fn set_phase(phase: Phase, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetPhase(phase));
}

#[tauri::command]
fn set_role(user_id: String, role: Role, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetRole(user_id, role));
}

#[tauri::command]
fn reorder(user_id: String, index: usize, state: State<'_, AppState>) {
    state.broadcast(UiCommand::Reorder(user_id, index));
}

#[tauri::command]
fn reset_scenario(state: State<'_, AppState>) {
    state.broadcast(UiCommand::Reset);
}

#[tauri::command]
fn set_gm_mode(on: bool, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetGmMode(on));
}

/// ログインし直す（side: "main" | "sub"）
#[tauri::command]
fn relogin(side: Option<String>, state: State<'_, AppState>) {
    let acc = state.accounts.lock().unwrap();
    if side.as_deref() == Some("sub") {
        if let Some(sub) = &acc.sub {
            let _ = sub.ui.send(UiCommand::Relogin);
        }
    } else {
        let _ = acc.main_ui.send(UiCommand::Relogin);
    }
}

/// ロビー通話のマイク。None は Discord と同じ（自動）
#[tauri::command]
fn set_input_device(id: Option<String>, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetInputDevice(id));
}

#[tauri::command]
fn set_output_device(id: Option<String>, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetOutputDevice(id));
}

#[tauri::command]
fn refresh_devices(state: State<'_, AppState>) {
    state.broadcast(UiCommand::RefreshDevices);
}

/// 自分のミュート（2垢モードでは両方の Discord）
#[tauri::command]
fn set_self_mute(on: bool, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetSelfMute(on));
}

/// GM が他の人にミュート／解除を指示する
#[tauri::command]
fn request_mute(user_id: String, on: bool, state: State<'_, AppState>) {
    state.broadcast(UiCommand::RequestMute(user_id, on));
}

#[tauri::command]
fn set_gap(gap: f64, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetGap(gap));
}

#[tauri::command]
fn set_span(lo: f64, hi: f64, state: State<'_, AppState>) {
    state.broadcast(UiCommand::SetSpan(lo, hi));
}

/// サブ垢を追加する（ブラウザか Discord で、サブ垢として許可する）
#[tauri::command]
fn add_sub_account(app: AppHandle, state: State<'_, AppState>) {
    let mut acc = state.accounts.lock().unwrap();
    start_sub(&app, &mut acc);
}

/// サブ垢を外す（保存したトークンも消す）
#[tauri::command]
fn remove_sub_account(app: AppHandle, state: State<'_, AppState>) {
    {
        let mut acc = state.accounts.lock().unwrap();
        let Some(sub) = acc.sub.take() else { return };
        log::info!("サブ垢を外す");
        acc.routes.lock().unwrap().remove(&sub.voice.index());
        sub.voice.remove();
        // ui を落とすと制御が終わり、Discord との接続も閉じる（Discord が音量を元に戻す）
        drop(sub.ui);
        let _ = std::fs::remove_file(acc.dir.join(SUB_TOKEN));
        let mut d = acc.base.dual.lock().unwrap();
        d.sub_active = false;
        d.seated[1] = 0;
    }
    state.snaps.lock().unwrap().1 = None;
    emit(&app);
}

/// アプリを終える：Discord 側の音量とマイク入力を戻し、通話を終えてから終了する（トレイにも残さない）
#[tauri::command]
async fn quit_app(app: AppHandle) {
    quit_and_restore(app).await;
}

async fn quit_and_restore(app: AppHandle) {
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
    let n = {
        let state = app.state::<AppState>();
        let acc = state.accounts.lock().unwrap();
        let mut n = 0;
        if acc.main_ui.send(UiCommand::Shutdown(ack_tx.clone())).is_ok() {
            n += 1;
        }
        if let Some(sub) = &acc.sub {
            if sub.ui.send(UiCommand::Shutdown(ack_tx.clone())).is_ok() {
                n += 1;
            }
        }
        n
    };
    // 戻し終わるのを待つ（最大 2 秒）
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        for _ in 0..n {
            ack_rx.recv().await;
        }
    })
    .await;
    // ブリッジが EndCall を送り切る時間
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    app.exit(0);
}

/// ログのフォルダをエクスプローラーで開く（不具合の報告用）
#[tauri::command]
fn open_log_dir(app: AppHandle) {
    if let Ok(dir) = app.path().app_log_dir() {
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }
}

/// `%APPDATA%\<identifier>\config.json`（任意）。ビルド時の環境変数より優先する。
#[derive(Deserialize, Default)]
struct FileConfig {
    client_id: Option<String>,
}

fn load_config(dir: &Path, dual: Dual) -> controller::Config {
    let file: FileConfig = std::fs::read_to_string(dir.join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let client_id = file
        .client_id
        .or(std::env::var("DPAN_CLIENT_ID").ok().filter(|s| !s.is_empty()))
        .or(option_env!("DPAN_CLIENT_ID").map(String::from))
        .unwrap_or_default();
    controller::Config {
        client_id,
        pipe_prefix: rpc::PIPE_PREFIX.to_string(),
        timing: Timing::default(),
        settings_path: Some(dir.join("settings.json")),
        restore_path: Some(dir.join("discord_restore.json")),
        side: Side::Main,
        dual,
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn tray_tooltip(s: &Snapshot) -> String {
    let state = match &s.status {
        Status::Setup => "client_idが未設定です".to_string(),
        Status::Waiting => "Discordの起動を待っています".to_string(),
        Status::Authorizing => "ブラウザかDiscordで許可してください".to_string(),
        Status::Error(e) => format!("エラー: {e}"),
        Status::Connected => match &s.channel {
            Some(c) => format!("{}（{}人）", c.name, s.members.len()),
            None => "VCに参加していません".to_string(),
        },
    };
    let state = match &s.voice {
        VoiceStatus::InCall => state,
        VoiceStatus::Error(_) => format!("{state} ／ ロビー通話に入れません"),
        VoiceStatus::Authorizing => format!("{state} ／ ロビー通話の許可待ち"),
        _ => state,
    };
    let state = match &s.sub {
        Some(sub) => format!("{state} ＋ サブ: {}", sub.channel.as_ref().map_or("VC なし", |c| c.name.as_str())),
        None => state,
    };
    let mut t = format!("Discord自動pan - {state}");
    // トレイのツールチップは128文字まで
    if t.chars().count() > 120 {
        t = t.chars().take(119).collect::<String>() + "…";
    }
    t
}

fn emit(app: &AppHandle) {
    let snap = app.state::<AppState>().merged();
    let _ = app.emit("state", &snap);
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(tray_tooltip(&snap)));
    }
}

fn sink(app: &AppHandle, side: Side) -> controller::Sink {
    let handle = app.clone();
    Box::new(move |snap: &Snapshot| {
        {
            let state = handle.state::<AppState>();
            let mut snaps = state.snaps.lock().unwrap();
            match side {
                Side::Main => snaps.0 = snap.clone(),
                Side::Sub => snaps.1 = Some(snap.clone()),
            }
        }
        emit(&handle);
    })
}

/// サブ垢の制御を始める（保存したトークンがなければ許可から）。
fn start_sub(app: &AppHandle, acc: &mut Accounts) {
    if acc.sub.is_some() {
        return;
    }
    let Some(host) = acc.host.clone() else { return };
    log::info!("サブ垢を追加する");
    let voice = host.add_account(&acc.dir.join(SUB_TOKEN));
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    acc.routes.lock().unwrap().insert(voice.index(), ev_tx);
    acc.base.dual.lock().unwrap().sub_active = true;
    let cfg = controller::Config {
        side: Side::Sub,
        restore_path: Some(acc.dir.join("discord_restore_sub.json")),
        ..acc.base.clone()
    };
    tauri::async_runtime::spawn(controller::run(cfg, ui_rx, sink(app, Side::Sub), Arc::new(voice.clone()), ev_rx));
    acc.sub = Some(SubAccount { ui: ui_tx, voice });
}

fn main() {
    // 開発ビルドだけ .env を読む（配布ビルドは secret を読み込む経路を持たない）
    #[cfg(debug_assertions)]
    let _ = dotenvy::dotenv();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_main(app)))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                // 切り分け用に多めに残す（既定の 40KB だと 1 分ほどで上書きされる）
                .max_file_size(5_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_phase,
            set_role,
            reorder,
            reset_scenario,
            set_gm_mode,
            relogin,
            set_input_device,
            set_output_device,
            refresh_devices,
            set_gap,
            set_span,
            set_self_mute,
            request_mute,
            add_sub_account,
            remove_sub_account,
            quit_app,
            open_log_dir
        ])
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let dual: Dual = Arc::new(Mutex::new(DualShared::default()));
            let cfg = load_config(&dir, dual);
            log::info!(
                "起動: バージョン {} / client_id {}",
                app.package_info().version,
                if cfg.client_id.is_empty() { "未設定" } else { "設定済み" }
            );

            let show = MenuItem::with_id(app, "show", "表示", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "終了", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("Discord自動pan")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        tauri::async_runtime::spawn(quit_and_restore(app.clone()));
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // Social SDK（プロセスに 1 つ）とメイン垢
            let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<VoiceMsg>();
            let routes: Arc<Mutex<HashMap<usize, mpsc::UnboundedSender<VoiceEvent>>>> = Default::default();
            let (main_ev_tx, main_ev_rx) = mpsc::unbounded_channel();
            let (host, main_voice): (Option<Arc<SdkHost>>, Arc<dyn Voice>) = match cfg.client_id.parse::<u64>() {
                Ok(app_id) => {
                    let host = SdkHost::start(app_id, msg_tx);
                    let voice = host.add_account(&dir.join("sdk_token.txt"));
                    routes.lock().unwrap().insert(voice.index(), main_ev_tx);
                    (Some(host), Arc::new(voice))
                }
                Err(_) => (None, Arc::new(NoopVoice)),
            };
            // ボイスのイベントをアカウントごとの制御へ振り分ける
            let r = routes.clone();
            tauri::async_runtime::spawn(async move {
                while let Some(m) = msg_rx.recv().await {
                    if let Some(tx) = r.lock().unwrap().get(&m.account) {
                        let _ = tx.send(m.event);
                    }
                }
            });

            let (main_ui, main_ui_rx) = mpsc::unbounded_channel();
            app.manage(AppState {
                accounts: Mutex::new(Accounts {
                    dir: dir.clone(),
                    base: cfg.clone(),
                    host,
                    main_ui,
                    main_voice: main_voice.clone(),
                    sub: None,
                    routes,
                }),
                snaps: Mutex::new((Snapshot::default(), None)),
            });
            tauri::async_runtime::spawn(controller::run(cfg, main_ui_rx, sink(app.handle(), Side::Main), main_voice, main_ev_rx));

            // 前回サブ垢を使っていたら、続けて使う
            if dir.join(SUB_TOKEN).exists() {
                let state = app.state::<AppState>();
                let mut acc = state.accounts.lock().unwrap();
                start_sub(app.handle(), &mut acc);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // ×で閉じてもトレイに常駐する
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // 通話を終えてから終了する（ロビーのメンバーのまま）
                let state = app.state::<AppState>();
                let acc = state.accounts.lock().unwrap();
                acc.main_voice.leave_call();
                if let Some(sub) = &acc.sub {
                    sub.voice.leave_call();
                }
                drop(acc);
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        });
}
