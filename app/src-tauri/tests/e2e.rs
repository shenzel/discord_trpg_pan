//! 偽Discordにつないだ結合テスト。実機でしか見られない部分（音の聞こえ方・本物のDiscordの挙動）以外をここで確かめる。

mod common;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{near, wait_until, FakeDiscord, FakeOAuth, FakeVoice};
use dpan::{
    auth::{Authenticator, Exchanger, Token, TokenStore},
    controller::{self, Dual, Side, Snapshot, Status, Timing, UiCommand},
    pan::{Pan, Polar},
    rpc::{self, ConnectFailure},
    session::{Phase, Role},
    voice::{Device, VoiceEvent},
};
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(5);

fn temp_token_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dpan-test-{}", common::unique_prefix().rsplit('\\').next().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    // 過去の実行とプロセス ID が重なると前回のトークンが残っているので、必ず消してから使う
    let path = dir.join("tokens.json");
    let _ = std::fs::remove_file(&path);
    path
}

fn exchanger(oauth: &FakeOAuth) -> Exchanger {
    Exchanger::Direct {
        client_id: "cid".into(),
        client_secret: "secret".into(),
        token_url: oauth.url.clone(),
        redirect_uri: None,
    }
}

fn pos(p: f64) -> Pan {
    Polar::from_position(p).to_pan()
}

// ---------- 接続と認証 ----------

