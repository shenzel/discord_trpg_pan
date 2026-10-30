//! 制御ループ。Discord 本体（RPC）とロビー通話（Voice）をつなぎ、配置を話者ごとの左右の倍率にして渡す。
//! Tauri に依存せず、状態は `Snapshot` として外に渡す。
//!
//! - RPC：今いる VC と参加者、Discord 側のマイクとスピーカーの設定。通話中は Discord 側のマイク入力を 0、
//!   ロビーにいる人の Discord 側の音量を 0 にして、声が二重に聞こえないようにする（切断すると Discord が元に戻す）
//! - Voice：VC ごとのロビー「dpan:<channel_id>」で通話し、受信音声に倍率を掛ける
//! - 同期：GM として卓を開いた人のメンバーメタデータ（役職とフェーズ）を、他の人のアプリが読む

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use crate::{
    animator::Animator,
    rpc::{self, parse_voice_member, ConnectFailure, Connection, Event, RpcClient, User},
    session::{Channel, Phase, Presence, Role, Session},
    voice::{match_device, Device, Voice, VoiceEvent},
};

const VOICE_EVENTS: [&str; 3] = ["VOICE_STATE_CREATE", "VOICE_STATE_UPDATE", "VOICE_STATE_DELETE"];
pub const LOBBY_PREFIX: &str = "dpan:";

#[derive(Clone, Debug)]
pub struct Timing {
    /// 他の参加者の再入室の猶予
    pub grace: Duration,
    /// ゆっくり移動の所要時間
    pub tween: Duration,
    /// 倍率を更新する間隔
    pub tick: Duration,
    /// Discordへの再接続間隔
    pub reconnect: Duration,
    /// 認証失敗後に再試行するまで
    pub auth_retry: Duration,
    /// GM の変更をまとめて書き込むまでの待ち（メタデータの書き込みは回数枠を使う）
    pub sync_debounce: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            tween: Duration::from_secs(3),
            tick: Duration::from_millis(20),
            reconnect: Duration::from_secs(3),
            auth_retry: Duration::from_secs(30),
            sync_debounce: Duration::from_millis(1500),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub client_id: String,
    pub pipe_prefix: String,
    pub timing: Timing,
    /// 設定（マイク・スピーカーの選択）の保存先。None なら保存しない
    pub settings_path: Option<PathBuf>,
    /// Discord 側で 0 にした音量の元の値の保存先（次に起動したとき Discord がかけ直す 0 を戻すため）
    pub restore_path: Option<PathBuf>,
    /// メイン垢かサブ垢か（2垢モード）
    pub side: Side,
    /// メインとサブで共有する状態（1 アカウントのときもメインだけで使う）
    pub dual: Dual,
}

/// 2垢モードのどちら側か。メインは左、サブは右を受け持つ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[default]
    Main,
    Sub,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Side::Main => 0,
            Side::Sub => 1,
        }
    }
}

/// メインとサブの制御が共有する状態。役職は VC を移っても引き継ぐ。
#[derive(Debug)]
pub struct DualShared {
    pub roles: HashMap<String, Role>,
    pub roles_rev: u64,
    /// 各側の席の数（空席を含む）。両方に席があれば左右に分ける
    pub seated: [usize; 2],
    /// GM 本人のアカウント（両側の一覧と席から外す）
    pub self_ids: HashSet<String>,
    /// 2 分割のときの中央の隙間の半幅 g（メイン −1〜−g、サブ +g〜+1）
    pub gap: f64,
    /// 席を並べる範囲の L 端と R 端（既定は −1〜+1）。聞く人の好みで狭める
    pub span: (f64, f64),
    /// サブ垢が動いているか
    pub sub_active: bool,
}

impl Default for DualShared {
    fn default() -> Self {
        Self {
            roles: HashMap::new(),
            roles_rev: 0,
            seated: [0, 0],
            self_ids: HashSet::new(),
            gap: DEFAULT_GAP,
            span: DEFAULT_SPAN,
            sub_active: false,
        }
    }
}

pub type Dual = Arc<Mutex<DualShared>>;

pub const DEFAULT_GAP: f64 = 0.2;
pub const DEFAULT_SPAN: (f64, f64) = (-1.0, 1.0);

fn default_gap() -> f64 {
    DEFAULT_GAP
}

fn default_span() -> (f64, f64) {
    DEFAULT_SPAN
}

/// 席を並べる範囲。2 分割ならメイン −1〜−g・サブ +g〜+1、それを L 端〜R 端（span）の中に縮める。
fn seat_range(split: bool, side: Side, gap: f64, span: (f64, f64)) -> (f64, f64) {
    let (lo, hi) = match (split, side) {
        (false, _) => (-1.0, 1.0),
        (true, Side::Main) => (-1.0, -gap),
        (true, Side::Sub) => (gap, 1.0),
    };
    let map = |p: f64| span.0 + (p + 1.0) / 2.0 * (span.1 - span.0);
    (map(lo), map(hi))
}

/// Discord 側で 0 にしたものと、その元の値（ファイルに残す）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Restore {
    #[serde(default)]
    volumes: HashMap<String, f64>,
    #[serde(default)]
    input_volume: Option<f64>,
}

/// ロビー通話で使うデバイスの指定。None は「Discord と同じ（自動）」。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSettings {
    #[serde(default)]
    pub input_device: Option<DeviceRef>,
    #[serde(default)]
    pub output_device: Option<DeviceRef>,
    /// 2垢モードの隙間の半幅 g
    #[serde(default = "default_gap")]
    pub gap: f64,
    /// 席を並べる範囲の L 端と R 端
    #[serde(default = "default_span")]
    pub span: (f64, f64),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceRef {
    pub id: String,
    pub name: String,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { input_device: None, output_device: None, gap: DEFAULT_GAP, span: DEFAULT_SPAN }
    }
}

