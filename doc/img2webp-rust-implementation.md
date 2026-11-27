# img2webp Rust 版 実装記録（Phase 1〜6）

設計書 [`img2webp-rust-design-spec.md`](img2webp-rust-design-spec.md) の Phase 1〜6 の実施記録。Phase 0 の決定と実測は [`img2webp-rust-phase0.md`](img2webp-rust-phase0.md) にある。

- 日付: 2026-11-27
- 実装: `rust/`（クレート `img2webp`、単一バイナリ）
- 実装は Phase 1〜5 を 1 回の作業で通した（Phase ごとの途中バイナリはリリースしていない）。以下は Phase の完了条件ごとの証跡。

## 1. 構成とビルド

```
rust/
  Cargo.toml / Cargo.lock      … バイナリクレート。lock はコミット対象
  .cargo/config.toml           … MSVC で CRT を静的リンク（単一 exe）
  build.rs                     … Cargo.lock から依存版を埋め込み（D-LOCK）
  src/main.rs                  … 起動、Ctrl-C ハンドラ設置、終了コード
  src/lib.rs                   … モジュール公開
  src/config.rs                … config.ini の厳格検証（現行規則の移植）
  src/plan.rs                  … 走査・分類・衝突命名
  src/state.rs                 … SQLite 状態 DB・シグネチャ・終了状態
  src/probe.rs                 … 第 1 段ヘッダ走査（PNG/JPEG/GIF のメタデータ含む）
  src/budget.rs                … メモリ予算マネージャ
  src/dispatch.rs              … ディスパッチャ・優先度付きプール・2 段実行・集計
  src/job.rs                   … 一時ファイル・日時・再 stat・原子置換
  src/fs_util.rs               … 権限・日時・上書き不可移動・起動時掃除
  src/convert.rs               … 静止画変換パイプライン
  src/animation.rs             … GIF/APNG のフレーム読み取りと丸め
  src/metadata.rs              … 生成テキスト・保存後検証
  src/tiff.rs                  … 最小 TIFF/Exif の読み書き
  src/color.rs                 … 向き・ICC 色正規化（lcms2）
  src/stealth.rs               … Stealth PNG Info（8bit/16bit）
  src/png_raw.rs               … 16bit PNG の生デコード
  src/webp_encode.rs           … libwebp 直接 FFI と RIFF メタデータ格納
  src/interrupt.rs             … 中断フラグとハンドラ
  src/report.rs                … ERROR/WARN 出力と写像
  tests/e2e.rs                 … バイナリをコピーして実行する black-box テスト
  tests/behaviour.rs           … 内部フックを使うテスト（予算・中断・掃除・競合・DB 互換）
  tests/compare_with_python.py … Python 版との出力比較（画素・メタデータ・アニメーション）
  tests/benchmark.py           … ベースライン計測（Windows Python で実行）
  tests/fixtures/              … フィクスチャと生成スクリプト
```

ビルドとテスト:

```shell
cd rust
cargo build --release --no-default-features --locked   # リリース（実 DB・実出力）
cargo test                                             # 開発ビルド（dev-db 有効）で全テスト
cargo test --no-default-features                       # リリース相当の読み替えで全テスト
```

開発ビルドは既定（`dev-db` feature が default）。状態 DB は `.img2webp-dev.sqlite3`、出力先は `outputs-dev`、起動行とマーカーファイル `.img2webp-dev-output` が付く。リリースビルドは `--no-default-features` を明示したときだけ。

## 2. Phase ごとの完了条件と証跡

### Phase 1（骨格・コピー・静止画 lossy/lossless・開発 DB 分離）