#[tokio::test(flavor = "multi_thread")]
async fn first_login_authorizes_once_then_reuses_saved_token() {
    let discord = FakeDiscord::start("me", 0);
    let oauth = FakeOAuth::start().await;
    let path = temp_token_path();

    let conn = rpc::connect_any(&discord.prefix, "cid").await.unwrap();
    assert_eq!(conn.user.id, "me");
    let mut auth = Authenticator::new("cid".into(), Some(exchanger(&oauth)), TokenStore::load(&path));
    let mut dialog = false;
    let user = auth.login(&conn.client, &conn.user.id, || dialog = true).await.unwrap();
    assert_eq!(user.id, "me");
    assert!(dialog);
    assert_eq!(discord.state().authorize_count, 1);
    assert_eq!(TokenStore::load(&path).get("me").unwrap().access_token, "at-1");
    conn.abort();

    // 2回目：保存したトークンで認証し、ダイアログは出ない
    let conn = rpc::connect_any(&discord.prefix, "cid").await.unwrap();
    let mut auth = Authenticator::new("cid".into(), Some(exchanger(&oauth)), TokenStore::load(&path));
    let mut dialog = false;
    auth.login(&conn.client, &conn.user.id, || dialog = true).await.unwrap();
    assert!(!dialog);
    assert_eq!(discord.state().authorize_count, 1);
    assert_eq!(discord.state().authenticated_with, vec!["at-1", "at-1"]);
    assert_eq!(oauth.count("authorization_code"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_token_is_refreshed_without_dialog() {
    let discord = FakeDiscord::start("me", 0);
    let oauth = FakeOAuth::start().await;
    let path = temp_token_path();
    TokenStore::load(&path)
        .put("me", Token { access_token: "at-old".into(), refresh_token: "rt-1".into(), expires_at: 0 })
        .unwrap();

    let conn = rpc::connect_any(&discord.prefix, "cid").await.unwrap();
    let mut auth = Authenticator::new("cid".into(), Some(exchanger(&oauth)), TokenStore::load(&path));
    let mut dialog = false;
    auth.login(&conn.client, "me", || dialog = true).await.unwrap();
    assert!(!dialog);
    assert_eq!(oauth.count("refresh_token"), 1);
    assert_eq!(discord.state().authenticated_with, vec!["at-refreshed"]);
    let saved = TokenStore::load(&path).get("me").cloned().unwrap();
    assert_eq!((saved.access_token.as_str(), saved.refresh_token.as_str()), ("at-refreshed", "rt-2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_token_and_failed_refresh_fall_back_to_authorize() {
    let discord = FakeDiscord::start("me", 0);
    let oauth = FakeOAuth::start().await;
    let path = temp_token_path();
    TokenStore::load(&path)
        .put("me", Token { access_token: "at-revoked".into(), refresh_token: "rt-bad".into(), expires_at: u64::MAX / 2 })
        .unwrap();
    discord.state().revoked.insert("at-revoked".into());

    let conn = rpc::connect_any(&discord.prefix, "cid").await.unwrap();
    let mut auth = Authenticator::new("cid".into(), Some(exchanger(&oauth)), TokenStore::load(&path));
    auth.login(&conn.client, "me", || {}).await.unwrap();
    assert_eq!(discord.state().authorize_count, 1);
    assert_eq!(discord.state().authenticated_with, vec!["at-revoked", "at-1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_rejection_is_reported() {
    let discord = FakeDiscord::start("me", 0);
    match rpc::connect_any(&discord.prefix, "bad").await {
        Err(ConnectFailure::Failed(e)) => assert!(format!("{e:#}").contains("4000"), "{e:#}"),
        other => panic!("拒否されるはず: {:?}", other.err()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn finds_discord_on_any_pipe_number() {
    let discord = FakeDiscord::start("me", 3);
    let conn = rpc::connect_any(&discord.prefix, "cid").await.unwrap();
    assert_eq!(conn.user.id, "me");
    assert!(matches!(rpc::connect_any(&common::unique_prefix(), "cid").await, Err(ConnectFailure::NotRunning)));
}

#[tokio::test(flavor = "multi_thread")]
async fn public_client_exchanges_without_secret() {
    let discord = FakeDiscord::start("me", 0);
    let oauth = FakeOAuth::start().await;
    let conn = rpc::connect_any(&discord.prefix, "cid").await.unwrap();
    let public = Exchanger::Public { client_id: "cid".into(), token_url: oauth.url.clone(), redirect_uri: None };
    let mut auth = Authenticator::new("cid".into(), Some(public), TokenStore::load(&temp_token_path()));
    auth.login(&conn.client, "me", || {}).await.unwrap();
    let reqs = oauth.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert!(!reqs[0].contains_key("client_secret"), "secret を送っていない");
    assert_eq!(reqs[0].get("client_id").map(String::as_str), Some("cid"));
}

// ---------- 制御ループ全体（ロビー通話は偽物） ----------

struct App {
    ui: mpsc::UnboundedSender<UiCommand>,
    voice_tx: mpsc::UnboundedSender<VoiceEvent>,
    voice: Arc<FakeVoice>,
    snap: Arc<Mutex<Option<Snapshot>>>,
}

impl App {
    /// ロビー通話のログイン（me）は済んでいて、そのトークン（at-sdk）で Discord 本体にもログインする。
    fn start_with(prefix: &str, client_id: &str) -> Self {
        Self::start_full(prefix, client_id, Side::Main, Default::default(), None, "me")
    }

    fn start_full(prefix: &str, client_id: &str, side: Side, dual: Dual, restore: Option<PathBuf>, user: &str) -> Self {
        let app = Self::start_without_token(prefix, client_id, side, dual, restore);
        app.voice_event(VoiceEvent::Token { access_token: "at-sdk".into(), scopes: String::new() });
        app.voice_event(VoiceEvent::Ready { user_id: user.into(), name: user.to_uppercase() });
        app
    }

    fn start_without_token(prefix: &str, client_id: &str, side: Side, dual: Dual, restore: Option<PathBuf>) -> Self {
        let cfg = controller::Config {
            client_id: client_id.into(),
            pipe_prefix: prefix.into(),
            timing: fast_timing(),
            settings_path: None,
            restore_path: restore,
            side,
            dual,
        };
        let (ui, rx) = mpsc::unbounded_channel();
        let (voice_tx, voice_rx) = mpsc::unbounded_channel();
        let voice = Arc::new(FakeVoice::default());
        let snap: Arc<Mutex<Option<Snapshot>>> = Default::default();
        let s = snap.clone();
        let sink: controller::Sink = Box::new(move |x: &Snapshot| *s.lock().unwrap() = Some(x.clone()));
        tokio::spawn(controller::run(cfg, rx, sink, voice.clone(), voice_rx));
        Self { ui, voice_tx, voice, snap }
    }

    fn start(discord: &FakeDiscord) -> Self {
        Self::start_with(&discord.prefix, "cid")
    }

    fn snap(&self) -> Snapshot {
        self.snap.lock().unwrap().clone().unwrap_or_default()
    }

    fn send(&self, c: UiCommand) {
        self.ui.send(c).unwrap();
    }

    fn voice_event(&self, e: VoiceEvent) {
        self.voice_tx.send(e).unwrap();
    }

    /// ロビー通話にログインし、通話がつながった状態にする。
    fn call_connected(&self, participants: &[&str]) {
        self.voice_event(VoiceEvent::Ready { user_id: "me".into(), name: "Me".into() });
        self.voice_event(VoiceEvent::Call { lobby_id: String::new(), status: "Connected".into(), error: "None".into() });
        for p in participants {
            self.voice_event(VoiceEvent::Participant { lobby_id: String::new(), user_id: p.to_string(), added: true, name: String::new() });
        }
    }
}

fn fast_timing() -> Timing {
    Timing {
        grace: Duration::from_millis(600),
        tween: Duration::from_millis(300),
        tick: Duration::from_millis(20),
        reconnect: Duration::from_millis(50),
        auth_retry: Duration::from_millis(200),
        sync_debounce: Duration::from_millis(100),
    }
}

async fn wait_gain(app: &App, id: &str, want: Pan) {
    wait_until(&format!("{id} の倍率が {want:?} になる"), T, || app.voice.gain(id).is_some_and(|g| near(g, want))).await;
}

async fn wait_connected(app: &App, members: usize) {
    wait_until(&format!("接続して {members} 人が並ぶ"), T, || {
        let s = app.snap();
        s.status == Status::Connected && s.members.iter().filter(|m| !m.is_self).count() == members
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn full_session_flow() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A"), ("b", "B"), ("c", "C")]);
    discord.add_channel("vc2", "卓2", &[("b2", "B2")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);

    // 接続・認証・購読・ロビー参加
    wait_connected(&app, 3).await;
    {
        let st = discord.state();
        assert_eq!(st.authorize_count, 0, "Discord 本体の許可（AUTHORIZE）は使わない");
        assert_eq!(st.authenticated_with, vec!["at-sdk"], "ロビー通話のトークンでログインする");
        assert!(st.unauthenticated_calls.is_empty(), "認証前にコマンドを送った: {:?}", st.unauthenticated_calls);
        for evt in ["VOICE_STATE_CREATE", "VOICE_STATE_UPDATE", "VOICE_STATE_DELETE"] {
            assert!(st.subs.contains(&(evt.into(), Some("vc1".into()))), "{evt} vc1 を購読していない");
        }
        assert!(st.subs.contains(&("VOICE_CHANNEL_SELECT".into(), None)));
        assert!(st.subs.contains(&("VOICE_SETTINGS_UPDATE".into(), None)));
    }
    wait_until("VC のロビーに入る", T, || app.voice.called("join dpan:vc1")).await;
    let s = app.snap();
    assert_eq!(s.channel.unwrap().name, "卓1");
    let me: Vec<_> = s.members.iter().filter(|m| m.is_self).collect();
    assert_eq!(me.len(), 1, "自分の行は 1 つ");
    assert_eq!((me[0].id.as_str(), me[0].seat), ("me", None), "自分は席を持たない");
    assert!(s.members.iter().all(|m| m.role == Role::Pl), "シナリオ前の初期値はPL");

    // シナリオ前は左右とも 1.0
    wait_gain(&app, "a", Pan::NEUTRAL).await;

    // シナリオ中：PL3人を左端・中央・右端へ
    app.send(UiCommand::SetPhase(Phase::During));
    wait_gain(&app, "a", pos(-1.0)).await;
    wait_gain(&app, "b", pos(0.0)).await;
    wait_gain(&app, "c", pos(1.0)).await;

    // bをGMに → 中央で席なし、a・cで2席
    app.send(UiCommand::SetRole("b".into(), Role::Gm));
    wait_gain(&app, "b", Polar::center().to_pan()).await;
    assert!(near(app.voice.gain("a").unwrap(), pos(-1.0)) && near(app.voice.gain("c").unwrap(), pos(1.0)));

    // 途中入室は観戦者で中央
    discord.join("vc1", "d", "D");
    wait_gain(&app, "d", Polar::center().to_pan()).await;
    assert_eq!(app.snap().members.iter().find(|m| m.id == "d").unwrap().role, Role::Spectator);

    // aが落ちる → 猶予中は席を空けたまま（cは右端のまま）→ 猶予切れで詰める
    discord.leave("vc1", "a");
    wait_until("aが猶予中になる", T, || {
        app.snap().members.iter().any(|m| m.id == "a" && m.presence == "grace" && m.grace_secs.is_some())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(near(app.voice.gain("c").unwrap(), pos(1.0)), "猶予中に詰めてしまった");
    wait_gain(&app, "c", pos(0.0)).await;
    assert!(app.snap().members.iter().all(|m| m.id != "a"));

    // aが戻る（猶予切れ後なので右端へ）
    discord.join("vc1", "a", "A");
    wait_gain(&app, "a", pos(1.0)).await;
    wait_gain(&app, "c", pos(-1.0)).await;

    // 感想戦：PLの並び（c, a）の右に GM(b) → 観戦者(d)
    app.send(UiCommand::SetPhase(Phase::After));
    let third = 1.0 / 3.0;
    wait_gain(&app, "a", pos(-third)).await;
    wait_gain(&app, "b", pos(third)).await;
    wait_gain(&app, "d", pos(1.0)).await;

    // 並べ替え：dを先頭へ
    app.send(UiCommand::Reorder("d".into(), 0));
    wait_gain(&app, "d", pos(-1.0)).await;
    wait_gain(&app, "b", pos(1.0)).await;

    // Discordがクラッシュ → 再接続（トークン再利用）。ロビー通話の倍率はそのまま
    discord.crash();
    wait_until("再接続", T, || discord.state().connections == 2 && app.snap().status == Status::Connected).await;
    assert!(near(app.voice.gain("d").unwrap(), pos(-1.0)));
    assert_eq!(discord.state().authorize_count, 0, "再接続でダイアログを出さない");
    assert_eq!(discord.state().authenticated_with, vec!["at-sdk", "at-sdk"]);

    // 自分のVC移動：購読し直し、ロビーも切り替える。移動先にいない人の分は詰めて並び順だけ覚えておく
    discord.state().channels.get_mut("vc2").unwrap().members.push(common::FakeMember {
        id: "a".into(),
        name: "A".into(),
        pan: Pan::NEUTRAL,
        volume: 100.0,
    });
    discord.state().channels.get_mut("vc1").unwrap().members.retain(|m| m.id != "a");
    discord.select(Some("vc2"));
    wait_until("vc2 を購読してロビーを切り替える", T, || {
        let st = discord.state();
        st.subs.contains(&("VOICE_STATE_CREATE".into(), Some("vc2".into())))
            && !st.subs.contains(&("VOICE_STATE_CREATE".into(), Some("vc1".into())))
            && app.voice.called("join dpan:vc2")
    })
    .await;
    wait_gain(&app, "a", pos(-1.0)).await;
    wait_gain(&app, "b2", pos(1.0)).await;
    assert_eq!(app.snap().seat_count, 2);

    // シナリオをリセット：左右とも 1.0 に戻し、役職も捨てる
    app.send(UiCommand::Reset);
    wait_gain(&app, "a", Pan::NEUTRAL).await;
    wait_gain(&app, "b2", Pan::NEUTRAL).await;
    assert!(app.snap().members.iter().all(|m| m.role == Role::Pl));

    // RPC の pan は使わない。自分には倍率を掛けない
    assert!(discord.state().pan_log.is_empty());
    assert!(app.voice.gain("me").is_none());

    // VC を抜けたら通話を終える
    discord.select(None);
    wait_until("通話を終える", T, || app.voice.called("leave")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_side_is_silenced_only_while_in_lobby_call() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A"), ("x", "X")]);
    discord.state().channels.get_mut("vc1").unwrap().members.iter_mut().for_each(|m| m.volume = 150.0);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 2).await;

    // 通話がつながるまでは何も変えない
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(discord.input_volume(), 80.0);
    assert!(discord.state().volume_log.is_empty());

    // a はアプリ利用者（ロビーにいる）、x はアプリなし
    app.call_connected(&["a"]);
    wait_until("Discord 側のマイク入力が 0", T, || discord.input_volume() == 0.0).await;
    wait_until("a の Discord 側の音量が 0", T, || discord.volume_of("a") == Some(0.0)).await;
    assert_eq!(discord.volume_of("x"), Some(150.0), "アプリなしの人はそのまま");
    assert!(app.snap().members.iter().any(|m| m.id == "a" && m.in_lobby));

    // a がロビーから抜けたら、a の Discord 側の音量を元に戻す
    app.voice_event(VoiceEvent::Participant { lobby_id: String::new(), user_id: "a".into(), added: false, name: String::new() });
    wait_until("a の音量を戻す", T, || discord.volume_of("a") == Some(150.0)).await;
    app.voice_event(VoiceEvent::Participant { lobby_id: String::new(), user_id: "a".into(), added: true, name: String::new() });
    wait_until("a の音量を再び 0", T, || discord.volume_of("a") == Some(0.0)).await;

    // ロビー通話に失敗したら、普段の Discord の通話に戻す
    app.voice_event(VoiceEvent::Error {
        stage: "join".into(),
        message: "rate limited".into(),
        code: 0,
        http: 429,
        retryable: true,
        retry_after: 30.0,
    });
    wait_until("マイク入力を戻す", T, || discord.input_volume() == 80.0).await;
    wait_until("a の音量を戻す", T, || discord.volume_of("a") == Some(150.0)).await;
    assert!(matches!(app.snap().voice, controller::VoiceStatus::Error(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_events_from_previous_call_are_ignored() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 1).await;
    app.voice_event(VoiceEvent::Ready { user_id: "me".into(), name: "Me".into() });

    // VC を移ったあと：新しい通話（L2）が始まり参加者が入ってから、前の通話（L1）の終了通知が遅れて届く
    app.voice_event(VoiceEvent::CallStarted { lobby_id: "L2".into() });
    app.voice_event(VoiceEvent::Participant { lobby_id: "L2".into(), user_id: "a".into(), added: true, name: String::new() });
    app.voice_event(VoiceEvent::Call { lobby_id: "L1".into(), status: "Disconnected".into(), error: "None".into() });
    app.voice_event(VoiceEvent::CallEnded { lobby_id: "L1".into() });
    app.voice_event(VoiceEvent::Call { lobby_id: "L2".into(), status: "Connected".into(), error: "None".into() });
    wait_until("新しい通話の参加者が残る", T, || {
        let s = app.snap();
        s.voice == controller::VoiceStatus::InCall && s.members.iter().any(|m| m.id == "a" && m.in_lobby)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mute_and_devices_follow_discord() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 1).await;
    app.voice_event(VoiceEvent::Ready { user_id: "me".into(), name: "Me".into() });

    // SDK は Windows の既定のマイクを使っている → Discord と同じマイク（名前で照合）に切り替える
    app.voice_event(VoiceEvent::Devices {
        input: vec![
            Device { id: "sdk-1".into(), name: "default (マイク (Virtual Desktop Audio))".into(), default: true },
            Device { id: "sdk-2".into(), name: "INPUT (2- Volt 1)".into(), default: false },
        ],
        output: vec![Device { id: "o1".into(), name: "default (Speakers)".into(), default: true }],
        current_input: "sdk-1".into(),
        current_output: "o1".into(),
    });
    wait_until("Discord と同じマイクに切り替える", T, || app.voice.called("input sdk-2")).await;
    app.voice_event(VoiceEvent::DeviceSet { kind: "input".into(), id: "sdk-2".into() });
    wait_until("切り替えが反映される", T, || app.snap().audio.current_input == "sdk-2").await;
    assert!(!app.voice.calls().iter().any(|c| c.starts_with("output")), "スピーカーは既定どうしなので変えない");

    // 設定でマイクを指定したら、Discord の設定より優先する。「Discord と同じ」に戻せば戻る
    app.send(UiCommand::SetInputDevice(Some("sdk-1".into())));
    wait_until("指定したマイクに切り替える", T, || app.voice.called("input sdk-1")).await;
    app.voice_event(VoiceEvent::DeviceSet { kind: "input".into(), id: "sdk-1".into() });
    wait_until("切り替えが反映される", T, || app.snap().audio.current_input == "sdk-1").await;
    app.send(UiCommand::SetInputDevice(None));
    wait_until("Discord と同じマイクに戻す", T, || app.voice.calls().iter().filter(|c| *c == "input sdk-2").count() == 2).await;

    // Discord でミュートしたら、ロビー通話もミュート
    discord.set_discord_mute(true);
    wait_until("ロビー通話もミュート", T, || app.voice.called("mute true")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gm_writes_table_state_to_metadata() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A"), ("b", "B")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 2).await;
    app.call_connected(&["a", "b"]);

    app.send(UiCommand::SetGmMode(true));
    wait_until("GM の印を書く", T, || app.voice.last_metadata().is_some_and(|m| m.get("gm").map(String::as_str) == Some("1")))
        .await;
    // GM 本人も役職表に入る（入っていないと、PL 側で GM が PL として席を持ってしまう）
    assert!(app.voice.last_metadata().unwrap()["roles"].contains(r#""me":"gm""#));
    assert!(app.snap().members.iter().any(|m| m.is_self && m.role == Role::Gm));
    app.send(UiCommand::SetRole("a".into(), Role::Gm));
    app.send(UiCommand::SetPhase(Phase::During));
    wait_until("役職とフェーズを書く", T, || {
        app.voice.last_metadata().is_some_and(|m| {
            m.get("phase").map(String::as_str) == Some("during") && m.get("roles").is_some_and(|r| r.contains(r#""a":"gm""#))
        })
    })
    .await;
    // まとめて書く（変更 2 回でも書き込みは 1 回）
    let writes = app.voice.calls().iter().filter(|c| *c == "metadata").count();
    assert!(writes <= 2, "書き込みすぎ: {writes} 回");

    // 並べ替えは自分の耳だけ。書き込まない
    app.send(UiCommand::Reorder("b".into(), 0));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(app.voice.calls().iter().filter(|c| *c == "metadata").count(), writes);

    // GM をやめたら印を消す
    app.send(UiCommand::SetGmMode(false));
    wait_until("GM の印を消す", T, || app.voice.last_metadata().is_some_and(|m| !m.contains_key("gm"))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pl_follows_remote_gm() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("g", "G"), ("a", "A"), ("b", "B")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 3).await;
    app.call_connected(&["g", "a", "b"]);

    let table = |phase: &str| VoiceEvent::Member {
        user_id: "g".into(),
        name: "G".into(),
        connected: true,
        metadata: [
            ("gm".to_string(), "1".to_string()),
            ("phase".to_string(), phase.to_string()),
            ("roles".to_string(), r#"{"g":"gm","a":"pl","b":"pl","me":"pl"}"#.to_string()),
        ]
        .into_iter()
        .collect(),
    };
    app.voice_event(table("during"));
    wait_until("GM の卓に従う", T, || {
        let s = app.snap();
        s.read_only && s.remote_gm.as_ref().is_some_and(|g| g.id == "g") && s.phase == Phase::During
    })
    .await;
    wait_gain(&app, "a", pos(-1.0)).await;
    wait_gain(&app, "b", pos(1.0)).await;
    wait_gain(&app, "g", Polar::center().to_pan()).await;

    // PL は役職とフェーズを変えられない。並べ替えはできる
    app.send(UiCommand::SetPhase(Phase::Before));
    app.send(UiCommand::Reorder("b".into(), 0));
    wait_gain(&app, "b", pos(-1.0)).await;
    assert_eq!(app.snap().phase, Phase::During);

    // GM が感想戦にしたら従う
    app.voice_event(table("after"));
    wait_until("感想戦に従う", T, || app.snap().phase == Phase::After).await;

    // GM が VC を抜けたら自分で操作できる
    discord.leave("vc1", "g");
    wait_until("GM がいなくなる", T, || !app.snap().read_only && app.snap().remote_gm.is_none()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_login_waits_for_lobby_token_and_recovers_from_rejection() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A")]);
    discord.select(Some("vc1"));
    let app = App::start_without_token(&discord.prefix, "cid", Side::Main, Default::default(), None);

    // ロビー通話のログインが済むまでは許可待ち
    wait_until("許可待ち", T, || app.snap().status == Status::Authorizing).await;
    app.voice_event(VoiceEvent::Ready { user_id: "me".into(), name: "Me".into() });

    // Discord 本体に拒否されるトークン（別アカウント・テスター未登録など）
    app.voice_event(VoiceEvent::Token { access_token: "bad".into(), scopes: String::new() });
    wait_until("ログインできない旨を出す", T, || matches!(app.snap().status, Status::Error(_))).await;

    // 「ログインし直す」で許可からやり直し、新しいトークンが届いたらつながる
    app.send(UiCommand::Relogin);
    wait_until("許可をやり直す", T, || app.voice.called("reauth")).await;
    app.voice_event(VoiceEvent::Token { access_token: "at-new".into(), scopes: String::new() });
    wait_connected(&app, 1).await;
    assert_eq!(discord.state().authenticated_with, vec!["bad", "at-new"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_to_the_discord_of_the_same_account() {
    // Stable（me2）と PTB（me）を同時に起動している。ロビー通話は me でログイン
    let prefix = common::unique_prefix();
    let _other = FakeDiscord::start_at(&prefix, "me2", 0);
    let mine = FakeDiscord::start_at(&prefix, "me", 1);
    mine.add_channel("vc1", "卓1", &[("a", "A")]);
    mine.select(Some("vc1"));
    let app = App::start_with(&prefix, "cid");
    wait_connected(&app, 1).await;
    assert_eq!(app.snap().self_user.unwrap().id, "me");
    assert_eq!(mine.state().authenticated_with, vec!["at-sdk"]);

    // 同じアカウントの Discord がなければ、起動中のアカウントを知らせる
    let prefix = common::unique_prefix();
    let _only_other = FakeDiscord::start_at(&prefix, "me2", 0);
    let app = App::start_with(&prefix, "cid");
    wait_until("アカウントが違う旨を出す", T, || matches!(&app.snap().status, Status::Error(m) if m.contains("見つかりません"))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dual_accounts_split_left_and_right() {
    // メイン垢 me（メインVC：a, b）とサブ垢 me2（サブVC：c）
    let prefix = common::unique_prefix();
    let main = FakeDiscord::start_at(&prefix, "me", 0);
    let sub = FakeDiscord::start_at(&prefix, "me2", 1);
    main.add_channel("main", "メイン", &[("a", "A"), ("b", "B"), ("c", "C")]);
    sub.add_channel("sub", "サブ", &[]);
    main.select(Some("main"));
    sub.select(Some("sub"));
    let dual: Dual = Default::default();
    dual.lock().unwrap().sub_active = true;
    let m = App::start_full(&prefix, "cid", Side::Main, dual.clone(), None, "me");
    let s = App::start_full(&prefix, "cid", Side::Sub, dual.clone(), None, "me2");
    wait_connected(&m, 3).await;
    wait_until("サブもつながる", T, || s.snap().status == Status::Connected).await;
    for app in [&m, &s] {
        app.send(UiCommand::SetPhase(Phase::During));
    }
    // まず全員メイン：全幅
    wait_gain(&m, "a", pos(-1.0)).await;
    wait_gain(&m, "c", pos(1.0)).await;

    // c が密談でサブ VC へ：メインは −1〜−g、サブは +g〜+1（1 人なら中点）
    main.leave("main", "c");
    sub.join("sub", "c", "C");
    let g = controller::DEFAULT_GAP;
    wait_gain(&m, "a", pos(-1.0)).await;
    wait_gain(&m, "b", pos(-g)).await;
    wait_gain(&s, "c", pos((g + 1.0) / 2.0)).await;
    assert_eq!(s.snap().members.iter().find(|m| m.id == "c").unwrap().role, Role::Pl, "VC を移っても役職を引き継ぐ");

    // メインの PL が全員サブへ：サブが全幅
    main.leave("main", "a");
    main.leave("main", "b");
    sub.join("sub", "a", "A");
    sub.join("sub", "b", "B");
    wait_gain(&s, "c", pos(-1.0)).await;
    wait_gain(&s, "b", pos(1.0)).await;
    // GM 本人の 2 つのアカウントは、どちらの一覧にも出ない（自分の行を除く）
    assert!(m.snap().members.iter().chain(s.snap().members.iter()).all(|x| x.is_self || (x.id != "me" && x.id != "me2")));
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_side_zeros_are_restored_after_restart() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A")]);
    discord.state().channels.get_mut("vc1").unwrap().members.iter_mut().for_each(|m| m.volume = 150.0);
    discord.select(Some("vc1"));
    let restore = temp_token_path().with_file_name("restore.json");

    // 通話中にアプリを終了（音量 0・マイク入力 0 のまま。Discord は次に同じアプリがつなぐとかけ直す）
    let app = App::start_full(&discord.prefix, "cid", Side::Main, Default::default(), Some(restore.clone()), "me");
    wait_connected(&app, 1).await;
    app.call_connected(&["a"]);
    wait_until("a の音量が 0", T, || discord.volume_of("a") == Some(0.0) && discord.input_volume() == 0.0).await;
    drop(app);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 起動し直す：ロビー通話はまだつながっていないので、元に戻す
    let app = App::start_full(&discord.prefix, "cid", Side::Main, Default::default(), Some(restore), "me");
    wait_connected(&app, 1).await;
    wait_until("a の音量を戻す", T, || discord.volume_of("a") == Some(150.0)).await;
    wait_until("マイク入力を戻す", T, || discord.input_volume() == 80.0).await;
}

fn discord_muted(d: &FakeDiscord) -> bool {
    d.state().voice_settings["mute"].as_bool().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn self_mute_toggles_discord_and_lobby() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 1).await;
    app.send(UiCommand::SetSelfMute(true));
    wait_until("Discord がミュート", T, || discord_muted(&discord)).await;
    wait_until("ロビー通話もミュート・表示も変わる", T, || app.voice.called("mute true") && app.snap().self_muted).await;
    app.send(UiCommand::SetSelfMute(false));
    wait_until("解除", T, || !discord_muted(&discord) && !app.snap().self_muted).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gm_mute_request_is_followed_once() {
    // GM 側：指示を書く
    let gm_discord = FakeDiscord::start("gm", 0);
    gm_discord.add_channel("vc1", "卓1", &[("me", "Me")]);
    gm_discord.select(Some("vc1"));
    let gm = App::start_full(&gm_discord.prefix, "cid", Side::Main, Default::default(), None, "gm");
    wait_connected(&gm, 1).await;
    gm.call_connected(&["me"]);
    gm.send(UiCommand::SetGmMode(true));
    gm.send(UiCommand::RequestMute("me".into(), true));
    wait_until("ミュートの指示を書く", T, || {
        gm.voice.last_metadata().is_some_and(|m| m.get("mutes").is_some_and(|x| x.contains(r#""me":["#) && x.contains("true")))
    })
    .await;

    // PL 側：GM の卓を初めて読んだときの古い指示には従わない
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("gm", "G")]);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 1).await;
    app.call_connected(&["gm"]);
    let table = |seq: u64, on: bool| VoiceEvent::Member {
        user_id: "gm".into(),
        name: "G".into(),
        connected: true,
        metadata: [
            ("gm".to_string(), "1".to_string()),
            ("phase".to_string(), "during".to_string()),
            ("roles".to_string(), r#"{"gm":"gm","me":"pl"}"#.to_string()),
            ("mutes".to_string(), format!(r#"{{"me":[{seq},{on}]}}"#)),
        ]
        .into_iter()
        .collect(),
    };
    app.voice_event(table(3, true));
    wait_until("GM の卓に従う", T, || app.snap().read_only).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!discord_muted(&discord), "入る前の古い指示には従わない");

    // 新しい指示には 1 回だけ従う
    app.voice_event(table(5, true));
    wait_until("GM の指示でミュート", T, || discord_muted(&discord)).await;
    // 本人が解除してよい。同じ指示のままなら、また勝手にミュートしない
    app.send(UiCommand::SetSelfMute(false));
    wait_until("本人が解除", T, || !discord_muted(&discord)).await;
    app.voice_event(table(5, true));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!discord_muted(&discord));
}

#[tokio::test(flavor = "multi_thread")]
async fn quitting_restores_discord_side_before_exit() {
    let discord = FakeDiscord::start("me", 0);
    discord.add_channel("vc1", "卓1", &[("a", "A")]);
    discord.state().channels.get_mut("vc1").unwrap().members.iter_mut().for_each(|m| m.volume = 150.0);
    discord.select(Some("vc1"));
    let app = App::start(&discord);
    wait_connected(&app, 1).await;
    app.call_connected(&["a"]);
    wait_until("通話中は 0", T, || discord.volume_of("a") == Some(0.0) && discord.input_volume() == 0.0).await;

    // 終了：戻し終わってから知らせが来る
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
    app.send(UiCommand::Shutdown(ack_tx));
    tokio::time::timeout(T, ack_rx.recv()).await.expect("終了の知らせが来ない");
    assert_eq!(discord.volume_of("a"), Some(150.0));
    assert_eq!(discord.input_volume(), 80.0);
    assert!(app.voice.called("leave"), "通話も終える");
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_before_discord_and_waits() {
    let app = App::start_with(&common::unique_prefix(), "cid");
    wait_until("待機中", T, || app.snap().status == Status::Waiting).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_client_id_shows_setup() {
    let app = App::start_with(&common::unique_prefix(), "");
    wait_until("設定エラー", T, || app.snap().status == Status::Setup).await;
}
