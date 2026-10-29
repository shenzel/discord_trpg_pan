//! Discord RPC クライアント（Windows 名前付きパイプ `\\?\pipe\discord-ipc-N`）。
//!
//! フレーム形式は opcode(u32 LE) + 長さ(u32 LE) + JSON。
//! コマンドの応答は nonce で対応づけ、nonce のない DISPATCH はイベントとして流す。

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::ClientOptions,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::pan::Pan;

pub const PIPE_PREFIX: &str = r"\\?\pipe\discord-ipc-";
pub const PIPE_COUNT: u8 = 10;

pub const OP_HANDSHAKE: u32 = 0;
pub const OP_FRAME: u32 = 1;
pub const OP_CLOSE: u32 = 2;
pub const OP_PING: u32 = 3;
pub const OP_PONG: u32 = 4;

const MAX_FRAME: usize = 16 << 20;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header).await?;
    let op = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok((op, payload))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, op: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf).await?;
    w.flush().await
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.username)
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub evt: String,
    pub data: Value,
}

pub type Reply = std::result::Result<Value, String>;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Reply>>>>;

#[derive(Clone)]
pub struct RpcClient {
    out: mpsc::UnboundedSender<(u32, Vec<u8>)>,
    pending: Pending,
    nonce: Arc<AtomicU64>,
}

impl RpcClient {
    /// 送信だけ行い、応答の受け口を返す。送信順は保たれる。
    pub fn send(&self, cmd: &str, args: Value, evt: Option<&str>) -> oneshot::Receiver<Reply> {
        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(nonce.clone(), tx);
        let mut msg = json!({ "cmd": cmd, "args": args, "nonce": nonce });
        if let Some(evt) = evt {
            msg["evt"] = json!(evt);
        }
        if self.out.send((OP_FRAME, msg.to_string().into_bytes())).is_err() {
            self.pending.lock().unwrap().remove(&nonce);
        }
        rx
    }

    pub async fn request(&self, cmd: &str, args: Value, evt: Option<&str>, timeout: Duration) -> Result<Value> {
        let rx = self.send(cmd, args, evt);
        match tokio::time::timeout(timeout, rx).await {
            Err(_) => bail!("{cmd}: 応答がありません"),
            Ok(Err(_)) => bail!("{cmd}: 接続が切れました"),
            Ok(Ok(Err(e))) => bail!("{cmd}: {e}"),
            Ok(Ok(Ok(v))) => Ok(v),
        }
    }

    pub async fn command(&self, cmd: &str, args: Value) -> Result<Value> {
        self.request(cmd, args, None, COMMAND_TIMEOUT).await
    }

    pub async fn subscribe(&self, evt: &str, args: Value) -> Result<()> {
        self.request("SUBSCRIBE", args, Some(evt), COMMAND_TIMEOUT).await.map(|_| ())
    }

    pub async fn unsubscribe(&self, evt: &str, args: Value) -> Result<()> {
        self.request("UNSUBSCRIBE", args, Some(evt), COMMAND_TIMEOUT).await.map(|_| ())
    }

    /// panだけを変更する（volume には触れない）。
    pub fn set_pan(&self, user_id: &str, pan: Pan) -> oneshot::Receiver<Reply> {
        self.send(
            "SET_USER_VOICE_SETTINGS",
            json!({ "user_id": user_id, "pan": { "left": pan.left, "right": pan.right } }),
            None,
        )
    }
}

pub struct Connection {
    pub client: RpcClient,
    /// READY に入っていた、このパイプの先のDiscordにログインしているユーザー。
    pub user: User,
    pub events: mpsc::UnboundedReceiver<Event>,
    /// パイプが閉じると終了する。
    pub reader: JoinHandle<()>,
    pub writer: JoinHandle<()>,
}

impl Connection {
    pub fn abort(&self) {
        self.reader.abort();
        self.writer.abort();
    }
}