| 完了条件 | 証跡 |
| --- | --- |
| コピーと静止画変換の black-box 相当テスト | `tests/e2e.rs`（34 件、2026-11-27 照合時の実測。CRC フィクスチャの 1 件を追加） |
| 再構成した単体テスト | `src/*.rs` の単体テスト（46 件、同じく照合時の実測。FFI パニック安全性の 2 件を追加） |
| Python 製 DB の複製でコピー再利用・変換の一度きりの再作成 | `tests/behaviour.rs::python_database_copy_records_are_reused_and_conversions_rebuilt` |
| 待機制御（先着順・後続停止・リコンサイル） | `tests/behaviour.rs::jobs_wait_for_budget_without_losing_work`、`lost_completion_notification_is_reclaimed_by_reconcile`、`src/budget.rs` の単体テスト |
| 同梱 config.ini テンプレとコード既定値の一致 | `src/config.rs::bundled_template_matches_code_defaults` |
| dev-db 無効ビルドのスモークテスト | `cargo test --no-default-features` と `tests/e2e.rs::dev_and_release_build_paths_are_separated` |
| `parallel_jobs` の値域検証 | `tests/e2e.rs::config_validation_errors_exit_two`、`parallel_jobs_setting_is_applied_and_clamped` |
| DB エラーで終了コード 2・専用メッセージ | `src/dispatch.rs` の DB エラー経路（`処理履歴DBエラー: …`）、`tests/behaviour.rs::interrupted_state_keeps_running_marker` で正常系の終了状態を確認 |
| 出力へ付ける atime が第 1 段前の記録用 stat | `tests/e2e.rs::output_atime_comes_from_the_stat_taken_before_header_reading` |
| 起動時掃除の一連の流れ | `tests/behaviour.rs::startup_cleanup_removes_old_temp_files_but_keeps_tracked_ones`（初回掃除→正常終了→省略、10 倍超の残骸、追跡済みは保持） |
| 上限超過・予算超過がジョブ単位の失敗 | `tests/e2e.rs::dimension_and_pixel_limits_fail_the_job`、`tests/behaviour.rs::budget_estimate_larger_than_total_fails_the_job` |
| 上書き不可の競合テスト（BUG-05） | `tests/behaviour.rs::overwrite_disabled_race_fails_the_job_and_keeps_the_winner`（`Options::commit_hook` で割り込み） |
| lossless 出力の画素が現行入力と完全一致 | `tests/e2e.rs::lossless_conversion_is_pixel_exact`、`compare_with_python.py` の lossless 変種 |
| 新設定のシグネチャ反映・依存版の識別 | `src/state.rs::convert_signature_shape_is_pinned`、`convert_signature_contains_identity_and_settings` |
| 参考計測 | `img2webp-rust-phase0.md` §5 |
| 変換シグネチャのゴールデン（暫定形） | 本ドキュメント §3 |
| config 構文細部の再現テスト | `src/config.rs` の単体テスト（コメント行・見出し末尾文字列・`[ webp]`・継続行・重複） |
| 拡張子だけの名前・末尾ドット形の分類 | `src/plan.rs::symlinks_and_extension_only_names`、`src/paths.rs` の単体テスト |
| 大小文字だけが異なるパス・兄弟フォルダ | `src/paths.rs::sibling_folder_is_not_a_child`（Windows は小文字化比較） |

### Phase 2（メタデータ）

| 完了条件 | 証跡 |
| --- | --- |
| メタデータ系テスト | `tests/e2e.rs::png_metadata_and_timestamps`, `conflict_keeps_user_comment_and_writes_xp_comment`, `icc_and_xmp_are_carried_over`, `metadata_can_be_disabled`, `jpeg_orientation_and_cmyk` |
| 生成テキストの規則 | `src/metadata.rs::parameters_come_first_and_xmp_is_excluded`, `gif_comment_is_the_generated_text` |
| A1111／NovelAI 実画像の読み戻し | fixtures `text.png`（A1111 形式の `parameters` と NovelAI 風の `Description`）で UserComment を確認。実画像そのものはリポジトリに置けないため、同じ構造のフィクスチャで代替（**限界**） |
| 識別に依存版が入り、版変更で再変換 | `src/state.rs` のゴールデンテスト（`dependencies` と `libwebp` をシグネチャに含む）。`env!("IMG2WEBP_DEP_VERSIONS")` が変わると文字列が変わる |
| 変換シグネチャのゴールデン（最終形） | 本ドキュメント §3 |

