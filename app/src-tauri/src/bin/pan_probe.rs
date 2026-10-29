//! 実機確認用の最小プローブ。
//! 「自分のアカウントで1人のpanを変えられるか」（rpc.voice.write がテスターで使えるか）と、
//! RPCのレート制限を確かめる。
//!
//!   pan-probe                              VCの参加者を表示
//!   pan-probe set <user_id> <left> <right> [--volume N]
//!                                          panを設定し、Enterまで保持（--volume は切り分け用）
//!   pan-probe sweep [user_id] [--hz N] [--secs N]
//!                                          左右に往復させてエラーと応答時間を計測
//!
//! 設定: DPAN_CLIENT_ID（必須）、DPAN_CLIENT_SECRET（開発用・必須）、DPAN_REDIRECT_URI（任意）。
//! 環境変数か、カレントフォルダから親へさかのぼって見つけた .env から読む
//! --sdk-token を付けると、lobby-probe（Social SDK）で保存したトークンで AUTHENTICATE する（secret 不要）

use std::{path::PathBuf, time::{Duration, Instant}};

use anyhow::{bail, Context, Result};
use dpan::{
    auth::{Authenticator, Exchanger, TokenStore, DISCORD_TOKEN_URL},
    pan::{Pan, Polar},
    rpc::{self, parse_voice_member, ConnectFailure, RpcClient, VoiceMember},
};
use serde_json::json;

const USAGE: &str = "\
使い方:
  pan-probe                               VCの参加者を表示
  pan-probe set <user_id> <left> <right> [--volume N]
                                          panを設定し、Enterを押すまで保持（切断するとDiscordが元に戻す）
                                          --volume は切り分け用（0〜200。本体は音量を変えない）
  pan-probe sweep [user_id] [--hz N] [--secs N]
                                          左右に往復させてエラー数と応答時間を計測（既定 10Hz・10秒）
設定: DPAN_CLIENT_ID（必須）、DPAN_CLIENT_SECRET（必須・開発用）、DPAN_REDIRECT_URI（任意）
      環境変数か .env（カレントフォルダから親へさかのぼって探す）に書く";

#[tokio::main]
async fn main() {
    // 既に環境変数にあるものは上書きしない
    let _ = dotenvy::dotenv();
    if let Err(e) = run().await {
        eprintln!("エラー: {e:#}");
        std::process::exit(1);
    }
}

