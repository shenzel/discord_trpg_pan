//! RPC の認証。
//!
//! 初回だけ AUTHORIZE（Discordの許可ダイアログ）でcodeを得てトークンに交換し、保存する。
//! 以後は保存したトークンで AUTHENTICATE する。期限切れや失効時は refresh_token で更新する。
//! client_secret は配布物に入れない。交換は「直接（開発用。secretは実行時の環境変数）」か「サーバー経由」で行う。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::rpc::{RpcClient, User};

pub const SCOPES: [&str; 4] = ["rpc", "rpc.voice.read", "rpc.voice.write", "identify"];
pub const DISCORD_TOKEN_URL: &str = "https://discord.com/api/v10/oauth2/token";
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(300);
/// 期限のこれだけ前から更新対象とみなす。
const EXPIRY_MARGIN_SECS: u64 = 60;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// UNIX秒。
    pub expires_at: u64,
}

/// user_id ごとのトークンをJSONファイルに保存する（2垢でも使えるようにキーを分ける）。
pub struct TokenStore {
    path: PathBuf,
    tokens: HashMap<String, Token>,
}

impl TokenStore {
    pub fn load(path: &Path) -> Self {
        let tokens = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path: path.to_path_buf(), tokens }
    }

    pub fn get(&self, user_id: &str) -> Option<&Token> {
        self.tokens.get(user_id)
    }

    pub fn put(&mut self, user_id: &str, token: Token) -> Result<()> {
        self.tokens.insert(user_id.to_string(), token);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.tokens)?)
            .with_context(|| format!("トークンを保存できません: {}", self.path.display()))
    }
}

/// codeとトークンを交換する相手。
#[derive(Clone, Debug)]
pub enum Exchanger {
    /// Discordへ直接。開発用で、secretは実行時に渡す（ビルドに埋め込まない）。
    Direct { client_id: String, client_secret: String, token_url: String, redirect_uri: Option<String> },
    /// サーバー（Worker）経由。`POST {base_url}/oauth/token` に grant を JSON で送る。
    Server { base_url: String },
    /// secret なしで Discord へ直接（Developer Portal で Public Client を有効にしたアプリ）。
    Public { client_id: String, token_url: String, redirect_uri: Option<String> },
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

impl Exchanger {
    pub async fn exchange(&self, http: &reqwest::Client, code: &str) -> Result<Token> {
        let mut grant = vec![("grant_type", "authorization_code".to_string()), ("code", code.to_string())];
        if let Exchanger::Direct { redirect_uri: Some(uri), .. } | Exchanger::Public { redirect_uri: Some(uri), .. } = self {
            grant.push(("redirect_uri", uri.clone()));
        }
        self.post(http, grant).await
    }

    pub async fn refresh(&self, http: &reqwest::Client, refresh_token: &str) -> Result<Token> {
        self.post(http, vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh_token.to_string())])
            .await
    }

    async fn post(&self, http: &reqwest::Client, mut grant: Vec<(&str, String)>) -> Result<Token> {
        let resp = match self {
            Exchanger::Direct { client_id, client_secret, token_url, .. } => {
                grant.push(("client_id", client_id.clone()));
                grant.push(("client_secret", client_secret.clone()));
                http.post(token_url).form(&grant).send().await?
            }
            Exchanger::Public { client_id, token_url, .. } => {
                grant.push(("client_id", client_id.clone()));
                http.post(token_url).form(&grant).send().await?
            }
            Exchanger::Server { base_url } => {
                let body: HashMap<&str, String> = grant.into_iter().collect();
                http.post(format!("{}/oauth/token", base_url.trim_end_matches('/'))).json(&body).send().await?
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("トークンの取得に失敗しました（HTTP {status}）: {body}");
        }
        let r: TokenResponse = resp.json().await.context("トークン応答を解釈できません")?;
        Ok(Token { access_token: r.access_token, refresh_token: r.refresh_token, expires_at: now_secs() + r.expires_in })
    }
}

pub struct Authenticator {
    client_id: String,
    exchanger: Option<Exchanger>,
    store: TokenStore,
    http: reqwest::Client,
}

impl Authenticator {
    pub fn new(client_id: String, exchanger: Option<Exchanger>, store: TokenStore) -> Self {
        Self { client_id, exchanger, store, http: reqwest::Client::new() }
    }

    fn exchanger(&self) -> Result<&Exchanger> {
        self.exchanger
            .as_ref()
            .context("トークンの交換先がありません（開発時は DPAN_CLIENT_SECRET を設定してください）")
    }

    /// `user_id`（READYのユーザー）としてRPCを認証する。
    /// 許可ダイアログが必要になったときだけ `on_authorize` を呼ぶ。
    pub async fn login(&mut self, client: &RpcClient, user_id: &str, on_authorize: impl FnOnce()) -> Result<User> {
        if let Some(token) = self.store.get(user_id).cloned() {
            if token.expires_at > now_secs() + EXPIRY_MARGIN_SECS {
                match authenticate(client, &token).await {
                    Ok(user) => return Ok(user),
                    Err(e) => log::info!("保存済みトークンで認証できません: {e:#}"),
                }
            }
            match self.refresh(client, user_id, &token).await {
                Ok(user) => return Ok(user),
                Err(e) => log::info!("トークンを更新できません: {e:#}"),
            }
        }

        on_authorize();
        let data = client
            .request(
                "AUTHORIZE",
                json!({ "client_id": self.client_id, "scopes": SCOPES }),
                None,
                AUTHORIZE_TIMEOUT,
            )
            .await?;
        let code = data["code"].as_str().context("AUTHORIZEの応答にcodeがありません")?;
        let token = self.exchanger()?.exchange(&self.http, code).await?;
        let user = authenticate(client, &token).await?;
        self.store.put(&user.id, token)?;
        Ok(user)
    }

    async fn refresh(&mut self, client: &RpcClient, user_id: &str, old: &Token) -> Result<User> {
        let token = self.exchanger()?.refresh(&self.http, &old.refresh_token).await?;
        self.store.put(user_id, token.clone())?;
        authenticate(client, &token).await
    }
}

async fn authenticate(client: &RpcClient, token: &Token) -> Result<User> {
    let data = client.command("AUTHENTICATE", json!({ "access_token": token.access_token })).await?;
    serde_json::from_value(data["user"].clone()).context("AUTHENTICATEの応答にuserがありません")
}