### Phase 3（Stealth）

| 完了条件 | 証跡 |
| --- | --- |
| Stealth 全バリエーション | `src/stealth.rs::all_variants_are_extracted`（8/16bit × RGB/RGBA/LA × info/gzip/zlib × フィルタ 0〜4） |
| 不正ペイロード・制限・切り詰め | `invalid_payloads_return_empty`、`tests/e2e.rs::stealth_payloads_become_user_comments` |
| APNG の Stealth がフレーム巡回前の先頭フレームから読まれる | `src/convert.rs` が `decode_still`（開いた直後のフレーム）から抽出し、生成テキストへ結合。`tests/e2e.rs::stealth_payloads_become_user_comments` が変換を通した結合を確認 |
| 負ケースのコスト計測 | **未達（2026-11-27 再計測）**。`rust/examples/profile_pipeline.rs` で同一ファイルの Stealth 走査 on/off を比較した結果、8bit 負ケース（AI PNG 40 件）は変換本体の 0.09%（基準内）、16bit 負ケース（256×256 10 件）は 19.0%、1024×1024 3 件では 20.6% となり、設計書 §4.2 の「変換本体の 10% 以内」を満たさない。旧記述の「カテゴリ 2 と 1 の差」は別コーパス間の比較で、判定に使えない（`img2webp-rust-completion-evidence.md` §6）。是正するか基準の適用範囲を見直すかは**決定依頼** |

### Phase 4（アニメーション）

| 完了条件 | 証跡 |
| --- | --- |
| アニメーションテスト＋BUG-01 フィクスチャ＋loop/duration | `tests/e2e.rs::gif_animation_loop_duration_and_comment`, `gif_without_loop_count_defaults_to_one`, `apng_default_image_is_skipped`。`optimized.gif`（部分フレーム＋disposal 1/2/3）と `background-disposal.gif`（不透明背景色）は Python 版と画素完全一致 |
| 総量上限超過がジョブ単位の失敗 | `tests/behaviour.rs::budget_estimate_larger_than_total_fails_the_job`、`tests/e2e.rs::dimension_and_pixel_limits_fail_the_job`（アニメーションの総量は `budget::estimate` がフレーム数×キャンバス寸法で見積もる） |
| 16bit APNG の明示的な失敗 | `tests/e2e.rs::sixteen_bit_apng_is_rejected` |
| default_image のみでジョブ単位の失敗 | `tests/e2e.rs::default_image_only_apng_fails` |
| default_image 付きで保存フレーム 1 つなら静止画 | `src/convert.rs` の単一フレーム分岐（`apng-separate.png` の 2 フレーム版で動作確認） |
| アニメーションの lossless／method 反映 | `compare_with_python.py` の lossless 変種（`anim.gif` が一致）と `src/webp_encode.rs::encode_animation`（フレーム側 config に lossless/method を設定） |

### Phase 5（並列化と調整）