impl AudioSettings {
    fn load(path: &Option<PathBuf>) -> Self {
        path.as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Option<PathBuf>) {
        let Some(p) = path else { return };
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(p, s) {
                    log::warn!("設定を保存できません（{}）: {e}", p.display());
                }
            }
            Err(e) => log::warn!("設定を保存できません: {e}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum Status {
    /// client_id 未設定
    Setup,
    /// Discordの起動待ち
    Waiting,
    /// 許可待ち（ロビー通話のログインで取るトークンを、Discord 本体のログインにも使う）
    Authorizing,
    Error(String),
    Connected,
}

/// ロビー通話の状態。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum VoiceStatus {
    Starting,
    /// ブラウザか Discord で許可待ち
    Authorizing,
    /// ログイン済み・VC に入っていない
    Idle,
    /// ロビーに参加して通話をつないでいる途中
    Joining,
    InCall,
    /// 失敗。普段の Discord の通話に戻している
    Error(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct UserView {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberView {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// "present" | "grace" | "away"
    pub presence: &'static str,
    pub grace_secs: Option<u64>,
    /// 席の番号（数える席の中で）。席なしは None。
    pub seat: Option<usize>,
    pub position: Option<f64>,
    /// このアプリでロビー通話に入っている（左右に振れる）か
    pub in_lobby: bool,
    pub speaking: bool,
    /// 自分の行（席は持たない。役職だけ選べる）
    pub is_self: bool,
    /// Discord でミュート中か
    pub muted: bool,
}

/// 設定画面に出すマイクとスピーカー。
#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioView {
    pub inputs: Vec<Device>,
    pub outputs: Vec<Device>,
    /// ロビー通話が今使っているデバイスの ID
    pub current_input: String,
    pub current_output: String,
    /// 指定しているデバイスの ID（None は Discord と同じ）
    pub input_choice: Option<String>,
    pub output_choice: Option<String>,
    /// Discord で選んでいるデバイスの名前
    pub discord_input: Option<String>,
    pub discord_output: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub status: Status,
    pub voice: VoiceStatus,
    pub self_user: Option<UserView>,
    pub channel: Option<Channel>,
    pub phase: Phase,
    pub members: Vec<MemberView>,
    pub seat_count: usize,
    /// 自分が GM として卓を開いているか
    pub gm_mode: bool,
    /// 他の人が開いた卓に参加中なら、その GM
    pub remote_gm: Option<UserView>,
    /// 役職とフェーズを GM が決めている（自分では変えられない）
    pub read_only: bool,
    /// 注意（GM が複数いる、アカウントが違う、デバイスが見つからない など）
    pub warnings: Vec<String>,
    pub audio: AudioView,
    pub side: Side,
    /// 自分が Discord でミュート中か
    pub self_muted: bool,
    /// 2垢モードの隙間の半幅 g
    pub gap: f64,
    /// 席を並べる範囲の L 端と R 端
    pub span: (f64, f64),
    /// 2垢モードのサブ側（画面に出すときにメイン側へ付ける）
    pub sub: Option<Box<Snapshot>>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Waiting,
            voice: VoiceStatus::Starting,
            self_user: None,
            channel: None,
            phase: Phase::Before,
            members: vec![],
            seat_count: 0,
            gm_mode: false,
            remote_gm: None,
            read_only: false,
            warnings: vec![],
            audio: AudioView::default(),
            side: Side::Main,
            self_muted: false,
            gap: DEFAULT_GAP,
            span: DEFAULT_SPAN,
            sub: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum UiCommand {
    SetPhase(Phase),
    SetRole(String, Role),
    Reorder(String, usize),
    Reset,
    SetGmMode(bool),
    /// 保存したトークンを捨てて、許可からやり直す（アカウントを替えるとき）
    Relogin,
    /// ロビー通話のマイク・スピーカー。None は Discord と同じ（自動）
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),
    RefreshDevices,
    /// 2垢モードの隙間の半幅 g
    SetGap(f64),
    /// 席を並べる範囲の L 端と R 端（L・R をドラッグ）
    SetSpan(f64, f64),
    /// 自分の Discord のミュート（斜線のマイクアイコン）
    SetSelfMute(bool),
    /// GM が他の人にミュート／解除を指示する（相手のアプリが 1 回だけ従う）
    RequestMute(String, bool),
    /// アプリを終える：Discord 側の音量とマイク入力を戻し、通話を終えてから ack に知らせ、制御を止める
    Shutdown(mpsc::UnboundedSender<()>),
}

enum ConnMsg {
    Status(Status),
    /// Discord 本体にログインできなかった
    AuthFailed(String),
    Connected { client: RpcClient, user: User, events: mpsc::UnboundedReceiver<Event> },
}

pub type Sink = Box<dyn Fn(&Snapshot) + Send + Sync>;

/// 制御ループ本体。`ui` が閉じたら終わる。
pub async fn run(
    cfg: Config,
    mut ui: mpsc::UnboundedReceiver<UiCommand>,
    sink: Sink,
    voice: Arc<dyn Voice>,
    mut voice_events: mpsc::UnboundedReceiver<VoiceEvent>,
) {
    let cfg = Arc::new(cfg);
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
    let (token_tx, token_rx) = watch::channel(Login::default());
    let mut ctl = Controller::new(&cfg, sink, voice, token_tx);
    if cfg.client_id.is_empty() {
        ctl.status = Status::Setup;
    } else {
        tokio::spawn(connector(cfg.clone(), conn_tx, token_rx));
    }
    let mut ticker = tokio::time::interval(cfg.timing.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            Some(msg) = conn_rx.recv() => ctl.on_conn(msg).await,
            ev = next_event(&mut ctl.events) => match ev {
                Some(ev) => ctl.on_event(ev).await,
                None => ctl.on_disconnect(),
            },
            Some(ev) = voice_events.recv() => ctl.on_voice(ev),
            cmd = ui.recv() => match cmd {
                Some(UiCommand::Shutdown(ack)) => {
                    ctl.shutdown().await;
                    let _ = ack.send(());
                    break;
                }
                Some(cmd) => ctl.on_ui(cmd),
                None => break,
            },
            _ = ticker.tick() => ctl.on_tick(),
        }
        ctl.reconcile_discord();
        ctl.flush();
    }
}

async fn next_event(rx: &mut Option<mpsc::UnboundedReceiver<Event>>) -> Option<Event> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// ロビー通話のログインで取ったトークンと、そのアカウント。そろったら Discord 本体にもログインする。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Login {
    token: Option<String>,
    user_id: Option<String>,
    user_name: String,
}

/// パイプへの接続と認証を繰り返す。つながったら Controller に渡し、切れるまで待つ。
///
/// 認証には、ロビー通話（Social SDK）のログインで取ったトークンを使う。RPC の AUTHORIZE で取った code は
/// client_secret なしでは交換できないが、Social SDK のログインは PKCE なので secret が要らない。
/// つなぐのは、ロビー通話と同じアカウントでログインしている Discord だけ（Stable と PTB を取り違えない）。
async fn connector(cfg: Arc<Config>, tx: mpsc::UnboundedSender<ConnMsg>, mut login: watch::Receiver<Login>) {
    while !tx.is_closed() {
        // ロビー通話のログインが済むまで待つ
        let (access, user_id, user_name) = loop {
            let l = login.borrow_and_update().clone();
            if let (Some(t), Some(u)) = (l.token, l.user_id) {
                break (t, u, l.user_name);
            }
            let _ = tx.send(ConnMsg::Status(Status::Authorizing));
            if login.changed().await.is_err() {
                return;
            }
        };
        let conn = match rpc::connect_user(&cfg.pipe_prefix, &cfg.client_id, &user_id).await {
            Ok(c) => c,
            Err(ConnectFailure::NotRunning) => {
                let _ = tx.send(ConnMsg::Status(Status::Waiting));
                tokio::time::sleep(cfg.timing.reconnect).await;
                continue;
            }
            Err(ConnectFailure::OtherUsers(names)) => {
                log::warn!("「{user_name}」でログインしている Discord がありません（起動中: {names:?}）");
                let _ = tx.send(ConnMsg::AuthFailed(format!(
                    "Discord 本体に「{user_name}」でログインしているものが見つかりません（起動中：{}）。Discord のアカウントを切り替えるか、「ログインし直す」で Discord 本体と同じアカウントにしてください",
                    names.join("、")
                )));
                tokio::select! {
                    _ = tokio::time::sleep(cfg.timing.auth_retry) => {}
                    _ = login.changed() => {}
                }
                continue;
            }
            Err(ConnectFailure::Failed(e)) => {
                log::warn!("Discordに接続できません: {e:#}");
                let _ = tx.send(ConnMsg::Status(Status::Error(format!("{e:#}"))));
                tokio::time::sleep(cfg.timing.auth_retry).await;
                continue;
            }
        };
        let Connection { client, user, events, mut reader, writer } = conn;
        log::info!("Discordに接続: {} ({})", user.display_name(), user.id);
        match client.command("AUTHENTICATE", json!({ "access_token": access })).await {
            Ok(_) => {
                let _ = tx.send(ConnMsg::Connected { client, user, events });
                // 切れるまで待つ。制御が終わったら（サブ垢を外した）こちらから閉じて、Discord に設定を戻させる
                tokio::select! {
                    _ = &mut reader => {}
                    _ = tx.closed() => reader.abort(),
                }
                writer.abort();
                if tx.is_closed() {
                    return;
                }
            }
            Err(e) => {
                reader.abort();
                writer.abort();
                log::warn!("Discord本体にログインできません: {e:#}");
                let _ = tx.send(ConnMsg::AuthFailed(format!(
                    "Discord本体（{}）にログインできません: {e:#}。このアカウントが App Testers に登録・承認されているかを確認し、「ログインし直す」を押してください",
                    user.display_name()
                )));
                // 新しいトークンが届くまで待つ（ログインし直す・期限の更新）
                if login.changed().await.is_err() {
                    return;
                }
                continue;
            }
        }
        tokio::time::sleep(cfg.timing.reconnect).await;
    }
}

