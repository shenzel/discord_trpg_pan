//! Discord自動pan のコア。Tauri に依存しない部分（RPC・認証・配置・アニメーション・制御）をまとめる。

pub mod animator;
pub mod voice;
pub mod auth;
pub mod controller;
pub mod pan;
pub mod rpc;
pub mod session;