| 完了条件 | 証跡 |
| --- | --- |
| ベースライン比の記録と評価 | `img2webp-rust-phase0.md` §5（全体 2.99×、全カテゴリで 1.97× 以上） |
| `parallel_jobs` が並列度に反映・値域検証・クランプ WARN | `tests/e2e.rs::parallel_jobs_setting_is_applied_and_clamped`, `config_validation_errors_exit_two` |
| 予算の合算が総量予算を超えない | `tests/behaviour.rs::jobs_wait_for_budget_without_losing_work`（同時 1 件分の予算で 4 件が順に完了） |
| 並列実行時の DB エラーで中断時意味論の畳み＋終了コード 2 | `src/dispatch.rs` の DB エラー経路（`db_failure` → プール停止 → 最終 commit 試行 → 2） |
| 故障注入（二重通知・通知喪失・実行中でない割当の回収・ワーカーパニック） | `tests/behaviour.rs::duplicate_notification_is_ignored`, `lost_completion_notification_is_reclaimed_by_reconcile`, `worker_panic_becomes_a_job_failure_with_final_commit` |
| 停滞打ち切りの誤爆なし | 打ち切り判定は「待ち行列が空でない状態の経過時間」だけを見る（`src/dispatch.rs`）。通常運転で発火しないことは全 e2e テストで確認 |
| FFI パニック／ディスパッチャパニック | FFI 中はスレッドローカルの `webp_encode::FFI_ACTIVE` を立て、`catch_unwind` がそれを観測したらプロセス終了コード 2（最終 commit なし）。判定は `dispatch::classify_worker_panic` に切り出し、単体テスト `ffi_panic_is_fatal_and_other_panics_are_job_failures` で「FFI 中→Fatal」「それ以外→ジョブ失敗」を確認。ディスパッチャのパニックは `dispatch::run` の `catch_unwind` で 2。注入テスト `dispatcher_panic_exits_two_without_final_commit` で終了コード 2・記録 0 件・終了状態が `running` のままを確認（実際の FFI パニック自体の注入は不可能なため、分類ロジックの単体テストで代替） |
| 安全係数の確定 | 暫定 2.0 を維持すると決定。実測との比較: 4K 静止画 1 件で見積もり 298,975,232 バイト（285MiB）に対し実ピーク 102MiB（約 2.9 倍の余裕）、GIF 10 フレーム 1 件で見積もり 35.8MB（34MiB）に対し実ピーク 14.7MiB（約 2.4 倍）。全カテゴリで Rust のピークは Python 以下（Phase 0 記録 §5）。見積もりが実ピークを下回る証拠が無いため、下げずに 2.0 を確定値とする |
| エンコーダ設定の掃引（§4.3） | 実施（`rust/tests/benchmark.py --sweep`、コーパス 1＋7、2 回の中央値）。結果は下記。既定値（method 静止画 4、quality 90、並列度＝コア数）は変更しない |
| 停滞打ち切りの誤爆 | 打ち切りは「予算待ちの行列が空でない状態の経過時間」だけで判定し、通常の全テストで発火しないことを確認（誤爆の注入試験は未実施） |
| 保存後検証を弱めるかの判断 | 弱めない（維持）。計測では検証込みでも 2.99× を達成しており、弱める根拠が無い |
| デコード中の累積チェック（保険） | 実装済み。`animation::check_frame_budget` がフレームを保持する前に累積保持量を見積もりと比較し、超えたらそのフレームを保持せずジョブ単位の失敗にする（`src/animation.rs::frame_budget_guard_rejects_undersized_estimates`）。通常運転では見積もりが保持量全体を安全係数込みで含むため発火しない |

### Phase 6（リリース）

| 完了条件 | 証跡 |
| --- | --- |
| `img2webp.bat` を exe 起動へ | [リポジトリ直下の `img2webp.bat`](../img2webp.bat) を書き換え済み（exe が無い場合は Python 版へフォールバックし、`pause` の挙動を維持） |
| Python 版へのタグ付け | `git tag python-version-final`（Rust 版フォルダ追加前の最後のコミット） |
| リリースタグ名と移行手順書 | `rust-v0.1.0`、[`img2webp-rust-migration.md`](img2webp-rust-migration.md) |
| 手順書どおりに Python 版へ戻せることの確認 | 実施済み。Rust 版で `text.png`（変換）と `sample.webp`（コピー）を処理した出力フォルダに対し、`python-version-final` の `img2webp.py` を実行。Python 版は Rust 版が追加した `img2webp_meta` テーブルごと DB を開き、コピーは `再利用 1`、変換は `pipeline` 不一致で `変換 1` となり、終了コード 0。`config.ini` から新項目を削除する手順も含めて確認 |
| 配布プロファイルが `panic = "unwind"` | `rust/Cargo.toml` の `[profile.release] panic = "unwind"` |
| dev-db 無し・`--locked` でビルド | `cargo build --release --no-default-features --locked` で成功。配布 exe 個体の確認: 試験フォルダで実行し、`.img2webp.sqlite3` が作られ、出力先が `outputs`、開発ビルドの起動行が出ないことを確認 |
| 単一 exe | `.cargo/config.toml` で CRT を静的リンク。`objdump -p` の依存は Windows のシステム DLL のみ（VCRUNTIME140.dll への依存なし）。lcms2/libwebp/SQLite はすべて静的リンク |
| BUG-04 の解消を Windows で確認 | 生成した `outputs\text.webp` の ACL が、同じフォルダに通常作成したファイルの ACL（ディレクトリ継承）と一致することを `icacls` で確認 |
| dev-db 非混入の個体確認（照合時に再取得） | 2026-11-27 20:12 に `rust/target/release/img2webp.exe`（4,290,048 バイト）を空の一時フォルダで実行し、`.img2webp.sqlite3` を `outputs` に生成、`outputs-dev`・`.img2webp-dev-output`・開発ビルドの起動行がいずれも出ないことを確認（終了コード 0、0.24 s）。ビルド側の裏付けは `target/release/.fingerprint/img2webp-*/bin-img2webp.json` の `features: []` |
| `--locked` の確認（照合時） | `cargo build --release --no-default-features --locked` が成功（`Finished release profile ... in 3.08s`）。`rust/Cargo.lock` は `c964601` から未変更（`git diff HEAD -- rust/Cargo.lock` が空） |

