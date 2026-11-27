# WebPConfig 全フィールド対応表（D-ENC 補完）

設計書 [`img2webp-rust-design-spec.md`](img2webp-rust-design-spec.md) §6 Phase 0 の D-ENC 検証項目「`WebPConfig` の全フィールド対応表の作成」に対応する成果物。Phase 0 記録 [`img2webp-rust-phase0.md`](img2webp-rust-phase0.md) §2 の D-ENC 行から参照する。

- 作成日: 2026-11-27（完了申告の照合で欠落が判明したため追加）
- 対象: `libwebp-sys` 0.14.4 が同梱する libwebp **1.6.0**
- 一次資料:
  - 構造体定義: `libwebp-sys-0.14.4/vendor/src/webp/encode.h` の `struct WebPConfig`（29 フィールド）
  - 既定値: 同 `vendor/src/enc/config_enc.c` の `WebPConfigInitInternal`
  - Pillow 側の設定値: Pillow 13.0.0 の `PIL/WebPImagePlugin.py`（`_save` / `_save_all`）と、そこから呼ばれる `_webp.c` の `WebPEncode` / `WebPAnimEncoder.add` の引数
- 実装: `rust/src/webp_encode.rs::init_config`（静止画・アニメーション共通）

## 1. 初期化の手順

```rust
WebPConfigInitInternal(&mut config, WEBP_PRESET_DEFAULT, quality, WEBP_ENCODER_ABI_VERSION)
```

プリセットは `WEBP_PRESET_DEFAULT`（`PICTURE`/`PHOTO` 等のプリセット上書きを一切受けない）。この呼び出しで 29 フィールドすべてが libwebp の既定値に設定され、その後に必要なフィールドだけを上書きする。`webp` クレートの `WebPConfig::new()` は decoder ABI を渡して初期化に失敗するため使わない（D-ENC）。

## 2. 全フィールド対応表

凡例: 「libwebp 既定」= `WebPConfigInitInternal`（`WEBP_PRESET_DEFAULT`）が入れる値。「Pillow 静止画」= `_save` → `_webp.WebPEncode` が渡す値。「Pillow アニメ」= `_save_all` → `WebPAnimEncoder.add` が渡す値。「img2webp」= `init_config` の設定値。

| # | フィールド | libwebp 既定 | Pillow 静止画 | Pillow アニメ | img2webp 静止画 | img2webp アニメ | 一致 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `lossless` | 0 | `lossless` kwarg（既定 `False`） | 同左 | `config.lossless` | 同左 | ○ |
| 2 | `quality` | 呼び出し引数（`WebPConfigInit` は 75） | `quality` kwarg（Pillow 既定 80） | 同左 | `config.quality`（既定 90） | 同左 | ○（同じ値を渡す） |
| 3 | `method` | 4 | `method` kwarg（Pillow 既定 **4**） | `method` kwarg（Pillow 既定 **0**） | `method_still()`（既定 4） | `method_animation()`（既定 0） | ○ |
| 4 | `image_hint` | `WEBP_HINT_DEFAULT` | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 5 | `target_size` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 6 | `target_PSNR` | 0.0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 7 | `segments` | 4 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 8 | `sns_strength` | 50 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 9 | `filter_strength` | 60 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 10 | `filter_sharpness` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 11 | `filter_type` | 1 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 12 | `autofilter` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 13 | `alpha_compression` | 1 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 14 | `alpha_filtering` | 1 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 15 | `alpha_quality` | 100 | `alpha_quality` kwarg（既定 100） | 同左 | **明示的に 100** | **明示的に 100** | ○ |
| 16 | `pass` | 1 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 17 | `show_compressed` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 18 | `preprocessing` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 19 | `partitions` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 20 | `partition_limit` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 21 | `emulate_jpeg_size` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 22 | `thread_level` | 0 | 設定しない（0 のまま） | 同左 | **明示的に 0** | **明示的に 0** | ○（§3.6 の方針を明示化） |
| 23 | `low_memory` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 24 | `near_lossless` | 100 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 25 | `exact` | 0 | `exact` kwarg（既定 `False`） | **渡さない**（0 のまま） | `config.exact` | **0 固定** | ○ |
| 26 | `use_delta_palette` | 0（予約） | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 27 | `use_sharp_yuv` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 28 | `qmin` | 0 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |
| 29 | `qmax` | 100 | 設定しない | 設定しない | 設定しない | 設定しない | ○ |

`WebPValidateConfig` は `init_config` の末尾で必ず呼び、範囲外があればジョブ単位の失敗にする。

## 3. この表から確定する 4 つの判断

1. **明示設定は 6 フィールドだけ**（`quality` / `lossless` / `method` / `exact` / `alpha_quality` / `thread_level`）。残り 23 フィールドは libwebp の既定値に委ねる。Pillow も同じ 6 フィールド（アニメの `exact` を除く）しか設定しないため、既定値のまま一致する。
2. **`alpha_quality = 100` と `thread_level = 0` は「明示するが値は既定と同じ」**。前者は Pillow が明示する値に合わせるため、後者はジョブ並列との二重並列化を避ける設計判断（§3.6）を既定値に依存させないため。
3. **アニメーションの `exact` は 0 固定が正しい**。Pillow の `_save_all` は `exact` を `WebPAnimEncoder.add` に渡さないため、Pillow 側も 0 になる。`img2webp` の `encode_animation` が `exact` を無視するのは**仕様どおり**で、欠落ではない（照合時に疑義が出たため明記する）。
4. **`quality` の既定値だけは Pillow（80）と異なる 90 を使う**。これは本ツールの設定値であり、同じ 90 を両実装に渡して比較している（`rust/tests/compare_with_python.py` は `config.ini` の 90 を使う）。既定値の差ではなく設定値の差。

## 4. 裏付け

- 画素一致: `rust/tests/compare_with_python.py` の lossy／lossless／`method 0`／`exact`／metadata-off の各変種で、許容基準（§3.1）内で一致する。
- アニメーションの `method` 既定 0 は、`config.rs::method_animation` の既定値と `compare_with_python.py` の `anim.gif` 一致で確認する。
- libwebp の版は実行時 `WebPGetEncoderVersion()`（1.6.0）を変換シグネチャに含める（D-LOCK）。