/// Discord 本体の音声設定（RPC の GET_VOICE_SETTINGS / VOICE_SETTINGS_UPDATE）。
#[derive(Clone, Debug, Default, PartialEq)]
struct DiscordVoice {
    input_id: String,
    input_name: Option<String>,
    input_volume: f64,
    output_id: String,
    output_name: Option<String>,
    mute: bool,
    deaf: bool,
}

impl DiscordVoice {
    fn parse(v: &Value) -> Self {
        let name_of = |io: &Value| -> Option<String> {
            let id = io["device_id"].as_str()?;
            io["available_devices"].as_array()?.iter().find(|d| d["id"] == id)?["name"].as_str().map(String::from)
        };
        Self {
            input_id: v["input"]["device_id"].as_str().unwrap_or_default().to_string(),
            input_name: name_of(&v["input"]),
            input_volume: v["input"]["volume"].as_f64().unwrap_or(100.0),
            output_id: v["output"]["device_id"].as_str().unwrap_or_default().to_string(),
            output_name: name_of(&v["output"]),
            mute: v["mute"].as_bool().unwrap_or(false),
            deaf: v["deaf"].as_bool().unwrap_or(false),
        }
    }
}

/// GM が配る卓の状態（メンバーメタデータ）。
#[derive(Clone, Debug, PartialEq)]
struct TableState {
    phase: Phase,
    roles: HashMap<String, Role>,
    /// GM からのミュートの指示（user_id → (番号, ミュートするか)）。番号が新しくなったときだけ 1 回従う
    mutes: HashMap<String, (u64, bool)>,
}

fn table_metadata(name: &str, gm: Option<&TableState>) -> Vec<(String, String)> {
    let mut m = vec![("name".to_string(), name.to_string())];
    if let Some(t) = gm {
        let roles: serde_json::Map<String, Value> =
            t.roles.iter().map(|(id, r)| (id.clone(), Value::from(r.as_str()))).collect();
        m.push(("gm".into(), "1".into()));
        m.push(("phase".into(), t.phase.as_str().into()));
        m.push(("roles".into(), Value::Object(roles).to_string()));
        if !t.mutes.is_empty() {
            let mutes: serde_json::Map<String, Value> =
                t.mutes.iter().map(|(id, (seq, on))| (id.clone(), json!([seq, on]))).collect();
            m.push(("mutes".into(), Value::Object(mutes).to_string()));
        }
    }
    m
}

fn parse_table(meta: &HashMap<String, String>) -> Option<TableState> {
    if meta.get("gm").map(String::as_str) != Some("1") {
        return None;
    }
    let phase = Phase::parse(meta.get("phase")?)?;
    let roles = serde_json::from_str::<HashMap<String, String>>(meta.get("roles").map(String::as_str).unwrap_or("{}"))
        .ok()?
        .into_iter()
        .filter_map(|(id, r)| Role::parse(&r).map(|r| (id, r)))
        .collect();
    let mutes = meta
        .get("mutes")
        .and_then(|s| serde_json::from_str::<HashMap<String, (u64, bool)>>(s).ok())
        .unwrap_or_default();
    Some(TableState { phase, roles, mutes })
}

#[derive(Clone, Debug)]
struct LobbyMember {
    name: String,
    connected: bool,
    table: Option<TableState>,
}

struct Controller {
    session: Session,
    animator: Animator,
    timing: Timing,
    status: Status,
    self_user: Option<User>,
    client: Option<RpcClient>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
    subscribed: Option<String>,
    sink: Sink,
    dirty: bool,
    last_grace_emit: Instant,

    voice: Arc<dyn Voice>,
    rpc_token: watch::Sender<Login>,
    voice_status: VoiceStatus,
    voice_user: Option<String>,
    secret: Option<String>,
    /// 今の通話のロビー（CallStarted で決まる）。これ以外のロビーの通話イベントは無視する
    current_lobby: Option<String>,
    call_connected: bool,
    lobby_participants: HashSet<String>,
    lobby_members: HashMap<String, LobbyMember>,
    speaking: HashSet<String>,
    sdk_devices: Option<(Vec<Device>, Vec<Device>, String, String)>,
    device_warning: Option<String>,

    discord_voice: Option<DiscordVoice>,
    /// 元の Discord 側の音量（VC の参加者ごと）
    discord_volumes: HashMap<String, f64>,
    /// こちらで音量 0 にしている人と、元の音量
    zeroed: HashMap<String, f64>,
    /// こちらでマイク入力を 0 にしているなら、元の入力音量
    input_zeroed: Option<f64>,

    gm_mode: bool,
    sync_due: Option<Instant>,
    applied_table: Option<TableState>,
    /// 自分の役職（自分は席を持たないが、GM が配る役職表と一覧に出す）
    self_role: Role,

    settings: AudioSettings,
    settings_path: Option<PathBuf>,
    last_layout_log: String,

    side: Side,
    dual: Dual,
    seen_roles_rev: u64,
    restore_path: Option<PathBuf>,

    /// VC の各人が Discord でミュート中か
    muted: HashMap<String, bool>,
    /// GM として出したミュートの指示
    mute_requests: HashMap<String, (u64, bool)>,
    /// GM の卓から受け取って処理済みの、自分へのミュートの指示の番号
    seen_mute_seq: Option<u64>,
}

impl Controller {
    fn new(cfg: &Config, sink: Sink, voice: Arc<dyn Voice>, rpc_token: watch::Sender<Login>) -> Self {
        Self {
            session: Session::new(cfg.timing.grace),
            animator: Animator::new(cfg.timing.tween),
            timing: cfg.timing.clone(),
            status: Status::Waiting,
            self_user: None,
            client: None,
            events: None,
            subscribed: None,
            sink,
            dirty: true,
            last_grace_emit: Instant::now(),
            voice,
            rpc_token,
            voice_status: VoiceStatus::Starting,
            voice_user: None,
            secret: None,
            current_lobby: None,
            call_connected: false,
            lobby_participants: HashSet::new(),
            lobby_members: HashMap::new(),
            speaking: HashSet::new(),
            sdk_devices: None,
            device_warning: None,
            discord_voice: None,
            discord_volumes: HashMap::new(),
            zeroed: HashMap::new(),
            input_zeroed: None,
            gm_mode: false,
            sync_due: None,
            applied_table: None,
            self_role: Role::Pl,
            settings: AudioSettings::load(&cfg.settings_path),
            settings_path: cfg.settings_path.clone(),
            last_layout_log: String::new(),
            side: cfg.side,
            dual: cfg.dual.clone(),
            seen_roles_rev: u64::MAX,
            restore_path: cfg.restore_path.clone(),
            muted: HashMap::new(),
            mute_requests: HashMap::new(),
            seen_mute_seq: None,
        }
    }

