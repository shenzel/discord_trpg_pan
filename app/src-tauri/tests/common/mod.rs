//! テスト用の偽Discord（RPCの名前付きパイプサーバー）と偽OAuthトークンサーバー。
#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use dpan::{
    pan::Pan,
    rpc::{read_frame, write_frame, OP_CLOSE, OP_FRAME},
    voice::Voice,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{windows::named_pipe::ServerOptions, TcpListener},
    sync::mpsc,
    task::AbortHandle,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn unique_prefix() -> String {
    format!(r"\\.\pipe\dpan-test-{}-{}-", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

pub async fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        if start.elapsed() > timeout {
            panic!("待ちきれませんでした: {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn near(a: Pan, b: Pan) -> bool {
    a.max_diff(b) < 1e-6
}

#[derive(Clone, Debug)]
pub struct FakeMember {
    pub id: String,
    pub name: String,
    pub pan: Pan,
    pub volume: f64,
}

#[derive(Clone, Debug, Default)]
pub struct FakeChannel {
    pub name: String,
    pub members: Vec<FakeMember>,
}

#[derive(Default)]
pub struct FakeState {
    pub self_id: String,
    pub channels: BTreeMap<String, FakeChannel>,
    pub selected: Option<String>,
    pub subs: HashSet<(String, Option<String>)>,
    pub sub_log: Vec<String>,
    pub pan_log: Vec<(Instant, String, Pan)>,
    pub authorize_count: usize,
    pub authenticated_with: Vec<String>,
    pub revoked: HashSet<String>,
    pub connections: usize,
    /// 認証前に送られてきたコマンド（本物のDiscordは拒否する）
    pub unauthenticated_calls: Vec<String>,
    /// Discord 本体の音声設定（GET_VOICE_SETTINGS の応答）
    pub voice_settings: Value,
    /// SET_USER_VOICE_SETTINGS で送られた音量
    pub volume_log: Vec<(String, f64)>,
    out: Option<mpsc::UnboundedSender<Value>>,
    tasks: Vec<AbortHandle>,
}

impl FakeState {
    fn emit(&self, evt: &str, channel: Option<&str>, data: Value) {
        if !self.subs.contains(&(evt.to_string(), channel.map(String::from))) {
            return;
        }
        if let Some(out) = &self.out {
            let _ = out.send(json!({ "cmd": "DISPATCH", "evt": evt, "data": data, "nonce": null }));
        }
    }

    fn user_json(&self, id: &str, name: &str) -> Value {
        json!({ "id": id, "username": name.to_lowercase(), "global_name": name, "discriminator": "0", "avatar": null })
    }

    fn voice_state_json(&self, m: &FakeMember) -> Value {
        json!({
            "nick": m.name,
            "mute": false,
            "volume": m.volume,
            "pan": { "left": m.pan.left, "right": m.pan.right },
            "voice_state": { "mute": false, "deaf": false, "self_mute": false, "self_deaf": false, "suppress": false },
            "user": self.user_json(&m.id, &m.name),
        })
    }

    fn channel_json(&self, id: &str) -> Value {
        let ch = &self.channels[id];
        json!({
            "id": id,
            "name": ch.name,
            "type": 2,
            "guild_id": "guild",
            "voice_states": ch.members.iter().map(|m| self.voice_state_json(m)).collect::<Vec<_>>(),
        })
    }

    /// `authed` と `tx` は接続ごと（本物の Discord と同じく、別の接続の認証や通知先を上書きしない）
    fn respond(&mut self, msg: &Value, authed: &mut bool, tx: &mpsc::UnboundedSender<Value>) -> Value {
        let cmd = msg["cmd"].as_str().unwrap_or_default().to_string();
        let nonce = msg["nonce"].clone();
        let args = &msg["args"];
        let ok = |data: Value| json!({ "cmd": cmd, "data": data, "evt": null, "nonce": nonce });
        let err = |code: u32, m: &str| json!({ "cmd": cmd, "data": { "code": code, "message": m }, "evt": "ERROR", "nonce": nonce });
        if !*authed && cmd != "AUTHORIZE" && cmd != "AUTHENTICATE" {
            self.unauthenticated_calls.push(cmd.clone());
            return err(4006, "Not authenticated or invalid scope");
        }
        match cmd.as_str() {
            "AUTHORIZE" => {
                self.authorize_count += 1;
                ok(json!({ "code": "code-1" }))
            }
            "AUTHENTICATE" => {
                let token = args["access_token"].as_str().unwrap_or_default().to_string();
                self.authenticated_with.push(token.clone());
                if token.starts_with("at-") && !self.revoked.contains(&token) {
                    *authed = true;
                    // 通知（DISPATCH）は、認証を済ませた接続へ送る
                    self.out = Some(tx.clone());
                    let id = self.self_id.clone();
                    ok(json!({ "user": self.user_json(&id, "Me"), "scopes": ["rpc", "rpc.voice.read", "rpc.voice.write", "identify"] }))
                } else {
                    err(4009, "Invalid access token")
                }
            }
            "GET_SELECTED_VOICE_CHANNEL" => ok(match &self.selected {
                Some(id) => self.channel_json(id),
                None => Value::Null,
            }),
            "SUBSCRIBE" | "UNSUBSCRIBE" => {
                let evt = msg["evt"].as_str().unwrap_or_default().to_string();
                let channel = args["channel_id"].as_str().map(String::from);
                self.sub_log.push(format!("{cmd} {evt} {}", channel.as_deref().unwrap_or("-")));
                if cmd == "SUBSCRIBE" {
                    self.subs.insert((evt.clone(), channel));
                } else {
                    self.subs.remove(&(evt.clone(), channel));
                }
                ok(json!({ "evt": evt }))
            }
            "SET_USER_VOICE_SETTINGS" => {
                let id = args["user_id"].as_str().unwrap_or_default().to_string();
                let volume = args["volume"].as_f64();
                let pan = args.get("pan").map(|p| Pan { left: p["left"].as_f64().unwrap(), right: p["right"].as_f64().unwrap() });
                if let Some(v) = volume {
                    self.volume_log.push((id.clone(), v));
                }
                if let Some(p) = pan {
                    self.pan_log.push((Instant::now(), id.clone(), p));
                }
                for ch in self.channels.values_mut() {
                    for m in &mut ch.members {
                        if m.id == id {
                            if let Some(p) = pan {
                                m.pan = p;
                            }
                            if let Some(v) = volume {
                                m.volume = v;
                            }
                        }
                    }
                }
                ok(json!({ "user_id": id, "pan": args["pan"], "volume": volume }))
            }
            "GET_VOICE_SETTINGS" => ok(self.voice_settings.clone()),
            "SET_VOICE_SETTINGS" => {
                if let Some(v) = args["input"]["volume"].as_f64() {
                    self.voice_settings["input"]["volume"] = json!(v);
                }
                for k in ["mute", "deaf"] {
                    if let Some(b) = args[k].as_bool() {
                        self.voice_settings[k] = json!(b);
                    }
                }
                let settings = self.voice_settings.clone();
                self.emit("VOICE_SETTINGS_UPDATE", None, settings.clone());
                ok(settings)
            }
            _ => err(4000, "unknown command"),
        }
    }
}

pub struct FakeDiscord {
    pub prefix: String,
    state: Arc<Mutex<FakeState>>,
    accept: AbortHandle,
}

impl Drop for FakeDiscord {
    fn drop(&mut self) {
        self.accept.abort();
        for t in &self.state().tasks {
            t.abort();
        }
    }
}

impl FakeDiscord {
    /// `prefix{index}` でパイプを開く。
    pub fn start(self_id: &str, index: u8) -> Self {
        Self::start_at(&unique_prefix(), self_id, index)
    }

    /// 決まった prefix の `index` 番に立てる（Stable と PTB のように 2 つ起動している状態を作る）。
    pub fn start_at(prefix: &str, self_id: &str, index: u8) -> Self {
        let prefix = prefix.to_string();
        let path = format!("{prefix}{index}");
        let state = Arc::new(Mutex::new(FakeState {
            self_id: self_id.to_string(),
            voice_settings: json!({
                "input": { "device_id": "dev-b", "volume": 80.0, "available_devices": [
                    { "id": "dev-a", "name": "Mic A" }, { "id": "dev-b", "name": "INPUT (2- Volt 1)" }] },
                "output": { "device_id": "default", "volume": 100.0, "available_devices": [] },
                "mute": false, "deaf": false
            }),
            ..Default::default()
        }));
        let mut server = ServerOptions::new().first_pipe_instance(true).create(&path).unwrap();
        let st = state.clone();
        let accept = tokio::spawn(async move {
            loop {
                if server.connect().await.is_err() {
                    break;
                }
                let client = server;
                server = ServerOptions::new().create(&path).unwrap();
                serve(client, st.clone()).await;
            }
        })
        .abort_handle();
        Self { prefix, state, accept }
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap()
    }

    pub fn add_channel(&self, id: &str, name: &str, members: &[(&str, &str)]) {
        self.state().channels.insert(
            id.to_string(),
            FakeChannel {
                name: name.to_string(),
                members: members
                    .iter()
                    .map(|(id, name)| FakeMember { id: id.to_string(), name: name.to_string(), pan: Pan::NEUTRAL, volume: 100.0 })
                    .collect(),
            },
        );
    }

    /// 自分がVCに入る／移動する／抜ける。
    pub fn select(&self, channel: Option<&str>) {
        let mut st = self.state();
        let me = st.self_id.clone();
        for ch in st.channels.values_mut() {
            ch.members.retain(|m| m.id != me);
        }
        if let Some(id) = channel {
            st.channels.get_mut(id).unwrap().members.push(FakeMember { id: me, name: "Me".into(), pan: Pan::NEUTRAL, volume: 100.0 });
        }
        st.selected = channel.map(String::from);
        st.emit("VOICE_CHANNEL_SELECT", None, json!({ "channel_id": channel, "guild_id": channel.map(|_| "guild") }));
    }

    pub fn join(&self, channel: &str, id: &str, name: &str) {
        let mut st = self.state();
        let m = FakeMember { id: id.to_string(), name: name.to_string(), pan: Pan::NEUTRAL, volume: 100.0 };
        let data = st.voice_state_json(&m);
        st.channels.get_mut(channel).unwrap().members.push(m);
        st.emit("VOICE_STATE_CREATE", Some(channel), data);
    }

    pub fn leave(&self, channel: &str, id: &str) {
        let mut st = self.state();
        let ch = st.channels.get_mut(channel).unwrap();
        let i = ch.members.iter().position(|m| m.id == id).unwrap();
        let m = ch.members.remove(i);
        let data = st.voice_state_json(&m);
        st.emit("VOICE_STATE_DELETE", Some(channel), data);
    }

    /// Discordのクラッシュ。接続を切り、Discordの仕様どおりpanを変更前に戻す。
    pub fn crash(&self) {
        let mut st = self.state();
        for t in st.tasks.drain(..) {
            t.abort();
        }
        st.out = None;
        st.subs.clear();
        for ch in st.channels.values_mut() {
            for m in &mut ch.members {
                m.pan = Pan::NEUTRAL;
            }
        }
    }

    /// Discord 本体でミュートを切り替える（VOICE_SETTINGS_UPDATE が届く）。
    pub fn set_discord_mute(&self, mute: bool) {
        let mut st = self.state();
        st.voice_settings["mute"] = json!(mute);
        let settings = st.voice_settings.clone();
        st.emit("VOICE_SETTINGS_UPDATE", None, settings);
    }

    pub fn volume_of(&self, id: &str) -> Option<f64> {
        self.state().channels.values().flat_map(|c| c.members.iter()).find(|m| m.id == id).map(|m| m.volume)
    }

    pub fn input_volume(&self) -> f64 {
        self.state().voice_settings["input"]["volume"].as_f64().unwrap()
    }

    pub fn pan_of(&self, id: &str) -> Option<Pan> {
        self.state().channels.values().flat_map(|c| c.members.iter()).find(|m| m.id == id).map(|m| m.pan)
    }

    pub fn pans_sent_to(&self, id: &str) -> Vec<(Instant, Pan)> {
        self.state().pan_log.iter().filter(|(_, u, _)| u == id).map(|(t, _, p)| (*t, *p)).collect()
    }
}

async fn serve(pipe: tokio::net::windows::named_pipe::NamedPipeServer, st: Arc<Mutex<FakeState>>) {
    let (mut rd, mut wr) = tokio::io::split(pipe);
    let Ok((_, payload)) = read_frame(&mut rd).await else { return };
    let hello: Value = serde_json::from_slice(&payload).unwrap();
    if hello["client_id"] == "bad" {
        let close = json!({ "code": 4000, "message": "Invalid Client ID" }).to_string();
        let _ = write_frame(&mut wr, OP_CLOSE, close.as_bytes()).await;
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let ready = {
        let mut s = st.lock().unwrap();
        s.connections += 1;
        // 通知の送り先は AUTHENTICATE を済ませてから切り替える（ほかのアプリの確認だけの接続で上書きしない）
        let id = s.self_id.clone();
        json!({ "cmd": "DISPATCH", "evt": "READY", "data": { "v": 1, "user": s.user_json(&id, "Me") }, "nonce": null })
    };
    let _ = tx.send(ready);
    let writer = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            if write_frame(&mut wr, OP_FRAME, v.to_string().as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let st2 = st.clone();
    let reader = tokio::spawn(async move {
        let mut authed = false;
        while let Ok((op, payload)) = read_frame(&mut rd).await {
            if op != OP_FRAME {
                continue;
            }
            let msg: Value = serde_json::from_slice(&payload).unwrap();
            let reply = st2.lock().unwrap().respond(&msg, &mut authed, &tx);
            let _ = tx.send(reply);
        }
    });
    // 接続ごとに足す（後から来た確認だけの接続で、本命の接続を落とせなくならないように）
    st.lock().unwrap().tasks.extend([writer.abort_handle(), reader.abort_handle()]);
}

/// 偽のOAuthトークンエンドポイント（Discordの /oauth2/token 相当）。
pub struct FakeOAuth {
    pub url: String,
    pub requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl FakeOAuth {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oauth2/token", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<HashMap<String, String>>>> = Default::default();
        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let reqs = reqs.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head_end, len) = loop {
                        let n = sock.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map(|v| v.trim().parse::<usize>().unwrap())
                                .unwrap_or(0);
                            break (i + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                    let form: HashMap<String, String> = body
                        .split('&')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    reqs.lock().unwrap().push(form.clone());
                    let get = |k: &str| form.get(k).map(String::as_str);
                    let (status, body) = if get("client_secret").is_some_and(|s| s != "secret") {
                        (401, json!({ "error": "invalid_client" }))
                    } else if get("grant_type") == Some("authorization_code") && get("code") == Some("code-1") {
                        (200, json!({ "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 604800, "token_type": "Bearer" }))
                    } else if get("grant_type") == Some("refresh_token") && get("refresh_token") == Some("rt-1") {
                        (200, json!({ "access_token": "at-refreshed", "refresh_token": "rt-2", "expires_in": 604800, "token_type": "Bearer" }))
                    } else {
                        (400, json!({ "error": "invalid_grant" }))
                    };
                    let body = body.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        Self { url, requests }
    }

    pub fn count(&self, grant: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|r| r.get("grant_type").map(String::as_str) == Some(grant)).count()
    }
}

/// 呼び出しを記録するだけの偽のロビー通話。
#[derive(Default)]
pub struct FakeVoice {
    pub calls: Mutex<Vec<String>>,
    pub gains: Mutex<HashMap<String, Pan>>,
    pub metadata: Mutex<Vec<HashMap<String, String>>>,
}

impl FakeVoice {
    fn log(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn called(&self, s: &str) -> bool {
        self.calls().iter().any(|c| c == s)
    }

    pub fn gain(&self, id: &str) -> Option<Pan> {
        self.gains.lock().unwrap().get(id).copied()
    }

    pub fn last_metadata(&self) -> Option<HashMap<String, String>> {
        self.metadata.lock().unwrap().last().cloned()
    }
}

impl Voice for FakeVoice {
    fn join(&self, secret: &str, _: &[(String, String)]) {
        self.log(format!("join {secret}"));
    }
    fn leave_call(&self) {
        self.log("leave".into());
    }
    fn set_metadata(&self, metadata: &[(String, String)]) {
        self.metadata.lock().unwrap().push(metadata.iter().cloned().collect());
        self.log("metadata".into());
    }
    fn set_gain(&self, user_id: &str, pan: Pan) {
        self.gains.lock().unwrap().insert(user_id.to_string(), pan);
    }
    fn clear_gain(&self, user_id: &str) {
        self.gains.lock().unwrap().remove(user_id);
    }
    fn set_self_mute(&self, mute: bool) {
        self.log(format!("mute {mute}"));
    }
    fn set_self_deaf(&self, deaf: bool) {
        self.log(format!("deaf {deaf}"));
    }
    fn request_devices(&self) {
        self.log("devices?".into());
    }
    fn set_input_device(&self, id: &str) {
        self.log(format!("input {id}"));
    }
    fn set_output_device(&self, id: &str) {
        self.log(format!("output {id}"));
    }
    fn reauthorize(&self) {
        self.log("reauth".into());
    }
}