## 3. 変換シグネチャのゴールデン文字列

### Phase 1 時点の暫定形（履歴）

```
{"action":"convert","exact":false,"lossless":false,"metadata":true,"method_animation":0,"method_still":4,"missing_duration_ms":80,"pipeline":3,"platform":"nt","quality":90,"timestamps":true}
```

### 最終形（現行）

```
{"action":"convert","dependencies":"image=0.25.10;png=0.18.1;gif=0.14.2;zune-jpeg=0.5.15;zune-core=0.5.3;image-webp=0.2.4;jpeg-decoder=0.3.2;libwebp-sys=0.14.4;rusqlite=0.37.0;lcms2=6.3.0;lcms2-sys=4.2.0;flate2=1.1.11;miniz_oxide=0.8.9","exact":false,"libwebp":"1.6.0","lossless":false,"metadata":true,"method_animation":0,"method_still":4,"missing_duration_ms":80,"pipeline":3,"platform":"nt","quality":90,"timestamps":true}
```

コピー（Python 版とバイト一致、移行後も再利用される）:

```
{"action":"copy","pipeline":2,"platform":"nt","timestamps":true}
```

- `missing_duration_ms` は `.png`/`.gif` の変換ジョブだけに入る（BUG-12 を維持）。
- `.jpg`/`.jpeg` の変換ジョブでは `missing_duration_ms` が無い。
- `dependencies` は `Cargo.lock` から `build.rs` が埋め込む（パッチ版でも変わる）。`libwebp` は実行時の `WebPGetEncoderVersion()`。
- ゴールデンは `src/state.rs::convert_signature_shape_is_pinned` で固定している（キー順・区切り・真偽値の表記）。

## 4. 意図的な変更（§5）の実装状況