    /// 自分の Discord をミュート／解除する（Discord のミュートはロビー通話にも連動する）
    fn set_discord_mute(&mut self, mute: bool) {
        let Some(client) = self.client.clone() else { return };
        log::info!("Discord のミュートを {}", if mute { "オン" } else { "オフ" });
        fire(&client, "SET_VOICE_SETTINGS", json!({ "mute": mute }));
    }

    /// GM 本人のアカウントか（両側の一覧と席から外す）
    fn is_self_account(&self, id: &str) -> bool {
        Some(id) == self.self_id() || self.dual.lock().unwrap().self_ids.contains(id)
    }

    // ---------- Discord 側の音量の復元 ----------

    fn save_restore(&self) {
        let Some(p) = &self.restore_path else { return };
        let r = Restore { volumes: self.zeroed.clone(), input_volume: self.input_zeroed };
        if let Err(e) = serde_json::to_string(&r).map_err(anyhow::Error::from).and_then(|s| Ok(std::fs::write(p, s)?)) {
            log::warn!("音量の元の値を保存できません: {e:#}");
        }
    }

    /// 終了の前に、Discord 側で 0 にした音量とマイク入力を戻す（応答を待つ）。通話も終える。
    async fn shutdown(&mut self) {
        log::info!("終了：Discord 側の音量とマイク入力を戻す");
        if let Some(client) = self.client.clone() {
            let mut waits = vec![];
            for (id, v) in self.zeroed.drain() {
                log::info!("Discord 側の音量を戻す: {id} → {v}");
                waits.push(client.send("SET_USER_VOICE_SETTINGS", json!({ "user_id": id, "volume": v.round() }), None));
            }
            if let Some(original) = self.input_zeroed.take() {
                log::info!("Discord 側のマイク入力を戻す: {original}");
                waits.push(client.send("SET_VOICE_SETTINGS", json!({ "input": { "volume": original } }), None));
            }
            for w in waits {
                let _ = tokio::time::timeout(Duration::from_millis(1500), w).await;
            }
            self.save_restore();
        }
        self.voice.leave_call();
    }