#[derive(Debug)]
pub enum ConnectFailure {
    /// どのパイプも開いていない（Discordが起動していない）。
    NotRunning,
    /// パイプは開いたが、ハンドシェイクで失敗した。
    Failed(anyhow::Error),
    /// Discord は起動しているが、目的のアカウントでログインしているものがない（見つかったアカウントの名前）。
    OtherUsers(Vec<String>),
}

/// `prefix0`〜`prefix9` を順に試し、最初につながったものを返す。
pub async fn connect_any(prefix: &str, client_id: &str) -> std::result::Result<Connection, ConnectFailure> {
    let mut last_err = None;
    for i in 0..PIPE_COUNT {
        match connect(&format!("{prefix}{i}"), client_id).await {
            Ok(c) => return Ok(c),
            Err(ConnectFailure::Failed(e)) => last_err = Some(e),
            Err(_) => {}
        }
    }
    Err(last_err.map_or(ConnectFailure::NotRunning, ConnectFailure::Failed))
}

/// `user_id` でログインしている Discord を探してつなぐ（Stable と PTB を同時に起動していても取り違えない）。
pub async fn connect_user(prefix: &str, client_id: &str, user_id: &str) -> std::result::Result<Connection, ConnectFailure> {
    let mut others = vec![];
    let mut last_err = None;
    for i in 0..PIPE_COUNT {
        match connect(&format!("{prefix}{i}"), client_id).await {
            Ok(c) if c.user.id == user_id => return Ok(c),
            Ok(c) => {
                others.push(c.user.display_name().to_string());
                c.abort();
            }
            Err(ConnectFailure::Failed(e)) => last_err = Some(e),
            Err(_) => {}
        }
    }
    if !others.is_empty() {
        return Err(ConnectFailure::OtherUsers(others));
    }
    Err(last_err.map_or(ConnectFailure::NotRunning, ConnectFailure::Failed))
}

pub async fn connect(path: &str, client_id: &str) -> std::result::Result<Connection, ConnectFailure> {
    let pipe = ClientOptions::new().open(path).map_err(|_| ConnectFailure::NotRunning)?;
    handshake(pipe, client_id).await.map_err(ConnectFailure::Failed)
}

async fn handshake<P>(pipe: P, client_id: &str) -> Result<Connection>
where
    P: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(pipe);
    let hello = json!({ "v": 1, "client_id": client_id }).to_string();
    write_frame(&mut wr, OP_HANDSHAKE, hello.as_bytes()).await?;
    let (op, payload) = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut rd))
        .await
        .context("ハンドシェイクの応答がありません")??;
    let v: Value = serde_json::from_slice(&payload)?;
    if op == OP_CLOSE {
        bail!("Discordが接続を拒否しました（{}: {}）", v["code"], v["message"]);
    }
    if v["evt"] != "READY" {
        bail!("想定外のハンドシェイク応答: {v}");
    }
    let user: User = serde_json::from_value(v["data"]["user"].clone()).context("READYにuserがありません")?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<(u32, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((op, buf)) = out_rx.recv().await {
            if let Err(e) = write_frame(&mut wr, op, &buf).await {
                log::info!("RPC送信を終了: {e}");
                break;
            }
        }
    });
    let pending: Pending = Default::default();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let reader = tokio::spawn(read_loop(rd, out_tx.clone(), pending.clone(), ev_tx));

    Ok(Connection {
        client: RpcClient { out: out_tx, pending, nonce: Arc::new(AtomicU64::new(1)) },
        user,
        events: ev_rx,
        reader,
        writer,
    })
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut rd: R,
    out: mpsc::UnboundedSender<(u32, Vec<u8>)>,
    pending: Pending,
    events: mpsc::UnboundedSender<Event>,
) {
    loop {
        let (op, payload) = match read_frame(&mut rd).await {
            Ok(f) => f,
            Err(e) => {
                log::info!("RPCパイプが閉じました: {e}");
                break;
            }
        };
        match op {
            OP_PING => {
                let _ = out.send((OP_PONG, payload));
            }
            OP_CLOSE => {
                log::info!("DiscordがRPC接続を閉じました: {}", String::from_utf8_lossy(&payload));
                break;
            }
            OP_FRAME => match serde_json::from_slice::<Value>(&payload) {
                Ok(v) => dispatch(v, &pending, &events),
                Err(e) => log::warn!("RPCフレームを解釈できません: {e}"),
            },
            _ => {}
        }
    }
    // 応答待ちの送信側を落として、待っている側に切断を伝える
    pending.lock().unwrap().clear();
}