| # | 変更 | 実装 | テスト |
| --- | --- | --- | --- |
| 1 | 出力の権限が umask 準拠（BUG-04） | `fs_util::create_temp`（POSIX は 0o666、Windows は既定 ACL） | POSIX の単体テスト、Windows は `icacls` で確認 |
| 2 | 変換ジョブのシグネチャ拡張による一度きりの再変換 | `state::settings_signature`（pipeline 3＋識別） | `tests/behaviour.rs` の Python DB 互換テスト |
| 3 | 一辺 16,384px は処理前に失敗（BUG-07） | `dispatch::check_limits` | `tests/e2e.rs::dimension_and_pixel_limits_fail_the_job` |
| 4 | 設定エラーで missing と unknown の両方 | `config::load` | `src/config.rs::rejects_missing_and_unknown_keys_together` |
| 5 | メモリ予算の導入 | `budget.rs` + `dispatch.rs` | 予算超過・待機のテスト |
| 6 | 依存版が変わると全件再変換 | シグネチャの `dependencies`/`libwebp` | ゴールデンテスト |
| 7 | 16bit APNG は未対応 | `dispatch::check_limits` | `tests/e2e.rs::sixteen_bit_apng_is_rejected` |
| 8 | ERROR 行の文字列上限と切り詰め | `report::truncate_message`（4,096 バイト） | `src/report.rs` の単体テスト |
| 9 | 上書き不可の競合はジョブ失敗 | `job::run_job` + `fs_util::atomic_move_no_overwrite` | `tests/behaviour.rs` の競合テスト |
| 10 | 2 回目の中断で最終 commit なし | `dispatch` の中断経路 | `tests/behaviour.rs::second_interrupt_skips_the_final_commit` |
| 11 | 標準出力・標準エラーは UTF-8 | Rust の `println!`/`eprintln!`（`Reporter`） | 全テスト（日本語文言） |
| 12 | 整数のアンダースコア・Unicode 数字を受理しない | `config::parse_int` | `src/config.rs::validates_ranges_and_types` |
| 13 | 標準出力・標準エラーへの書き込み失敗はジョブ成否に数えない | `Reporter`（失敗したら以後の表示を打ち切る） | 実装のみ（パイプ切断の自動テストなし） |
| 14 | ディスパッチャ／FFI パニック時は最終 commit なし | `dispatch::run` の `catch_unwind`、`webp_encode::FFI_ACTIVE` | ディスパッチャ／FFI パニックの注入は未実施（ワーカーパニックは試験済み） |

## 5. 設計書からの差異（実装時に確定したもの）

1. **エンコーダ**: `webp` クレートではなく `libwebp-sys` 0.14.4 を直接使用（D-ENC）。同梱 libwebp が 1.6.0 になり Pillow と一致する。`webp::WebPConfig::new()` が decoder ABI を渡して失敗する不具合も回避。
2. **ピクチャのインポート**: Pillow と同じ ARGB（`use_argb = 1`）経路。RGB 直接インポートではアニメーションの後続フレームが一致しなかった。
3. **メタデータ格納**: `img-parts` ではなく自前 RIFF 書き換え。EXIF/ICCP/XMP のバイト内容は Pillow と同一だが、チャンクの並びが違うためファイル全体のバイトは一致しない（読み戻しは一致）。
4. **`Cargo.lock` の読み取り**: `cargo metadata --offline` は他プラットフォーム向け依存がキャッシュに無いと失敗するため、lock を直接パースする（D-LOCK）。
5. **CMS の sRGB プロファイル**: スレッドローカルに 1 回生成。Pillow の sRGB プロファイルとは作成日時 4 バイトが異なる（画素は一致）。
6. **16bit グレースケール**: Pillow と同じ `min(v, 255)` クリップ。
7. **GIF の背景色**: `(background_index, background_index, background_index, 255)`。
8. **UserComment**: UTF-16BE（piexif と同じ）。XPComment は UTF-16LE。
9. **`casefold`**: 主要な差分のみ写像（§Phase 0 記録 3.6）。
10. **文言**: 16bit APNG・寸法上限・SOF 精度・停滞打ち切り・パニック・起動時 WARN・マーカーの文言は実装裁量（§5 の裁量範囲）。`default_image` のみの APNG は「保存できるフレームがありません」（Pillow は `Truncated File Read`）。
11. **FFI フラグのスレッドローカル化**: libwebp 呼び出し中フラグをプロセス全体（原子変数）ではなくスレッドローカルにした。並列実行時、別ジョブの FFI 呼び出し中に起きた純 Rust パニックを「FFI パニック（プロセス終了）」と誤判定しないため（テストで発見）。
12. **UNC 出力の WAL フォールバック**: SMB 上では SQLite の WAL 共有メモリを作れないため、`journal_mode=WAL` の設定後に最初の書き込みが共有メモリ関連の I/O エラーになった場合だけ `journal_mode=DELETE` に切り替え、WARN を出して続行する（ローカルフォルダでは WAL のまま）。`StateDb::wal_enabled()` で検出する。
13. **テスト用フック**: `dispatch::Options` に `hook`/`interrupt_after`/`second_interrupt_after`/`commit_hook`/`faults`/`budget_override`/`parallel_override`/`cleanup_min_age`/`now`/`stagnation_timeout` を持つ。バイナリは既定値のまま使うため、本番に差し替え経路は無い。
14. **安全係数**: 2.0 で確定（§2 Phase 5 の「安全係数の確定」行。実測との比較で据え置きを決定）。以前ここに「2.0 の暫定のまま（Phase 5 の確定項目は未達）」と書いていたが、設計書 §6 Phase 0 が「安全係数は暫定値の決定をもって Phase 0 の決着範囲とする」と定めており、Phase 5 で実測比較のうえ据え置きを決めたため、確定として扱う。

