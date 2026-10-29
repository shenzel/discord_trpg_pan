// Social SDK のロビー通話を専用スレッドで動かし、Rust には小さな C の API だけを見せる。
//
// - 1 本のスレッドで複数のアカウントを動かす（RunCallbacks がプロセス全体で 1 つのため）
// - SDK の呼び出しはすべてそのスレッドで行う（コマンドはキューに積んで渡す）
// - 状態の変化は JSON 1 行のイベントでコールバックに返す。先頭に "account" が付く
// - pan の倍率（dpan_set_gain）だけはキューを通さず、受信音声のコールバックが直接読む
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void (*dpan_event_cb)(const char* json, void* ctx);

void* dpan_create(uint64_t application_id, dpan_event_cb cb, void* ctx);
void dpan_destroy(void* h);

// アカウントを足す（トークンの保存先ごと）。戻り値はアカウント番号
int dpan_add_account(void* h, const char* token_path_utf8);
void dpan_remove_account(void* h, int account);

// メタデータは "key\x1fvalue\x1ekey\x1fvalue" の形で渡す（値に JSON を入れられるよう区切りは制御文字）
void dpan_join(void* h, int account, const char* secret, const char* member_metadata);
void dpan_leave_call(void* h, int account);
void dpan_set_metadata(void* h, int account, const char* member_metadata);

void dpan_set_gain(void* h, int account, uint64_t user_id, float left, float right);
void dpan_clear_gain(void* h, int account, uint64_t user_id);

void dpan_set_self_mute(void* h, int account, int mute);
void dpan_set_self_deaf(void* h, int account, int deaf);

void dpan_request_devices(void* h, int account);
// 保存したトークンを捨てて、許可からやり直す（アカウントを替えるとき）
void dpan_reauthorize(void* h, int account);
void dpan_set_input_device(void* h, int account, const char* device_id);
void dpan_set_output_device(void* h, int account, const char* device_id);

#ifdef __cplusplus
}
#endif