fn dispatch(v: Value, pending: &Pending, events: &mpsc::UnboundedSender<Event>) {
    if let Some(nonce) = v.get("nonce").and_then(Value::as_str) {
        if let Some(tx) = pending.lock().unwrap().remove(nonce) {
            let reply = if v["evt"] == "ERROR" {
                Err(format!("{} ({})", v["data"]["message"].as_str().unwrap_or("error"), v["data"]["code"]))
            } else {
                Ok(v["data"].clone())
            };
            let _ = tx.send(reply);
            return;
        }
    }
    if v["cmd"] == "DISPATCH" {
        if let Some(evt) = v["evt"].as_str() {
            let _ = events.send(Event { evt: evt.to_string(), data: v["data"].clone() });
        }
    }
}

/// ボイスステート（GET_SELECTED_VOICE_CHANNEL の voice_states の要素、VOICE_STATE_* の data）から取り出す情報。
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceMember {
    pub id: String,
    pub name: String,
    pub pan: Option<Pan>,
    /// Discord 側でのこの人の音量（%）。音量 0 にした後で元に戻すために使う。
    pub volume: Option<f64>,
    /// 本人が Discord でミュート中か（サーバーミュートも含む）
    pub muted: bool,
}

pub fn parse_voice_member(v: &Value) -> Option<VoiceMember> {
    let user = v.get("user")?;
    let id = user.get("id")?.as_str()?.to_string();
    let pick = |x: &Value| x.as_str().filter(|s| !s.is_empty()).map(String::from);
    let name = pick(&v["nick"])
        .or_else(|| pick(&user["global_name"]))
        .or_else(|| pick(&user["username"]))
        .unwrap_or_else(|| id.clone());
    let pan = match (v["pan"]["left"].as_f64(), v["pan"]["right"].as_f64()) {
        (Some(left), Some(right)) => Some(Pan { left, right }),
        _ => None,
    };
    let vs = &v["voice_state"];
    let muted = vs["self_mute"].as_bool().unwrap_or(false) || vs["mute"].as_bool().unwrap_or(false);
    Some(VoiceMember { id, name, pan, volume: v["volume"].as_f64(), muted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, OP_FRAME, b"{\"x\":1}").await.unwrap();
        let (op, payload) = read_frame(&mut b).await.unwrap();
        assert_eq!(op, OP_FRAME);
        assert_eq!(payload, b"{\"x\":1}");
    }

    #[test]
    fn frame_header_is_little_endian() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut buf = Vec::new();
        rt.block_on(write_frame(&mut buf, OP_HANDSHAKE, b"ab")).unwrap();
        assert_eq!(buf, [0, 0, 0, 0, 2, 0, 0, 0, b'a', b'b']);
    }

    #[test]
    fn voice_member_name_priority() {
        let v = json!({ "nick": "", "user": { "id": "1", "username": "u", "global_name": "G" }, "pan": { "left": 1.0, "right": 0.5 } });
        let m = parse_voice_member(&v).unwrap();
        assert_eq!(m.name, "G");
        assert_eq!(m.pan, Some(Pan { left: 1.0, right: 0.5 }));
        let v = json!({ "nick": "N", "user": { "id": "1", "username": "u" } });
        assert_eq!(parse_voice_member(&v).unwrap().name, "N");
        assert_eq!(parse_voice_member(&v).unwrap().pan, None);
    }
}