### 5.1 エンコーダ設定の掃引（コーパス 1＋7、Rust のみ、2 回の中央値）

| 設定 | 中央値 | ピーク | 出力合計バイト |
| --- | --- | --- | --- |
| 既定（method 4 / quality 90 / 並列＝コア数） | 0.321 s | 102.8 MiB | 31,206 |
| method 0 | 0.287 s | 101.3 MiB | 52,748（+69%） |
| method 2 | 0.194 s | 101.2 MiB | 32,332（+3.6%） |
| method 6 | 0.351 s | 102.9 MiB | 31,206 |
| quality 80 | 0.327 s | 102.7 MiB | 31,142 |
| quality 100 | 0.353 s | 102.9 MiB | 30,882 |
| lossless | 0.137 s | 112.2 MiB | 10,552 |
| exact | 0.320 s | 102.9 MiB | 31,206 |
| parallel_jobs 1 | 0.675 s | 101.7 MiB | 31,206 |
| parallel_jobs 2 | 0.351 s | 101.9 MiB | 31,206 |
| parallel_jobs 4 | 0.327 s | 102.1 MiB | 31,206 |

コーパス 1 は単色に近い合成画像なので、method／quality の出力サイズ差は実際の写真コーパスより小さく出る（method 0 の +69% だけが明確な傾向）。並列度は 1→4 で約 2 倍。既定値の変更は行わない（変更する場合は §7 の改訂と利用者の再決定が必要）。

## 6. テスト実行結果

```
cargo test                        # 46 unit + 14 behaviour + 34 e2e = 94 件、失敗 0
cargo test --no-default-features  # 同じ件数（リリース相当の読み替え）
python3 doc/tools/check_spec_references.py  # 設計書の相互参照はすべて解決
```

件数は 2026-11-27 の照合時に実行して確認した（`img2webp-rust-completion-evidence.md` §3.1）。照合で追加したテスト（`webp_encode::tests::picture_is_freed_when_the_thread_unwinds`、`ffi_flag_is_thread_local`、`e2e::png_with_a_bad_chunk_crc_fails_the_job`）を含む。

## 7. 残っている未達・未検証項目

- 実際の FFI パニックの注入（FFI 中にパニックを起こす手段が無いため、分類ロジックの単体テストで代替）。
- 標準出力・標準エラーへの書き込み失敗（閉じたパイプ）の自動テスト（実装は `Reporter` で打ち切り）。
- A1111／NovelAI の実画像での読み戻し（同じ構造のフィクスチャで代替）。
- コールド I/O 状態での計測（OS のキャッシュ制御ができず未実施）。
- 安全係数は実測との比較で 2.0 を維持すると決めたが、長尺アニメーションの実コーパスでの再確認は未実施。

Python 版との出力比較（`rust/tests/compare_with_python.py`、**42 ケース**）は 40 ケースが完全等価、差分は 2 ケース。内訳は 16bit APNG（設計どおりの意図的な失敗）と `cmyk-icc.jpg`（ICC の 4 バイト差＝sRGB プロファイル作成日時で、§3.1 の合格基準内）。スクリプトは合格基準を実装していないため、基準内の差でも終了コード 1 を返す。