    /// 前回 0 にしたもの。Discord は同じアプリがつなぎ直すと前回の設定をかけ直すので、つないだら必要な分だけ戻す
    fn load_restore(&mut self) {
        let Some(p) = &self.restore_path else { return };
        let Some(r) = std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<Restore>(&s).ok()) else {
            return;
        };
        if !r.volumes.is_empty() || r.input_volume.is_some() {
            log::info!("前回 0 にした Discord 側の設定を引き継ぐ（必要なければ戻す）: {r:?}");
        }
        for (id, v) in &r.volumes {
            self.discord_volumes.insert(id.clone(), *v);
        }
        self.zeroed = r.volumes;
        self.input_zeroed = r.input_volume;
    }

    fn self_id(&self) -> Option<&str> {
        self.self_user.as_ref().map(|u| u.id.as_str())
    }

    fn self_name(&self) -> String {
        self.self_user.as_ref().map(|u| u.display_name().to_string()).unwrap_or_default()
    }

    // ---------- RPC ----------

    async fn on_conn(&mut self, msg: ConnMsg) {
        match msg {
            ConnMsg::Status(s) => {
                if self.client.is_none() {
                    self.status = s;
                    self.dirty = true;
                }
            }
            ConnMsg::AuthFailed(message) => {
                self.status = Status::Error(message);
                self.dirty = true;
            }
            ConnMsg::Connected { client, user, events } => {
                self.load_restore();
                self.client = Some(client.clone());
                self.events = Some(events);
                self.self_user = Some(user);
                self.subscribed = None;
                self.status = Status::Connected;
                self.dirty = true;
                for evt in ["VOICE_CHANNEL_SELECT", "VOICE_SETTINGS_UPDATE"] {
                    if let Err(e) = client.subscribe(evt, json!({})).await {
                        log::warn!("{evt} を購読できません: {e:#}");
                    }
                }
                match client.command("GET_VOICE_SETTINGS", json!({})).await {
                    Ok(v) => self.on_discord_voice(&v),
                    Err(e) => log::warn!("Discord の音声設定を取得できません: {e:#}"),
                }
                self.refresh_channel().await;
            }
        }
    }

    fn on_disconnect(&mut self) {
        log::info!("Discordとの接続が切れました");
        // 切断すると Discord が音量とミュートを元に戻すので、こちらの記録も捨てる。ロビー通話と卓の状態は保持する
        self.client = None;
        self.events = None;
        self.subscribed = None;
        self.zeroed.clear();
        self.input_zeroed = None;
        self.status = Status::Waiting;
        self.dirty = true;
    }

    fn on_discord_voice(&mut self, v: &Value) {
        let mut dv = DiscordVoice::parse(v);
        if self.input_zeroed.is_some() {
            // こちらで 0 にした入力音量を「元の値」として覚えないようにする
            dv.input_volume = self.discord_voice.as_ref().map_or(100.0, |d| d.input_volume);
        }
        let changed_mute = self.discord_voice.as_ref().map_or(true, |d| d.mute != dv.mute || d.deaf != dv.deaf);
        if self.discord_voice.as_ref() != Some(&dv) {
            log::info!(
                "Discord の音声設定: マイク={} ({}) 入力音量={} / スピーカー={} ({}) / mute={} deaf={}",
                dv.input_name.as_deref().unwrap_or("?"),
                dv.input_id,
                dv.input_volume,
                dv.output_name.as_deref().unwrap_or("?"),
                dv.output_id,
                dv.mute,
                dv.deaf
            );
        }
        if self.discord_voice.as_ref() != Some(&dv) {
            self.dirty = true;
        }
        self.discord_voice = Some(dv.clone());
        if changed_mute {
            // Discord でミュート・スピーカーミュートしたら、ロビー通話にも反映する
            self.voice.set_self_mute(dv.mute);
            self.voice.set_self_deaf(dv.deaf);
        }
        self.sync_devices();
    }

    /// 今いるVCを取り直し、VCが変わっていれば購読し直す。
    async fn refresh_channel(&mut self) {
        let Some(client) = self.client.clone() else { return };
        let mut data = match client.command("GET_SELECTED_VOICE_CHANNEL", json!({})).await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("VCを取得できません: {e:#}");
                return;
            }
        };
        let new_id = data.get("id").and_then(Value::as_str).map(String::from);
        if self.subscribed != new_id {
            if let Some(old) = self.subscribed.take() {
                for evt in VOICE_EVENTS {
                    if let Err(e) = client.unsubscribe(evt, json!({ "channel_id": old })).await {
                        log::info!("{evt} の購読解除に失敗: {e:#}");
                    }
                }
            }
            if let Some(id) = &new_id {
                for evt in VOICE_EVENTS {
                    if let Err(e) = client.subscribe(evt, json!({ "channel_id": id })).await {
                        log::warn!("{evt} を購読できません: {e:#}");
                    }
                }
                self.subscribed = Some(id.clone());
                // 購読するまでの間に入ってきた人を拾うため取り直す
                if let Ok(d) = client.command("GET_SELECTED_VOICE_CHANNEL", json!({})).await {
                    data = d;
                }
            }
        }
        self.apply_channel(&data);
    }

    fn apply_channel(&mut self, data: &Value) {
        let now = Instant::now();
        let channel = data.get("id").and_then(Value::as_str).map(|id| Channel {
            id: id.to_string(),
            name: data["name"].as_str().unwrap_or_default().to_string(),
        });
        let members: Vec<_> = data["voice_states"]
            .as_array()
            .map(|a| a.iter().filter_map(parse_voice_member).filter(|m| !self.is_self_account(&m.id)).collect())
            .unwrap_or_default();
        for m in &members {
            self.remember_volume(&m.id, m.volume);
            self.muted.insert(m.id.clone(), m.muted);
        }
        let present: Vec<(String, String)> = members.iter().map(|m| (m.id.clone(), m.name.clone())).collect();
        self.session.set_channel(channel.clone(), &present, now);
        self.follow_channel(channel.as_ref());
        self.apply_remote_table();
        self.sync_targets(now);
    }

    fn remember_volume(&mut self, id: &str, volume: Option<f64>) {
        // こちらで 0 にしている間に届いた 0 は、元の値として覚えない
        if let Some(v) = volume {
            if !self.zeroed.contains_key(id) {
                self.discord_volumes.insert(id.to_string(), v);
            }
        }
    }

    async fn on_event(&mut self, ev: Event) {
        let now = Instant::now();
        match ev.evt.as_str() {
            "VOICE_CHANNEL_SELECT" => self.refresh_channel().await,
            "VOICE_SETTINGS_UPDATE" => self.on_discord_voice(&ev.data),
            "VOICE_STATE_CREATE" | "VOICE_STATE_UPDATE" | "VOICE_STATE_DELETE" => {
                let Some(m) = parse_voice_member(&ev.data) else { return };
                if self.is_self_account(&m.id) {
                    return;
                }
                match ev.evt.as_str() {
                    "VOICE_STATE_CREATE" => {
                        self.muted.insert(m.id.clone(), m.muted);
                        self.remember_volume(&m.id, m.volume);
                        self.session.join(&m.id, &m.name);
                    }
                    "VOICE_STATE_UPDATE" => {
                        self.dirty |= self.muted.insert(m.id.clone(), m.muted) != Some(m.muted);
                        self.remember_volume(&m.id, m.volume);
                        self.dirty |= self.session.update_name(&m.id, &m.name);
                        return;
                    }
                    _ => self.session.leave(&m.id, now),
                }
                self.apply_remote_table();
                self.sync_targets(now);
            }
            _ => {}
        }
    }

    // ---------- ロビー通話 ----------

    /// 今いる VC に合わせて、通話するロビーを切り替える。
    fn follow_channel(&mut self, channel: Option<&Channel>) {
        let secret = channel.map(|c| format!("{LOBBY_PREFIX}{}", c.id));
        if secret == self.secret {
            return;
        }
        log::info!("VC が変わった: {:?} → ロビー {:?}", channel.map(|c| &c.name), secret);
        self.secret = secret.clone();
        self.current_lobby = None;
        self.call_connected = false;
        self.lobby_participants.clear();
        self.lobby_members.clear();
        self.speaking.clear();
        self.applied_table = None;
        match secret {
            Some(s) => {
                let meta = table_metadata(&self.self_name(), self.gm_table().as_ref());
                self.voice.join(&s, &meta);
                if !matches!(self.voice_status, VoiceStatus::Authorizing | VoiceStatus::Starting) {
                    self.voice_status = VoiceStatus::Joining;
                }
            }
            None => {
                self.voice.leave_call();
                if matches!(self.voice_status, VoiceStatus::Joining | VoiceStatus::InCall) {
                    self.voice_status = VoiceStatus::Idle;
                }
            }
        }
        self.dirty = true;
    }

    /// 前の通話の遅れた通知か（ロビー ID が今の通話と違う）
    fn stale(&self, lobby_id: &str) -> bool {
        !lobby_id.is_empty() && self.current_lobby.as_deref().is_some_and(|cur| cur != lobby_id)
    }

    fn on_voice(&mut self, ev: VoiceEvent) {
        let stale = match &ev {
            VoiceEvent::Call { lobby_id, .. }
            | VoiceEvent::CallEnded { lobby_id }
            | VoiceEvent::Participant { lobby_id, .. }
            | VoiceEvent::Speaking { lobby_id, .. } => self.stale(lobby_id),
            _ => false,
        };
        if stale {
            log::info!("前の通話の通知なので無視: {ev:?}");
            return;
        }
        // ロビー通話のできごとはログに残す（実機での切り分け用。トークンは出さない）
        match &ev {
            VoiceEvent::Log { .. } | VoiceEvent::Token { .. } | VoiceEvent::Speaking { .. } => {}
            VoiceEvent::Member { user_id, name, connected, metadata } => {
                log::info!("ロビーのメンバー: {name}（{user_id}）connected={connected} gm={:?}", metadata.get("gm"))
            }
            other => log::info!("ロビー通話: {other:?}"),
        }
        match ev {
            VoiceEvent::Log { message } => log::info!("[Social SDK] {message}"),
            VoiceEvent::Authorizing => self.voice_status = VoiceStatus::Authorizing,
            VoiceEvent::Token { access_token, .. } => {
                // Discord 本体（RPC）のログインにも使う
                self.rpc_token.send_modify(|l| l.token = Some(access_token));
            }
            VoiceEvent::Status { status, error, .. } => {
                if status == "Disconnected" && error != "None" {
                    self.voice_status = VoiceStatus::Error(format!("ロビー通話のログインに失敗しました（{error}）"));
                    self.call_connected = false;
                }
            }
            VoiceEvent::Ready { user_id, name } => {
                // このアカウントでログインしている Discord 本体につなぐ
                self.rpc_token.send_if_modified(|l| {
                    let changed = l.user_id.as_deref() != Some(&user_id);
                    l.user_id = Some(user_id.clone());
                    l.user_name = name.clone();
                    changed
                });
                self.dual.lock().unwrap().self_ids.insert(user_id.clone());
                self.voice_user = Some(user_id);
                self.voice_status = if self.secret.is_some() { VoiceStatus::Joining } else { VoiceStatus::Idle };
                self.voice.request_devices();
                if let Some(dv) = self.discord_voice.clone() {
                    self.voice.set_self_mute(dv.mute);
                    self.voice.set_self_deaf(dv.deaf);
                }
            }
            VoiceEvent::Joined { .. } => {}
            VoiceEvent::CallStarted { lobby_id } => {
                self.current_lobby = Some(lobby_id);
                self.lobby_participants.clear();
                self.speaking.clear();
            }
            VoiceEvent::Call { status, .. } => {
                let connected = status == "Connected";
                if connected != self.call_connected {
                    self.call_connected = connected;
                    if !connected {
                        self.lobby_participants.clear();
                        self.speaking.clear();
                    }
                }
                if connected {
                    self.voice_status = VoiceStatus::InCall;
                } else if status == "Disconnected" && self.secret.is_some() {
                    self.voice_status = VoiceStatus::Joining;
                }
            }
            VoiceEvent::CallEnded { .. } => {
                self.call_connected = false;
                self.lobby_participants.clear();
                self.speaking.clear();
            }
            VoiceEvent::Participant { user_id, added, .. } => {
                if Some(user_id.as_str()) != self.voice_user.as_deref() {
                    if added {
                        self.lobby_participants.insert(user_id);
                    } else {
                        self.lobby_participants.remove(&user_id);
                        self.speaking.remove(&user_id);
                    }
                }
            }
            VoiceEvent::Speaking { user_id, speaking, .. } => {
                if speaking {
                    self.speaking.insert(user_id);
                } else {
                    self.speaking.remove(&user_id);
                }
            }
            VoiceEvent::Member { user_id, name, connected, metadata } => {
                let table = parse_table(&metadata);
                self.lobby_members.insert(user_id, LobbyMember { name, connected, table });
                self.apply_remote_table();
            }
            VoiceEvent::MemberRemoved { user_id } => {
                self.lobby_members.remove(&user_id);
                self.apply_remote_table();
            }
            VoiceEvent::Devices { input, output, current_input, current_output } => {
                self.sdk_devices = Some((input, output, current_input, current_output));
                self.sync_devices();
            }
            VoiceEvent::DeviceSet { kind, id } => {
                if let Some(d) = &mut self.sdk_devices {
                    if kind == "input" {
                        d.2 = id;
                    } else {
                        d.3 = id;
                    }
                }
            }
            VoiceEvent::Error { stage, message, retry_after, .. } => {
                log::warn!("ロビー通話のエラー（{stage}）: {message}");
                let detail = if retry_after > 0.0 { format!("（{retry_after:.0}秒後に再試行できます）") } else { String::new() };
                match stage.as_str() {
                    "auth" if message.contains("invalid_scope") => {
                        self.voice_status = VoiceStatus::Error(
                            "このアカウントはアプリのテスターに登録されていません。Developer Portal の App Testers に追加し、届いたメールで承認してから「ログインし直す」を押してください".into(),
                        );
                    }
                    "join" | "call" | "auth" => {
                        self.call_connected = false;
                        self.voice_status = VoiceStatus::Error(format!("ロビー通話に入れません: {message}{detail}"));
                    }
                    _ => self.device_warning = Some(format!("デバイスを切り替えられません: {message}")),
                }
            }
        }
        self.dirty = true;
    }

    /// SDK のマイクとスピーカーを合わせる。設定で指定があればそれに、なければ Discord で選んでいるものに。
    fn sync_devices(&mut self) {
        let Some((ins, outs, cur_in, cur_out)) = &self.sdk_devices else { return };
        let dv = self.discord_voice.as_ref();
        let mut warn = vec![];
        for (kind, list, cur, choice, discord) in [
            ("マイク", ins, cur_in, &self.settings.input_device, dv.map(|d| (&d.input_id, d.input_name.as_deref()))),
            ("スピーカー", outs, cur_out, &self.settings.output_device, dv.map(|d| (&d.output_id, d.output_name.as_deref()))),
        ] {
            let (want, source) = match (choice, discord) {
                (Some(c), _) => (match_device(list, &c.id, Some(&c.name)), format!("設定で指定「{}」", c.name)),
                (None, Some((id, name))) => (match_device(list, id, name), format!("Discord と同じ「{}」", name.unwrap_or(id))),
                (None, None) => continue,
            };
            match want {
                Some(d) if &d.id != cur => {
                    log::info!("{kind}を切り替える: {source} → {}（{}）", d.name, d.id);
                    if kind == "マイク" {
                        self.voice.set_input_device(&d.id);
                    } else {
                        self.voice.set_output_device(&d.id);
                    }
                }
                Some(d) => log::info!("{kind}はそのまま: {source} = {}", d.name),
                None => {
                    log::warn!("{kind}が見つかりません: {source}");
                    warn.push(format!("{kind}が見つかりません（{source}）"));
                }
            }
        }
        self.device_warning = (!warn.is_empty()).then(|| warn.join("／"));
    }

    /// 通話中は、Discord 側のマイク入力と、ロビーにいる人の音量を 0 にする。そうでなければ元に戻す。
    fn reconcile_discord(&mut self) {
        let Some(client) = self.client.clone() else { return };
        let before = (self.zeroed.clone(), self.input_zeroed);
        let active = self.call_connected;
        let want_zero: HashSet<String> = if active {
            self.session
                .visible_members()
                .into_iter()
                .filter(|m| m.presence == Presence::Present && self.lobby_participants.contains(&m.id))
                .map(|m| m.id.clone())
                .collect()
        } else {
            HashSet::new()
        };
        for id in &want_zero {
            if !self.zeroed.contains_key(id) {
                let original = self.discord_volumes.get(id).copied().unwrap_or(100.0);
                self.zeroed.insert(id.clone(), original);
                log::info!("Discord 側の音量を 0 に: {id}（元の音量 {original}）");
                fire(&client, "SET_USER_VOICE_SETTINGS", json!({ "user_id": id, "volume": 0 }));
            }
        }
        let restore: Vec<(String, f64)> =
            self.zeroed.iter().filter(|(id, _)| !want_zero.contains(*id)).map(|(id, v)| (id.clone(), *v)).collect();
        for (id, v) in restore {
            self.zeroed.remove(&id);
            log::info!("Discord 側の音量を戻す: {id} → {v}");
            fire(&client, "SET_USER_VOICE_SETTINGS", json!({ "user_id": id, "volume": v.round() }));
        }
        self.reconcile_input(&client, active);
        if before != (self.zeroed.clone(), self.input_zeroed) {
            self.save_restore();
        }
    }

    fn reconcile_input(&mut self, client: &RpcClient, active: bool) {
        match (active, self.input_zeroed) {
            (true, None) => {
                let original = self.discord_voice.as_ref().map_or(100.0, |d| d.input_volume);
                self.input_zeroed = Some(original);
                log::info!("Discord 側のマイク入力を 0 に（元の入力音量 {original}）");
                fire(client, "SET_VOICE_SETTINGS", json!({ "input": { "volume": 0 } }));
            }
            (false, Some(original)) => {
                self.input_zeroed = None;
                log::info!("Discord 側のマイク入力を戻す: {original}");
                fire(client, "SET_VOICE_SETTINGS", json!({ "input": { "volume": original } }));
            }
            _ => {}
        }
    }

    // ---------- 同期 ----------

    fn gm_table(&self) -> Option<TableState> {
        self.gm_mode.then(|| {
            let mut roles = self.session.roles();
            if let Some(me) = self.self_id() {
                roles.insert(me.to_string(), self.self_role);
            }
            TableState { phase: self.session.phase(), roles, mutes: self.mute_requests.clone() }
        })
    }

    /// 他の人の卓（GM のメンバーメタデータ）。VC にいて接続中の GM だけを見る。
    fn remote_gms(&self) -> Vec<(&String, &LobbyMember)> {
        let me = self.voice_user.as_deref().or(self.self_id());
        let mut v: Vec<_> = self
            .lobby_members
            .iter()
            .filter(|(id, m)| {
                Some(id.as_str()) != me
                    && m.connected
                    && m.table.is_some()
                    && self.session.member(id).is_some_and(|s| s.presence == Presence::Present)
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    }

    fn read_only(&self) -> bool {
        !self.gm_mode && !self.remote_gms().is_empty()
    }

    fn apply_remote_table(&mut self) {
        if self.gm_mode {
            return;
        }
        let Some(table) = self.remote_gms().first().and_then(|(_, m)| m.table.clone()) else {
            self.applied_table = None;
            return;
        };
        if self.applied_table.as_ref() == Some(&table) {
            return;
        }
        let me = self.self_id().map(String::from);
        // 自分へのミュートの指示：新しい番号のときだけ 1 回従う（そのあと本人が解除してよい）。
        // GM の卓を初めて読んだときの指示は、入る前に出された古いものなので従わない
        if let Some(&(seq, on)) = me.as_ref().and_then(|id| table.mutes.get(id)) {
            match self.seen_mute_seq {
                Some(seen) if seen >= seq => {}
                Some(_) => {
                    log::info!("GM の指示で Discord のミュートを {}", if on { "オン" } else { "オフ" });
                    self.set_discord_mute(on);
                    self.seen_mute_seq = Some(seq);
                }
                None => self.seen_mute_seq = Some(seq),
            }
        } else if self.seen_mute_seq.is_none() {
            self.seen_mute_seq = Some(0);
        }
        if let Some(r) = me.as_ref().and_then(|id| table.roles.get(id)) {
            self.self_role = *r;
        }
        let roles: HashMap<String, Role> =
            table.roles.iter().filter(|(id, _)| Some(*id) != me.as_ref()).map(|(id, r)| (id.clone(), *r)).collect();
        log::info!("GM の卓を反映: フェーズ={} 役職={:?}", table.phase.as_str(), table.roles);
        // フェーズを先に合わせてから役職を反映する（シナリオ前→中の確定は、その時点の役職で行われるため）
        self.session.apply_roles(&roles);
        self.session.set_phase(table.phase);
        self.session.apply_roles(&roles);
        self.applied_table = Some(table);
        self.sync_targets(Instant::now());
    }

    fn schedule_sync(&mut self) {
        if self.gm_mode {
            self.sync_due = Some(Instant::now() + self.timing.sync_debounce);
        }
    }

    // ---------- UI・タイマー ----------

    fn on_ui(&mut self, cmd: UiCommand) {
        let read_only = self.read_only();
        match cmd {
            UiCommand::SetPhase(p) if !read_only => {
                self.session.set_phase(p);
                self.schedule_sync();
            }
            UiCommand::SetRole(id, r) if !read_only => {
                if Some(id.as_str()) == self.self_id() {
                    self.self_role = r;
                } else {
                    self.session.set_role(&id, r);
                }
                self.schedule_sync();
            }
            UiCommand::Reset if !read_only => {
                self.session.reset();
                self.schedule_sync();
            }
            // 並べ替えは自分の耳だけの上書き。同期しない
            UiCommand::Reorder(id, i) => self.session.reorder(&id, i),
            UiCommand::Relogin => {
                self.voice_status = VoiceStatus::Authorizing;
                self.voice.reauthorize();
            }
            UiCommand::SetInputDevice(id) => self.set_device_choice(true, id),
            UiCommand::SetOutputDevice(id) => self.set_device_choice(false, id),
            UiCommand::RefreshDevices => self.voice.request_devices(),
            UiCommand::SetSelfMute(on) => self.set_discord_mute(on),
            UiCommand::RequestMute(id, on) => {
                // GM として卓を開いていて、この VC にいる人だけ（2垢モードでは、その人がいる側が書く）
                if self.gm_mode && self.session.member(&id).is_some() {
                    let seq = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    log::info!("ミュートの指示: {id} → {}", if on { "オン" } else { "オフ" });
                    self.mute_requests.insert(id, (seq, on));
                    // すぐ届けたいので、まとめる待ちは短く
                    self.sync_due = Some(Instant::now() + Duration::from_millis(300));
                }
            }
            UiCommand::SetGap(g) => {
                let g = g.clamp(0.0, 0.8);
                self.settings.gap = g;
                self.settings.save(&self.settings_path);
                self.dual.lock().unwrap().gap = g;
            }
            UiCommand::SetSpan(lo, hi) => {
                // 最小幅は設けない（L と R が同じ位置なら全員が重なる）
                let lo = lo.clamp(-1.0, 1.0);
                let span = (lo, hi.clamp(lo, 1.0));
                self.settings.span = span;
                self.settings.save(&self.settings_path);
                self.dual.lock().unwrap().span = span;
            }
            UiCommand::SetGmMode(on) => {
                if on != self.gm_mode {
                    self.gm_mode = on;
                    // GM として卓を開いたら、自分の役職は GM
                    if on && self.self_role == Role::Pl {
                        self.self_role = Role::Gm;
                    }
                    self.applied_table = None;
                    // GM をやめたときも、gm の印を消すために 1 回書く
                    self.sync_due = Some(Instant::now());
                    if !on {
                        self.apply_remote_table();
                    }
                }
            }
            _ => {
                self.dirty = true;
                return;
            }
        }
        self.sync_targets(Instant::now());
    }

    /// ロビー通話のマイク・スピーカーの設定を変える（None は Discord と同じ）。保存して次回も使う。
    fn set_device_choice(&mut self, input: bool, id: Option<String>) {
        let list = self.sdk_devices.as_ref().map(|d| if input { &d.0 } else { &d.1 });
        let choice = id.and_then(|id| {
            list?.iter().find(|d| d.id == id).map(|d| DeviceRef { id: d.id.clone(), name: d.name.clone() })
        });
        log::info!("{}の設定: {:?}", if input { "マイク" } else { "スピーカー" }, choice);
        if input {
            self.settings.input_device = choice;
        } else {
            self.settings.output_device = choice;
        }
        self.settings.save(&self.settings_path);
        self.sync_devices();
    }

    /// メインとサブで役職と席の数を共有し、受け持つ範囲（左右）を決める。
    fn sync_dual(&mut self, now: Instant) {
        let dual = self.dual.clone();
        let mut d = dual.lock().unwrap();
        if self.side == Side::Main && self.seen_roles_rev == u64::MAX {
            d.gap = self.settings.gap;
            d.span = self.settings.span;
        }
        let mut changed = false;
        // 他の側で変わった役職を取り込む（VC を移ってきた人の役職もここで引き継ぐ）
        if d.roles_rev != self.seen_roles_rev {
            let roles: HashMap<String, Role> =
                d.roles.iter().filter(|(id, _)| !d.self_ids.contains(*id)).map(|(k, v)| (k.clone(), *v)).collect();
            changed |= self.session.apply_roles(&roles);
        }
        // こちらの役職を共有する（新しく入った人の初期値、こちらでの変更）
        for (id, r) in self.session.roles() {
            if d.roles.get(&id) != Some(&r) {
                d.roles.insert(id, r);
                d.roles_rev += 1;
            }
        }
        self.seen_roles_rev = d.roles_rev;
        // 席の数を共有し、両側に席があれば左右に分ける
        let mine = self.session.counted_seats().len();
        d.seated[self.side.index()] = mine;
        let other = d.seated[1 - self.side.index()];
        let split = d.sub_active && mine > 0 && other > 0;
        let (lo, hi) = seat_range(split, self.side, d.gap, d.span);
        drop(d);
        changed |= self.session.set_range(lo, hi);
        if changed {
            self.sync_targets(now);
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        self.sync_dual(now);
        if self.session.tick(now) {
            self.sync_targets(now);
        }
        if self.session.has_grace() && now.duration_since(self.last_grace_emit) >= Duration::from_secs(1) {
            self.dirty = true;
        }
        if self.sync_due.is_some_and(|t| t <= now) && self.secret.is_some() && self.voice_user.is_some() {
            self.sync_due = None;
            let meta = table_metadata(&self.self_name(), self.gm_table().as_ref());
            log::info!("卓の状態を書く: {meta:?}");
            self.voice.set_metadata(&meta);
        }
        for (id, pan) in self.animator.tick(now) {
            self.voice.set_gain(&id, pan);
        }
    }

    fn sync_targets(&mut self, now: Instant) {
        let layout = format!(
            "フェーズ={} 席={:?} 位置={:?}",
            self.session.phase().as_str(),
            self.session.counted_seats(),
            {
                let mut p: Vec<_> = self.session.positions().into_iter().map(|(k, v)| (k, (v * 100.0).round() / 100.0)).collect();
                p.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                p
            }
        );
        if layout != self.last_layout_log {
            log::info!("配置: {layout}");
            self.last_layout_log = layout;
        }
        let targets = self.session.targets();
        self.animator.retain(|id| targets.contains_key(id));
        for (id, t) in &targets {
            self.animator.set_target(id, *t, now);
        }
        self.dirty = true;
    }

    fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let now = Instant::now();
        self.last_grace_emit = now;
        let seats = self.session.counted_seats();
        let positions = self.session.positions();
        let members = self
            .session
            .visible_members()
            .into_iter()
            .map(|m| {
                let (presence, grace_secs) = match m.presence {
                    Presence::Present => ("present", None),
                    Presence::Grace(t) => ("grace", Some(t.saturating_duration_since(now).as_secs_f64().ceil() as u64)),
                    _ => ("away", None),
                };
                MemberView {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    role: m.role,
                    presence,
                    grace_secs,
                    seat: seats.iter().position(|s| *s == m.id),
                    position: positions.get(&m.id).copied(),
                    in_lobby: self.lobby_participants.contains(&m.id),
                    speaking: self.speaking.contains(&m.id),
                    is_self: false,
                    muted: self.muted.get(&m.id).copied().unwrap_or(false),
                }
            })
            .collect::<Vec<_>>();
        let mut members = members;
        if let (Some(me), Some(_)) = (&self.self_user, self.session.channel()) {
            members.insert(
                0,
                MemberView {
                    id: me.id.clone(),
                    name: me.display_name().to_string(),
                    role: self.self_role,
                    presence: "present",
                    grace_secs: None,
                    seat: None,
                    position: None,
                    in_lobby: self.call_connected,
                    speaking: self.voice_user.as_ref().is_some_and(|u| self.speaking.contains(u)),
                    is_self: true,
                    muted: self.discord_voice.as_ref().is_some_and(|d| d.mute),
                },
            );
        }
        let gms = self.remote_gms();
        let mut warnings = vec![];
        if gms.len() > 1 || (self.gm_mode && !gms.is_empty()) {
            warnings.push("この VC で GM として卓を開いている人が複数います".to_string());
        }
        if let (Some(rpc), Some(sdk)) = (self.self_id(), self.voice_user.as_deref()) {
            if rpc != sdk {
                warnings.push("Discord 本体とロビー通話でログインしているアカウントが違います".to_string());
            }
        }
        if let Some(w) = &self.device_warning {
            warnings.push(w.clone());
        }
        // Discord のミュートはロビー通話にも連動する（気づかずに話しても届かないので知らせる）
        if let Some(dv) = &self.discord_voice {
            if dv.deaf {
                warnings.push("Discord でスピーカーミュート中のため、ロビー通話でも聞こえず話せません".to_string());
            } else if dv.mute {
                warnings.push("ミュート中です（ロビー通話もミュートしています）".to_string());
            }
        }
        let (gap, span) = {
            let d = self.dual.lock().unwrap();
            (d.gap, d.span)
        };
        let snap = Snapshot {
            self_muted: self.discord_voice.as_ref().is_some_and(|d| d.mute),
            side: self.side,
            gap,
            span,
            sub: None,
            status: self.status.clone(),
            voice: self.voice_status.clone(),
            self_user: self.self_user.as_ref().map(|u| UserView { id: u.id.clone(), name: u.display_name().to_string() }),
            channel: self.session.channel().cloned(),
            phase: self.session.phase(),
            members,
            seat_count: seats.len(),
            gm_mode: self.gm_mode,
            remote_gm: if self.gm_mode {
                None
            } else {
                gms.first().map(|(id, m)| UserView { id: (*id).clone(), name: m.name.clone() })
            },
            read_only: self.read_only(),
            warnings,
            audio: AudioView {
                inputs: self.sdk_devices.as_ref().map(|d| d.0.clone()).unwrap_or_default(),
                outputs: self.sdk_devices.as_ref().map(|d| d.1.clone()).unwrap_or_default(),
                current_input: self.sdk_devices.as_ref().map(|d| d.2.clone()).unwrap_or_default(),
                current_output: self.sdk_devices.as_ref().map(|d| d.3.clone()).unwrap_or_default(),
                input_choice: self.settings.input_device.as_ref().map(|d| d.id.clone()),
                output_choice: self.settings.output_device.as_ref().map(|d| d.id.clone()),
                discord_input: self.discord_voice.as_ref().map(|d| d.input_name.clone().unwrap_or_else(|| d.input_id.clone())),
                discord_output: self
                    .discord_voice
                    .as_ref()
                    .map(|d| d.output_name.clone().unwrap_or_else(|| d.output_id.clone())),
            },
        };
        (self.sink)(&snap);
    }
}

/// 応答を待たずに RPC コマンドを送る（失敗はログだけ）。
fn fire(client: &RpcClient, cmd: &'static str, args: Value) {
    let rx = client.send(cmd, args, None);
    tokio::spawn(async move {
        if let Ok(Err(e)) = rx.await {
            log::warn!("{cmd} に失敗: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_metadata_roundtrip() {
        let t = TableState {
            phase: Phase::During,
            roles: [("1".to_string(), Role::Gm), ("2".to_string(), Role::Pl)].into_iter().collect(),
            mutes: [("2".to_string(), (7, true))].into_iter().collect(),
        };
        let meta: HashMap<String, String> = table_metadata("GM", Some(&t)).into_iter().collect();
        assert_eq!(parse_table(&meta), Some(t));
        let plain: HashMap<String, String> = table_metadata("PL", None).into_iter().collect();
        assert_eq!(parse_table(&plain), None);
    }

    #[test]
    fn discord_voice_settings_parse() {
        let v = json!({
            "input": { "device_id": "b", "volume": 80.0, "available_devices": [{ "id": "a", "name": "A" }, { "id": "b", "name": "INPUT (2- Volt 1)" }] },
            "output": { "device_id": "default", "volume": 100.0, "available_devices": [] },
            "mute": true, "deaf": false
        });
        let d = DiscordVoice::parse(&v);
        assert_eq!(d.input_name.as_deref(), Some("INPUT (2- Volt 1)"));
        assert_eq!(d.input_volume, 80.0);
        assert_eq!(d.output_id, "default");
        assert!(d.mute);
    }

    #[test]
    fn seat_range_fits_into_span() {
        let close = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9;
        assert_eq!(seat_range(false, Side::Main, 0.2, DEFAULT_SPAN), (-1.0, 1.0));
        // L と R は別々に動かせる
        assert!(close(seat_range(false, Side::Sub, 0.2, (-0.4, 1.0)), (-0.4, 1.0)));
        // 2 分割でも隙間ごと縮める（−0.5〜+0.5 の中で、メイン −0.5〜−0.1・サブ +0.1〜+0.5）
        assert!(close(seat_range(true, Side::Main, 0.2, (-0.5, 0.5)), (-0.5, -0.1)));
        assert!(close(seat_range(true, Side::Sub, 0.2, (-0.5, 0.5)), (0.1, 0.5)));
        // 幅 0 なら全員が同じ位置
        assert!(close(seat_range(true, Side::Sub, 0.2, (0.3, 0.3)), (0.3, 0.3)));
    }
}