fn token_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("com.shenzel.discordautopan").join("tokens.json")
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    // --sdk-token: Social SDK のログイン（PKCE、secret なし）で取ったトークンで AUTHENTICATE する
    let sdk_token = args.iter().any(|a| a == "--sdk-token");
    let args: Vec<String> = args.into_iter().filter(|a| a != "--sdk-token").collect();
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
    let client_id = var("DPAN_CLIENT_ID").context("DPAN_CLIENT_ID を .env か環境変数に書いてください")?;

    println!("[1/4] Discordに接続…");
    let conn = match rpc::connect_any(rpc::PIPE_PREFIX, &client_id).await {
        Ok(c) => c,
        Err(ConnectFailure::NotRunning) => bail!("Discordが起動していません（discord-ipc-0〜9 が見つかりません）"),
        Err(ConnectFailure::Failed(e)) => return Err(e),
        Err(ConnectFailure::OtherUsers(_)) => unreachable!("connect_any はアカウントを選ばない"),
    };
    println!("      READY: {} ({})", conn.user.display_name(), conn.user.id);

    if sdk_token {
        let path = token_path().with_file_name("sdk_token.txt");
        let text = std::fs::read_to_string(&path).with_context(|| format!("{} がありません", path.display()))?;
        let access = text.lines().next().unwrap_or_default().trim().to_string();
        println!("[2/4] Social SDK のトークンで AUTHENTICATE…（{}）", path.display());
        let data = conn.client.command("AUTHENTICATE", json!({ "access_token": access })).await?;
        println!("      認証OK: {} / スコープ: {}", data["user"]["username"], data["scopes"]);
        match conn.client.command("GET_VOICE_SETTINGS", json!({})).await {
            Ok(v) => println!("      GET_VOICE_SETTINGS OK（入力デバイス: {}）", v["input"]["device_id"]),
            Err(e) => println!("      GET_VOICE_SETTINGS 失敗: {e:#}"),
        }
    } else {
        let client_secret = var("DPAN_CLIENT_SECRET").context("DPAN_CLIENT_SECRET を .env か環境変数に書いてください")?;
        let exchanger = Exchanger::Direct {
            client_id: client_id.clone(),
            client_secret,
            token_url: DISCORD_TOKEN_URL.to_string(),
            redirect_uri: var("DPAN_REDIRECT_URI"),
        };
        println!("[2/4] 認証…");
        let mut auth = Authenticator::new(client_id, Some(exchanger), TokenStore::load(&token_path()));
        let user = auth
            .login(&conn.client, &conn.user.id, || println!("      Discordに許可ダイアログが出ています。「認証」を押してください"))
            .await?;
        println!("      認証OK: {} （トークン保存先: {}）", user.display_name(), token_path().display());
    }

    println!("[3/4] 参加中のVCを取得…");
    let data = conn.client.command("GET_SELECTED_VOICE_CHANNEL", json!({})).await?;
    if data.is_null() {
        bail!("VCに参加していません。VCに入ってから実行してください");
    }
    let members: Vec<VoiceMember> = data["voice_states"]
        .as_array()
        .map(|a| a.iter().filter_map(parse_voice_member).filter(|m| m.id != conn.user.id).collect())
        .unwrap_or_default();
    println!("      VC: {} ({})", data["name"].as_str().unwrap_or("?"), data["id"].as_str().unwrap_or("?"));
    for m in &members {
        let pan = m.pan.map(|p| format!("{{{:.2}, {:.2}}}", p.left, p.right)).unwrap_or_else(|| "-".into());
        println!("        {:<20} {}  pan={}", m.name, m.id, pan);
    }

    match args.first().map(String::as_str) {
        None => {
            println!("[4/4] 参加者の表示のみ（pan変更は set / sweep で）");
            Ok(())
        }
        Some("set") => {
            let (id, pan, volume) = match args.as_slice() {
                [_, id, l, r] => (id, Pan { left: l.parse()?, right: r.parse()? }, None),
                [_, id, l, r, flag, v] if flag == "--volume" => {
                    (id, Pan { left: l.parse()?, right: r.parse()? }, Some(v.parse::<f64>()?))
                }
                _ => bail!("{USAGE}"),
            };
            let mut set_args = json!({ "user_id": id, "pan": { "left": pan.left, "right": pan.right } });
            if let Some(v) = volume {
                set_args["volume"] = json!(v);
            }
            println!("[4/4] SET_USER_VOICE_SETTINGS {set_args}");
            let t = Instant::now();
            match conn.client.send("SET_USER_VOICE_SETTINGS", set_args, None).await {
                Ok(Ok(v)) => println!("      成功（{:?}）応答: {v}", t.elapsed()),
                Ok(Err(e)) => bail!("Discordがエラーを返しました: {e}"),
                Err(_) => bail!("接続が切れました"),
            }
            println!("      相手の声の定位が変わったか確認してください。Enterで終了（終了するとpanは元に戻ります）");
            let _ = tokio::task::spawn_blocking(|| std::io::stdin().read_line(&mut String::new())).await;
            Ok(())
        }
        Some("sweep") => {
            let mut target = None;
            let (mut hz, mut secs) = (10.0f64, 10.0f64);
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--hz" => hz = it.next().context("--hz の値")?.parse()?,
                    "--secs" => secs = it.next().context("--secs の値")?.parse()?,
                    id => target = Some(id.to_string()),
                }
            }
            let ids: Vec<String> = match target {
                Some(id) => vec![id],
                None => members.iter().map(|m| m.id.clone()).collect(),
            };
            if ids.is_empty() {
                bail!("panを変える相手がいません（自分以外がVCにいる状態で実行してください）");
            }
            sweep(&conn.client, &ids, hz, secs).await
        }
        Some(_) => bail!("{USAGE}"),
    }
}

async fn sweep(client: &RpcClient, ids: &[String], hz: f64, secs: f64) -> Result<()> {
    println!("[4/4] sweep: {}人 × {hz}Hz × {secs}秒（毎秒 {} 件）", ids.len(), ids.len() as f64 * hz);
    let start = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / hz));
    let mut waits = Vec::new();
    while start.elapsed().as_secs_f64() < secs {
        ticker.tick().await;
        // 4秒で左端→右端→左端
        let p = (start.elapsed().as_secs_f64() * std::f64::consts::PI / 2.0).sin();
        let pan = Polar::from_position(p).to_pan();
        for id in ids {
            let rx = client.set_pan(id, pan);
            let sent = Instant::now();
            waits.push(tokio::spawn(async move { (rx.await, sent.elapsed()) }));
        }
    }
    let (mut ok, mut lat) = (0usize, Vec::new());
    let mut errors: std::collections::BTreeMap<String, usize> = Default::default();
    for w in waits {
        match w.await? {
            (Ok(Ok(_)), d) => {
                ok += 1;
                lat.push(d);
            }
            (Ok(Err(e)), _) => *errors.entry(e).or_default() += 1,
            (Err(_), _) => *errors.entry("接続切断".into()).or_default() += 1,
        }
    }
    lat.sort();
    let pct = |q: f64| lat.get(((lat.len() as f64 - 1.0) * q) as usize).copied().unwrap_or_default();
    println!("      成功 {ok} 件 / エラー {} 件", errors.values().sum::<usize>());
    println!("      応答時間 p50={:?} p95={:?} max={:?}", pct(0.5), pct(0.95), pct(1.0));
    for (e, n) in &errors {
        println!("      エラー {n}件: {e}");
    }
    for id in ids {
        let _ = client.set_pan(id, Pan::NEUTRAL).await;
    }
    Ok(())
}
