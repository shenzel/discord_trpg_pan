//! Social SDK のロビー通話（C++ のブリッジ `native/voice_bridge.cpp` を FFI で呼ぶ）。
//!
//! ブリッジは専用スレッドで SDK を動かし、状態の変化を JSON のイベントで返す。
//! 制御側は `Voice` トレイト越しに使うので、テストでは偽物に差し替えられる。

use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    path::Path,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::pan::Pan;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub default: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceEvent {
    Log { message: String },
    Status { status: String, error: String, detail: i64 },
    Authorizing,
    /// 取ったトークン。Discord 本体（RPC）のログインにも使う
    Token { access_token: String, #[serde(default)] scopes: String },
    Ready { user_id: String, name: String },
    Joined { secret: String, lobby_id: String },
    CallStarted { lobby_id: String },
    /// 通話のイベントにはロビー ID が付く（空なら今の通話とみなす）。前の通話の遅れた通知を見分けるため
    Call { #[serde(default)] lobby_id: String, status: String, error: String },
    CallEnded { #[serde(default)] lobby_id: String },
    Participant { #[serde(default)] lobby_id: String, user_id: String, added: bool, #[serde(default)] name: String },
    Speaking { #[serde(default)] lobby_id: String, user_id: String, speaking: bool },
    Member { user_id: String, name: String, connected: bool, metadata: HashMap<String, String> },
    MemberRemoved { user_id: String },
    Devices { input: Vec<Device>, output: Vec<Device>, current_input: String, current_output: String },
    DeviceSet { kind: String, id: String },
    Error {
        stage: String,
        message: String,
        #[serde(default)]
        code: i64,
        #[serde(default)]
        http: i64,
        #[serde(default)]
        retryable: bool,
        #[serde(default)]
        retry_after: f64,
    },
}

/// ロビー通話の操作。実装は SDK（`SdkVoice`）とテスト用の偽物。
pub trait Voice: Send + Sync {
    /// 合言葉のロビーで通話する（参加済みなら参加し直さない）。
    fn join(&self, secret: &str, metadata: &[(String, String)]);
    /// 通話を終える（ロビーのメンバーのまま）。
    fn leave_call(&self);
    /// 自分のメンバーメタデータを書く（回数枠を 1 つ使う）。
    fn set_metadata(&self, metadata: &[(String, String)]);
    fn set_gain(&self, user_id: &str, pan: Pan);
    fn clear_gain(&self, user_id: &str);
    fn set_self_mute(&self, mute: bool);
    fn set_self_deaf(&self, deaf: bool);
    fn request_devices(&self);
    fn set_input_device(&self, id: &str);
    fn set_output_device(&self, id: &str);
    /// 保存したトークンを捨てて、許可からやり直す
    fn reauthorize(&self);
}

/// 何もしない Voice（client_id が未設定のとき）。
pub struct NoopVoice;

impl Voice for NoopVoice {
    fn join(&self, _: &str, _: &[(String, String)]) {}
    fn leave_call(&self) {}
    fn set_metadata(&self, _: &[(String, String)]) {}
    fn set_gain(&self, _: &str, _: Pan) {}
    fn clear_gain(&self, _: &str) {}
    fn set_self_mute(&self, _: bool) {}
    fn set_self_deaf(&self, _: bool) {}
    fn request_devices(&self) {}
    fn set_input_device(&self, _: &str) {}
    fn set_output_device(&self, _: &str) {}
    fn reauthorize(&self) {}
}

/// メタデータを "key\x1fvalue\x1e…" に詰める（値に JSON を入れられるよう区切りは制御文字）。
pub fn encode_metadata(metadata: &[(String, String)]) -> String {
    metadata.iter().map(|(k, v)| format!("{k}\x1f{v}")).collect::<Vec<_>>().join("\x1e")
}

pub fn parse_event(json: &str) -> Option<VoiceEvent> {
    match serde_json::from_str(json) {
        Ok(e) => Some(e),
        Err(err) => {
            log::warn!("ボイスのイベントを解釈できません: {err}: {json}");
            None
        }
    }
}

/// どのアカウントのイベントか（ブリッジは先頭に "account" を付けて返す）。
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceMsg {
    pub account: usize,
    pub event: VoiceEvent,
}

pub fn parse_msg(json: &str) -> Option<VoiceMsg> {
    let account = serde_json::from_str::<serde_json::Value>(json).ok()?["account"].as_u64().unwrap_or(0) as usize;
    Some(VoiceMsg { account, event: parse_event(json)? })
}

type EventCb = unsafe extern "C" fn(*const c_char, *mut c_void);

extern "C" {
    fn dpan_create(application_id: u64, cb: EventCb, ctx: *mut c_void) -> *mut c_void;
    fn dpan_destroy(h: *mut c_void);
    fn dpan_add_account(h: *mut c_void, token_path: *const c_char) -> i32;
    fn dpan_remove_account(h: *mut c_void, account: i32);
    fn dpan_join(h: *mut c_void, account: i32, secret: *const c_char, metadata: *const c_char);
    fn dpan_leave_call(h: *mut c_void, account: i32);
    fn dpan_set_metadata(h: *mut c_void, account: i32, metadata: *const c_char);
    fn dpan_set_gain(h: *mut c_void, account: i32, user_id: u64, left: f32, right: f32);
    fn dpan_clear_gain(h: *mut c_void, account: i32, user_id: u64);
    fn dpan_set_self_mute(h: *mut c_void, account: i32, mute: i32);
    fn dpan_set_self_deaf(h: *mut c_void, account: i32, deaf: i32);
    fn dpan_request_devices(h: *mut c_void, account: i32);
    fn dpan_reauthorize(h: *mut c_void, account: i32);
    fn dpan_set_input_device(h: *mut c_void, account: i32, id: *const c_char);
    fn dpan_set_output_device(h: *mut c_void, account: i32, id: *const c_char);
}

unsafe extern "C" fn on_event(json: *const c_char, ctx: *mut c_void) {
    let tx = &*(ctx as *const mpsc::UnboundedSender<VoiceMsg>);
    let json = CStr::from_ptr(json).to_string_lossy();
    if let Some(m) = parse_msg(&json) {
        let _ = tx.send(m);
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

/// Social SDK を動かすスレッド（プロセスに 1 つ）。アカウントは `add_account` で足す。破棄すると止める。
pub struct SdkHost {
    handle: *mut c_void,
    ctx: *mut mpsc::UnboundedSender<VoiceMsg>,
}

// ハンドルはブリッジ側でスレッド安全（コマンドはキュー、倍率はロック）
unsafe impl Send for SdkHost {}
unsafe impl Sync for SdkHost {}

impl SdkHost {
    pub fn start(application_id: u64, events: mpsc::UnboundedSender<VoiceMsg>) -> Arc<Self> {
        let ctx = Box::into_raw(Box::new(events));
        let handle = unsafe { dpan_create(application_id, on_event, ctx as *mut c_void) };
        Arc::new(Self { handle, ctx })
    }

    /// アカウントを足す（トークンの保存先ごと）。ログインはすぐ始まる。
    pub fn add_account(self: &Arc<Self>, token_path: &Path) -> AccountVoice {
        let path = cstr(&token_path.to_string_lossy());
        let idx = unsafe { dpan_add_account(self.handle, path.as_ptr()) };
        AccountVoice { host: self.clone(), idx }
    }

    pub fn remove_account(&self, idx: i32) {
        unsafe { dpan_remove_account(self.handle, idx) }
    }
}

impl Drop for SdkHost {
    fn drop(&mut self) {
        unsafe {
            dpan_destroy(self.handle);
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// 1 つのアカウントのロビー通話。
#[derive(Clone)]
pub struct AccountVoice {
    host: Arc<SdkHost>,
    idx: i32,
}

impl AccountVoice {
    pub fn index(&self) -> usize {
        self.idx as usize
    }

    pub fn remove(&self) {
        self.host.remove_account(self.idx);
    }
}

fn uid(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

impl Voice for AccountVoice {
    fn join(&self, secret: &str, metadata: &[(String, String)]) {
        let (s, m) = (cstr(secret), cstr(&encode_metadata(metadata)));
        unsafe { dpan_join(self.host.handle, self.idx, s.as_ptr(), m.as_ptr()) }
    }
    fn leave_call(&self) {
        unsafe { dpan_leave_call(self.host.handle, self.idx) }
    }
    fn set_metadata(&self, metadata: &[(String, String)]) {
        let m = cstr(&encode_metadata(metadata));
        unsafe { dpan_set_metadata(self.host.handle, self.idx, m.as_ptr()) }
    }
    fn set_gain(&self, user_id: &str, pan: Pan) {
        unsafe { dpan_set_gain(self.host.handle, self.idx, uid(user_id), pan.left as f32, pan.right as f32) }
    }
    fn clear_gain(&self, user_id: &str) {
        unsafe { dpan_clear_gain(self.host.handle, self.idx, uid(user_id)) }
    }
    fn set_self_mute(&self, mute: bool) {
        unsafe { dpan_set_self_mute(self.host.handle, self.idx, mute as i32) }
    }
    fn set_self_deaf(&self, deaf: bool) {
        unsafe { dpan_set_self_deaf(self.host.handle, self.idx, deaf as i32) }
    }
    fn request_devices(&self) {
        unsafe { dpan_request_devices(self.host.handle, self.idx) }
    }
    fn set_input_device(&self, id: &str) {
        let id = cstr(id);
        unsafe { dpan_set_input_device(self.host.handle, self.idx, id.as_ptr()) }
    }
    fn set_output_device(&self, id: &str) {
        let id = cstr(id);
        unsafe { dpan_set_output_device(self.host.handle, self.idx, id.as_ptr()) }
    }
    fn reauthorize(&self) {
        unsafe { dpan_reauthorize(self.host.handle, self.idx) }
    }
}

/// Discord（RPC）のデバイスに対応する SDK のデバイスを探す。ID が一致しなければ名前で照合する。
pub fn match_device<'a>(sdk: &'a [Device], discord_id: &str, discord_name: Option<&str>) -> Option<&'a Device> {
    if discord_id.is_empty() || discord_id == "default" {
        return sdk.iter().find(|d| d.default);
    }
    if let Some(d) = sdk.iter().find(|d| d.id == discord_id) {
        return Some(d);
    }
    let name = discord_name?.trim();
    if name.is_empty() {
        return None;
    }
    sdk.iter()
        .find(|d| d.name == name)
        .or_else(|| sdk.iter().find(|d| !d.name.starts_with("default") && (d.name.contains(name) || name.contains(&d.name))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bridge_events() {
        let e = parse_event(r#"{"type":"participant","lobby_id":"7","user_id":"42","added":true,"name":"A"}"#).unwrap();
        assert_eq!(e, VoiceEvent::Participant { lobby_id: "7".into(), user_id: "42".into(), added: true, name: "A".into() });
        let e = parse_event(r#"{"type":"member","user_id":"1","name":"G","connected":true,"metadata":{"gm":"1"}}"#);
        assert!(matches!(e, Some(VoiceEvent::Member { metadata, .. }) if metadata["gm"] == "1"));
        assert_eq!(parse_event(r#"{"type":"call_ended"}"#), Some(VoiceEvent::CallEnded { lobby_id: String::new() }));
        assert!(parse_event("not json").is_none());
        let m = parse_msg(r#"{"account":1,"type":"call_ended","lobby_id":"9"}"#).unwrap();
        assert_eq!(m, VoiceMsg { account: 1, event: VoiceEvent::CallEnded { lobby_id: "9".into() } });
    }

    #[test]
    fn metadata_encoding() {
        let m = vec![("gm".to_string(), "1".to_string()), ("roles".to_string(), r#"{"a":"pl"}"#.to_string())];
        assert_eq!(encode_metadata(&m), "gm\x1f1\x1eroles\x1f{\"a\":\"pl\"}");
    }

    #[test]
    fn device_matching() {
        let sdk = vec![
            Device { id: "{0.0.1}.{a}".into(), name: "default (マイク (Virtual Desktop Audio))".into(), default: true },
            Device { id: "{0.0.1}.{b}".into(), name: "INPUT (2- Volt 1)".into(), default: false },
        ];
        assert_eq!(match_device(&sdk, "default", None).unwrap().id, "{0.0.1}.{a}");
        assert_eq!(match_device(&sdk, "{0.0.1}.{b}", None).unwrap().name, "INPUT (2- Volt 1)");
        assert_eq!(match_device(&sdk, "other-id", Some("INPUT (2- Volt 1)")).unwrap().id, "{0.0.1}.{b}");
        assert!(match_device(&sdk, "other-id", Some("Nothing")).is_none());
    }
}
