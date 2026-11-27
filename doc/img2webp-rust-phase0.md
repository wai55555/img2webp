# img2webp Rust 版 Phase 0 記録

設計書 [`img2webp-rust-design-spec.md`](img2webp-rust-design-spec.md) の Phase 0（ベースライン計測・フィクスチャ整備・決定点の決着）の実施記録。Phase 1〜6 の実装は [`img2webp-rust-implementation.md`](img2webp-rust-implementation.md) に記録する。

- 日付: 2026-11-27
- 対象実装: `rust/`（パッケージ `img2webp`、バイナリ 1 本）
- 前提: 主対象 OS は Windows（§7 項目4）。POSIX のコード経路は実装するが検証対象外。

## 1. 環境と版

| 項目 | 値 |
| --- | --- |
| OS | Windows 10/11 x64（WSL2 Ubuntu 24.04 から Windows 版ツールチェーンを使用） |
| Rust | rustc 1.91.1 / cargo 1.91.1（stable-x86_64-pc-windows-msvc） |
| MSVC | Visual Studio 2022 BuildTools（MSVC 14.41） |
| Python（ベースライン計測） | 3.10.6（Windows ネイティブ） |
| Pillow（ベースライン計測） | 12.0.0（同梱 libwebp 1.6.0, zlib 1.3） |
| Pillow（挙動比較・フィクスチャ生成） | 13.0.0（WSL、同梱 libwebp 1.6.0, zlib 1.3） |
| libwebp（Rust 側） | 1.6.0（`libwebp-sys` 0.14.4 が C ソースから同梱ビルド） |
| SQLite（Rust 側） | `rusqlite` 0.37.0 + bundled |
| CMS（Rust 側） | `lcms2` 6.3.0（静的リンク、システム版は不要） |
| 画像デコード（Rust 側） | `image` 0.25.10 / `png` 0.18.1 / `gif` 0.14.2 / `zune-jpeg` 0.5.15 / `jpeg-decoder` 0.3.2 |

ベースライン計測の Python と Rust は**同じマシン・同じ OS・同じファイル**で実行した（WSL 上の Linux Python は挙動比較にのみ使用）。

## 2. 決定点の決着

| 決定点 | 決着 | 根拠 |
| --- | --- | --- |
| D-ENC | `libwebp-sys` 0.14.4 を直接 FFI で使用（`webp` クレートは不採用） | `webp` 0.3.1 は `libwebp-sys` 0.9.x に固定され、同梱 libwebp が **1.3.1**。Pillow 12/13 の同梱は **1.6.0** で、lossy 出力の画素が版差で変わる。`libwebp-sys` 0.14.4 は libwebp 1.6.0 を同梱し、ABI も `WebPConfigInitInternal` 等の必要 API を備える。追加で判明した事実: `webp::WebPConfig::new()` は `WebPConfigInitInternal` に **decoder ABI** を渡しており初期化に失敗する（`libwebp-sys` 0.9.6 の実装）。このため direct FFI を採用し、`WebPConfigInitInternal(WEBP_PRESET_DEFAULT, quality, WEBP_ENCODER_ABI_VERSION)` で初期化する。退路（libwebp-sys 直接利用）が本命になった形で、§3.3 の「退路」記述どおりの構成。 |
| D-MUX | 自前の RIFF 書き換え（`src/webp_encode.rs`）。EXIF/ICCP/XMP チャンクと VP8X フラグバイトを自前で計算 | `img-parts` は XMP トレイトを持たず VP8X の XMP ビットを立てない（§3.5 の既知制約）。チャンク順は VP8X → ICCP → 画像/ANIM → EXIF → XMP とし、格納後にフラグと実チャンクの一致を検証する。実測: Pillow が読む限り、EXIF/ICC/XMP の内容・アニメーションのコマ数・ループ・表示時間は Pillow 出力と一致する（バイト列は EXIF チャンクの配置が違うため一致しない）。 |
| D-EXIF | 自前の最小 TIFF パーサ／ライタ（`src/tiff.rs`）。未知タグは値バイト列のまま保持 | `little_exif` の現行版は WebP 書き込みの前提（VP8X と EXIF フラグ）が自前 mux と二重管理になり、往復で落ちるタグの検出限界（BUG-13）も変わらない。自前実装は IFD0/Exif/GPS/Interop/IFD1＋サムネイルを保持し、往復がバイト安定（単体テスト `round_trip_preserves_unknown_tags`）。保持範囲の確認は「生 TIFF の読み→書き→読み」の一致で行う。 |
| D-CMS | `lcms2` 6.3.0（静的リンク）。sRGB プロファイルは**スレッドローカルで 1 回だけ生成**（PERF-06） | `lcms2::Profile` は `Sync` でないためプロセス全体の `OnceLock` は不可。ワーカースレッドごとに 1 回生成して使い回す。CMYK の ICC 変換は `lcms2::PixelFormat::CMYK_8` → `RGB_8`。実測: ICC 付き CMYK JPEG の画素は Pillow（ImageCms）と完全一致し、出力 ICC は 588 バイト中 4 バイト（プロファイル作成日時フィールド）だけが異なる。 |
| D-INI | 自前パーサ（`src/config.rs`）で configparser の受理範囲を再現 | 復号は UTF-8（BOM 許容、失敗は設定エラー）、`=`/`:` 区切り、インデント行は継続行、行頭（インデント可）の `#`/`;` はコメントで継続状態に影響しない、インラインコメント無効、セクション見出しは行内最後の `]` までで`]` 以降は無視、セクション名の前後空白は保持（`[ webp]` は未知セクション）、`[DEFAULT]` はキーがあるとエラー（空なら許容）、重複キー／重複セクションはエラー、値なしキーはエラー、`""` は空値、引用符は剥がさない。整数は符号と前後の空白のみ受理（アンダースコア区切りと Unicode 数字は不許可＝§5 の意図的な変更 12）。真偽値は `1/yes/true/on/0/no/false/off`（大小区別なし）。単体テストで上記の受理範囲を固定。 |
| D-PAR | 自前の優先度付きスレッドプール（`src/dispatch.rs`）。rayon は不採用 | 第 1 段（ヘッダ読み）より第 2 段（デコード以降）を優先する必要があり、rayon にはタスク優先度が無い。単一プール内で優先度＋FIFO の `BinaryHeap` を使い、予算取得はディスパッチャ側で行う（ワーカーは予算待ちしない）。並列度は `parallel_jobs`（既定＝CPU コア数、明示値はコア数でクランプ＋WARN）。 |
| D-TIME | `filetime` で atime/mtime、Windows の作成日時は `SetFileTime`（`windows-sys`）で置換前の一時ファイルに付与 | BUG-06 の方式（置換前付与）を維持。実測: 出力ファイルの作成日時は元ファイルの作成日時と一致（e2e テスト `png_metadata_and_timestamps` で mtime/atime、Windows 作成日時は手動確認）。 |
| D-SIG | `windows-sys` の `SetConsoleCtrlHandler`（POSIX は `signal(SIGINT)`）でフラグのみ設定。確認点は初期化・計画段階、ジョブ境界、commit 間、フレーム境界、コピーの 64KiB チャンク、16bit 展開の 1MiB 単位 | 1 回目は「中断しました。記録済みのファイルは次回再利用されます。」＋最終 commit＋終了コード 130。2 回目は最終 commit なしで 130。初期化・計画段階は「中断しました。」＋130。実装は `src/interrupt.rs` と `src/dispatch.rs`。 |
| D-LIMIT | 画素数上限 178,956,970（Pillow のエラー閾値の 2 倍＝現行で失敗する画素数に合わせる）。辺上限 16,383（libwebp）。予算は利用可能メモリ×0.5 を 256MiB〜8GiB にクランプ。安全係数は暫定 2.0。ERROR 行のメッセージ上限 4,096 バイト（文字境界で切り詰め、末尾に `…`）。警告は最大 8 件（IFD 5 種＋サムネイル＋ICC＋XMP）。待ち行列の停滞打ち切りは 600 秒。一時ファイル掃除の最低経過時間は 3,600 秒、省略時の倍率は 10。 | 画素数: 現行 Pillow は約 8,950 万画素で警告のみ、その 2 倍で `DecompressionBombError`。上限をそのエラー閾値に合わせ、現行で成功する画像を失敗させない。辺 16,383 は libwebp の制限（BUG-07）。安全係数は Phase 5 の実測で確定する暫定値（§3.4 の指示どおり Phase 0 では暫定決着）。 |
| D-MEM | `sysinfo::System::available_memory()`（起動時に 1 回だけ取得） | 予算は起動時固定。テストでは `Options::budget_override` で内部指定する（本番バイナリに差し替え経路は無い）。 |
| D-LOCK | `build.rs` が **`Cargo.lock` を直接パース**して対象クレートの版を埋め込む（`cargo metadata` は使わない） | `cargo metadata --offline` は、lock に含まれる他プラットフォーム向けパッケージ（例: `objc2-core-foundation`）がローカルキャッシュに無いと失敗する。lock 直接パースはオフラインで決定的。対象クレートは image/png/gif/zune-jpeg/zune-core/image-webp/jpeg-decoder/libwebp-sys/rusqlite/lcms2/lcms2-sys/flate2/miniz_oxide。libwebp 本体の版は実行時に `WebPGetEncoderVersion()` で取得（動的リンクではないため実行時版＝ビルド時版）。`Cargo.lock` はリポジトリにコミットし、リリースビルドは `--locked` を必須とする。 |

上記の D-ENC の検証項目のうち、**`WebPConfig` の全フィールド対応表**は照合時に欠落が判明したため [`img2webp-rust-webpconfig-mapping.md`](img2webp-rust-webpconfig-mapping.md) に作成した（libwebp 1.6.0 の 29 フィールドについて、libwebp 既定値・Pillow 静止画・Pillow アニメ・img2webp 静止画・img2webp アニメを対比）。**FFI パニック時の Drop 挙動**は `src/webp_encode.rs` の単体テスト 2 件（`picture_is_freed_when_the_thread_unwinds`、`ffi_flag_is_thread_local`）で固定した。

## 3. §3.1〜§3.8 から委譲された検証項目の結論

### 3.1 画素等価性の合格基準（確定）

- 同一 libwebp 版（1.6.0）・同一設定では、**静止画・アニメーションの双方で画素の完全一致を要求する**。
- 実測（`rust/tests/compare_with_python.py`、**42 ケース**）: 画素差 0（合格基準内）。内訳は PNG（8bit RGBA/RGB/パレット、16bit RGB/RGBA/LA/グレー、Stealth 8/16bit、gzip/zlib/info、フィルタ 0〜4）、GIF（全面・部分フレーム・disposal 1/2/3・背景色あり）、APNG（default_image あり/なし）、JPEG（RGB/グレー/CMYK/CMYK+ICC/Orientation）、コピー（JPEG/WebP/WebM）。lossless／lossy、method 0/4、exact、metadata 有無の各変種を含む。
- CMS 経路（CMYK/L/LA＋ICC）だけは ICC プロファイルのバイト列が一致しない（sRGB プロファイルの作成日時 4 バイト）。**合格基準は「画素完全一致＋出力 ICC が sRGB として解釈できること」**とし、ICC バイトの一致は求めない。
- ファイルサイズは合格基準に含めない（実測差は全体で +0.11%）。
- 16bit APNG は §3.4 の決定どおり未対応（ジョブ単位の失敗）で、比較対象から除外する。

### 3.2 BUG-06（Windows 作成日時）

置換前の一時ファイルに `SetFileTime` で作成日時を付与し、`MoveFileEx`（同一ボリューム内のリネーム）で置換する方式で保持されることを実機で確認した（出力ファイルの作成日時＝元ファイルの作成日時）。方式変更は不要。

### 3.3 BUG-10（16bit PNG の 8bit 化方式）

Pillow 実測結果:

| 入力 | Pillow のモード | 8bit 化 |
| --- | --- | --- |
| 16bit RGB / RGBA / LA | RGB / RGBA / RGBA | 各サンプルの**上位バイト**（`>> 8`） |
| 16bit グレースケール（色タイプ 0） | `I;16` | `convert("RGB")` で **255 にクリップ**（`min(v, 255)`） |

Rust 側はこの 2 方式を実装し、比較で画素差 0 を確認した（`plain16-gray0/2/4/6.png`）。グレースケールのクリップは直感に反するが現行挙動であり、互換を優先して維持する。

### 3.4 BUG-11（loop／duration の意味論）

- 表示時間: Pillow は GIF の GCE 遅延（1/100 秒）を ×10、APNG の fcTL を `delay_num / delay_den × 1000`（den=0 は 100）で求める。`round()` は銀行丸め。Rust 側も同じ計算・同じ丸め（`animation::python_round`）で、比較で全フレーム一致（GIF 70/130/100/200/300、APNG 70/130/50）。
- ループ: GIF は NETSCAPE 拡張の値をそのまま、無ければ 1。APNG は acTL の num_plays、無ければ 0。WebP の ANIM チャンクに同じ値を書き、比較で一致。
- AnimationEncoder の時刻は Pillow と同じく**累積開始時刻**を渡し、最後に NULL フレームで総時間を渡して flush する。

### 3.5 GIF 合成と APNG の読み取り経路

- `image` 0.25 の `GifDecoder` は disposal 合成済みのフレームを返し、**Pillow と画素完全一致**（`optimized.gif`、`background-disposal.gif`、`anim.gif`）。disposal=Background の消去色も一致したため、自前合成は不要（BUG-01 は「差異なし」で決着）。
- APNG: `PngDecoder::apng()` の明示呼び出しで、`default_image` が分離された APNG では既定画像を除外したフレーム列が得られる（Pillow の `start_frame=1` と同じ）。`default_image` のみの APNG（acTL はあるが fcTL が無い）は 0 フレームとなるため「保存できるフレームがありません」でジョブ失敗にする（Pillow も `Truncated File Read` で失敗する。文言は異なる）。
- 16bit APNG: 第 1 段で IHDR のビット深度 16 と acTL の存在を確認した時点で「16bit APNG は未対応です」としてジョブ失敗（デコード・Stealth 走査より前）。

### 3.6 その他の実測結果

- **UserComment の符号化**: piexif 1.1.3 の `UserComment.dump(..., "unicode")` は **UTF-16BE**（EXIF 仕様）。XPComment は UTF-16LE＋NUL 終端。Rust 側も同じ（当初 LE で実装し比較で発見・修正）。
- **Pillow の ARGB インポート**: Pillow の `_webp.c` は `WebPPicture.use_argb = 1` でピクチャを作る（RGB→YUV 変換をライブラリに任せない）。Rust 側も同じ経路に合わせたところ、アニメーションの ANMF がバイト一致した（RGB 直接インポートでは後続フレームの lossy 出力がわずかにずれた）。
- **GIF の背景色（bgcolor）**: Pillow は `info["background"]`（グローバルカラーテーブルのインデックス）を RGBA に変換するが、本ツールのパイプラインではフレームが RGB/RGBA に変換済みでパレットが失われているため、実効値は `(index, index, index, 255)`。Rust 側も同じ値を使う（実測で ANIM の bgcolor が一致）。
- **PNG メタデータの取得範囲**: 実測（Pillow 13）では、テキストチャンクは IDAT の前後どちらでも `info` に入る（`tail-text.png` で確認）。eXIf / iCCP も同様に読めた。Rust 側はチャンク走査で全文を対象にするため、Pillow より取得範囲が広い（後方配置も取得）。これは「保持が増える」方向の差で、比較でも一致した。
- **PNG の CRC 不一致**: `image`/`png` クレートはチャンク CRC を検証するが、**Pillow は検証しない**ため、そのままでは Python 版が変換できるファイルを Rust 版が落とす（判明: 2026-11-27）。**決定（同日）: Pillow に合わせる**。復号に失敗したときだけ CRC を計算し直したバイト列で再試行し、`WARN ... PNG のチャンク CRC が不正でした（IDAT）。Pillow と同じく無視して変換します` を出す（`src/convert.rs::repair_png_crcs`）。実測: `rust/tests/fixtures/crc-mismatch.png` で Python 版・Rust 版とも終了コード 0、**出力バイト完全一致**。e2e テスト `png_with_a_bad_chunk_crc_is_converted_with_a_warning` が固定する。Stealth 用の生デコード（`png_raw.rs`）は現行どおり CRC を検証しない。
- **MPO / SOF 精度**: MPO は APP2 の `MPF\0` で検出し、変換対象なら「拡張子は JPEG ですが、実際の形式は MPO です」でジョブ失敗（コピー設定ならコピー）。SOF 精度が 8 でない JPEG は第 1 段で「JPEG のサンプル精度 12 は未対応です」でジョブ失敗。Pillow はどちらもデコードに失敗する（`OSError`）。
- **ctime の取得可否**: Windows は `Metadata::created()`、POSIX は `ctime`/`ctime_nsec` で取得可能。取得できない環境では再利用を無効化し、起動時に WARN を出す。
- **ICC の色空間不一致**: `lcms2::Profile::color_space()` の署名（`CMYK`/`RGB`/`GRAY`）で判定。Pillow の `profile.xcolor_space.strip()` と同じ判定になる。エラー文言も現行どおり 2 種を使い分ける。
- **`casefold` 差分（SPEC-02）**: Python の `str.casefold` と Rust の `to_lowercase` の差は、`ß→ss`、`ς→σ`、`ﬁ` などの合字、`ŉ`、`ǰ`、`ΐ`、`ΰ`、`ẖ/ẗ/ẘ/ẙ/ẚ`、`և`、アルメニア合字、`U+0345`。`src/paths.rs::casefold` でこれらを個別に写像する。未対応は Unicode の `F`（full folding）系の一部（ギリシャ文字の `ᾼ` 等の大文字＋ypogegrammeni 展開）で、ファイル名として現れる頻度は無視できる。差が残る文字をファイル名に使った場合のみ衝突判定が変わりうる。
- **UNC パス**: Windows の verbatim プレフィックス（`\\?\`、`\\?\UNC\`）を剥がして `C:\...` / `\\server\share\...` に戻す実装を入れた。実測は管理共有 `\\localhost\C$` を使い、入力・出力の両方を UNC にした設定で確認した（`tests/e2e.rs::unc_paths_are_supported_for_input_and_output`、開発ビルドの派生先 `outputs-dev` も UNC 上で作成される）。**判明した差異**: SQLite の WAL は SMB 上で共有メモリファイル（`-shm`）を作れず `disk I/O error` になる。Python 版（Windows の Python 3.10 同梱 SQLite）は同じ設定で成功したため、Rust 版は WAL 作成に失敗したときだけロールバックジャーナル（`journal_mode=DELETE`）へフォールバックし、WARN を出して続行する（ローカルフォルダでは従来どおり WAL）。このフォールバックは意図的な追加（§5 の意図的な変更に対する追加事項）として記録する。
- **ERROR 行の写像表**: `ConversionError`（形式不一致・表示時間・ループ・フレーム無し・処理中の変更・寸法上限・16bit APNG）、`UnidentifiedImageError`（形式不明）、`OSError`（デコード失敗・I/O）、`PermissionError`/`FileNotFoundError`（I/O 種別）、`DecompressionBombError`（画素数上限）、`MemoryError`（予算超過）、`PanicError`（ワーカーの純 Rust パニック。現行に対応する型が無いための専用対応）。ライブラリ由来の文面は `image` のエラー文字列をそのまま含める（Pillow の文面とは異なる）。
- **JPEG／GIF コメントの取得経路（2026-11-27 追記）**: JPEG は APP セグメントの COM（`0xFE`）を `probe.rs` のセグメント走査で読み、GIF はコメント拡張ブロック（`0xFE`）をブロック走査で読む。どちらも最初の 1 件を `Probe::comment` に保持し、生成テキストの差し込みに使う（`metadata::source_text`）。実測: `probe.rs::gif_frames_loop_and_comment` が `anim.gif` の `gif prompt` を検証し、`compare_with_python.py` の `anim.gif`・`once.gif` で出力の UserComment が Python 版と一致する。JPEG 側はフィクスチャに COM を持つものが無いため、経路の確認はコード読解と `probe.rs` のセグメント走査に留まる（**限界**）。

## 4. テストの white-box / black-box 振り分け

| 現行テスト（Python） | Rust 側の扱い |
| --- | --- |
| 設定検証（`test_config_validation`） | black-box（`tests/e2e.rs::config_validation_errors_exit_two`）＋ white-box（`src/config.rs` の単体テストで受理範囲を固定） |
| 衝突命名（`test_collision_names...`） | white-box（`src/plan.rs` 単体テスト）。black-box でも同じ入力で確認 |
| 再開・再利用・品質変更・adopt | black-box（`tests/e2e.rs::resume_*`, `quality_*`, `untracked_output_requires_adoption`） |
| 中断時の commit | white-box（`tests/behaviour.rs` の `Options::interrupt_after`） |
| メタデータ（UserComment/XPComment/ICC/XMP） | black-box（`tests/e2e.rs`）＋ white-box（`src/metadata.rs`, `src/tiff.rs`） |
| 向き・CMYK | black-box（`tests/e2e.rs::jpeg_orientation_and_cmyk`） |
| コピー・日時 | black-box（`tests/e2e.rs::copies_*`） |
| Stealth 全バリエーション | white-box（`src/stealth.rs` の単体テストで 8/16bit × RGB/RGBA/LA × info/gzip/zlib × フィルタ 0〜4）＋ black-box（代表 5 種） |
| `png_16bit_pixels` の制限・壊れた入力 | white-box（`src/stealth.rs` の単体テスト） |
| Windows 作成日時の API 引数検証 | white-box 相当を e2e（実ファイルの日時）で置き換え |
| `initialize_state` の mock（white-box） | Rust では DB の単体テスト（`src/state.rs`）と Python 製 DB の互換テスト（`tests/behaviour.rs`）に再構成 |

black-box の期待値の読み替え規則: 開発ビルドは出力先が `outputs-dev`、状態 DB が `.img2webp-dev.sqlite3`、標準エラーに起動行とマーカーファイル（`.img2webp-dev-output`）が加わる。テストは `cfg!(feature = "dev-db")` で読み替える（`tests/e2e.rs`）。

## 5. ベースライン計測（§4.2）

コーパスは `rust/tests/benchmark.py` が生成する 111 ファイル（カテゴリ 8 種）。同一マシン・同一入力で Python 版と Rust 版を各 3 回実行し、中央値を採った。出力フォルダと状態 DB は毎回初期化している（再利用の計測にならないようにする）。コールド／ウォームの区別は OS のファイルキャッシュ制御ができないため未実施（**限界として記録**）。ピークメモリはプロセスのピークワーキングセット。

| カテゴリ | Python 中央値 | Rust 中央値 | 倍率 | Python ピーク | Rust ピーク |
| --- | --- | --- | --- | --- | --- |
| 1. AI 生成 PNG 40 件 | 0.413 s | 0.075 s | 5.48× | 26.7 MiB | 26.2 MiB |
| 2. Stealth PNG 20 件（8/16bit） | 0.279 s | 0.085 s | 3.30× | 27.4 MiB | 25.3 MiB |
| 3. 16bit PNG 10 件 | 0.319 s | 0.075 s | 4.23× | 26.9 MiB | 19.0 MiB |
| 4. JPEG 10 件（変換） | 0.153 s | 0.063 s | 2.42× | 22.7 MiB | 9.3 MiB |
| 5. GIF 5 件（10 フレーム） | 0.472 s | 0.063 s | 7.45× | 26.6 MiB | 14.7 MiB |
| 6. APNG 5 件（10 フレーム） | 0.237 s | 0.064 s | 3.71× | 26.6 MiB | 15.1 MiB |
| 7. 4K 静止画 1 件 | 0.594 s | 0.301 s | 1.97× | 132.2 MiB | 102.1 MiB |
| 8. コピー主体 20 件 | 0.164 s | 0.065 s | 2.54× | 22.8 MiB | 9.3 MiB |
| 全体（111 件） | 1.051 s | 0.351 s | 2.99× | 135.3 MiB | 106.4 MiB |

- 出力合計バイト: Python 217,954 / Rust 218,194（+0.11%）。画素は全件一致。
- 判定基準（§4.2 の原案）に対する評価: 静止画バッチの目標「現行比 2 倍以上」を満たす（2.99×）。「単一ファイルで 5% 超の劣化カテゴリを作らない」も満たす（最遅カテゴリでも 1.97×）。§4.2 の判定基準はこの原案どおりで**確定**（緩和なし）。
- **Stealth 負ケースの追加コスト（2026-11-27 再計測）**: 本節の当初の評価（カテゴリ 2 とカテゴリ 1 の 1 件あたり時間の差）は別々のコーパスを比べており、負ケースの追加コストの判定には使えない（カテゴリ 2 は 4.23 ms/件、カテゴリ 1 は 1.89 ms/件で、差を取ると +124% になる）。同一ファイルで Stealth 走査 on/off を切り替える統制比較を `rust/examples/profile_pipeline.rs` で追加し、次の結果を得た: 8bit 負ケース（AI PNG 40 件）0.09%（基準内）、16bit 負ケース（256×256 10 件）**19.0%**、1024×1024 3 件 **20.6%**。16bit 負ケースは §4.2 の「変換本体の 10% 以内」を**満たさない**。→ **2026-11-27 に是正した**（16bit 走査をシグネチャ 120 ビット分の行だけで判定する 2 段方式に変更）。是正後の再計測は 8bit 0.10%／16bit 256×256 5.3%／16bit 1024×1024 3.7% で基準内。加えて `[preserve] stealth_max_pixels`（既定 16,777,216 画素）と `stealth` を追加し、巨大な 16bit は走査を省略できる（`img2webp-rust-completion-evidence.md` の「11. Stealth 走査の是正」）。
- 計測スクリプトと生データ: `rust/tests/benchmark.py`、`.bench/results.json`（リポジトリには含めない。再実行で再生成できる）。
- 限界: 3 回の中央値であり、5% 差の判定までは分解していない（§4.2 の要求どおり、Phase 5 の本計測では必要なら回数を増やす）。I/O キャッシュのコールド制御は未実施。

## 6. エンコーダ比較（§3.3 案D `zenwebp`）と §7 項目5

- `zenwebp` は AGPL-3.0 または商用ライセンスで、本プロジェクト（MIT）とは両立しない。§3.3 案D の採用条件 (4)「ライセンス問題の解消」が成立しないため、ベンチ条件 (1)〜(3) を満たしても採用できない。
- したがって §7 項目5 は「**採用しない**」で決着する（商用ライセンス取得も本プロジェクトのライセンス変更も行わない）。ベンチ計測は実施していない（条件 (4) の不成立により判断不要）。この結論は §3.3 案D の「ベンチが条件 (1)〜(3) を満たさなかった場合は判断不要として記録する」に準じ、満たす前にライセンスで不可とした形。Phase 1 以降の実装は libwebp のみを使う。

## 7. Phase 0 完了条件チェック

| 条件 | 状態 | 根拠 |
| --- | --- | --- |
| 計測結果が記録されている | 完了 | 本ドキュメント §5、`.bench/results.json` |
| 決定点がすべて決着（安全係数は暫定） | 完了 | §2 の表（D-ENC/D-MUX/D-EXIF/D-CMS/D-INI/D-PAR/D-TIME/D-SIG/D-LIMIT/D-MEM/D-LOCK） |
| §4.2 の判定基準が確定 | 完了 | 原案どおり（緩和なし）。§5 |
| white-box / black-box の振り分け表を固定 | 完了 | §4 |
| §7 項目1〜4・6 は決定済み、項目5 は採用しないで決着 | 完了 | §6、実装（`config.rs`, `dispatch.rs`） |
| 画素等価性の許容基準 | 完了 | §3.1 |
| BUG-06 の保持可否 | 完了 | §3.2 |
| BUG-10 の 8bit 化方式 | 完了 | §3.3 |
| BUG-11 のフィクスチャ化 | 完了 | §3.4、`optimized.gif` / `apng-standard.png` / `apng-separate.png` |
| pipeline: 3 の記録とコピー記録の双方向互換 | 完了 | `tests/behaviour.rs::python_database_copy_records_are_reused_and_conversions_rebuilt`（Python 製 DB を複製して再利用と一度きりの再変換を確認）。Rust が書いたコピー記録を Python が読む方向は、署名文字列がバイト一致（`src/state.rs` のゴールデンテスト）であることと、Python 製 DB 側の署名も同一文字列であることの両方向で確認 |
| ctime の取得可否 | 完了 | §3.6 |
| GIF 合成と APNG の読み取り経路 | 完了 | §3.5 |
| casefold 差分の列挙（SPEC-02） | 完了 | §3.6 |
| ERROR 行の写像表の固定 | 完了 | §3.6 |
| JPEG／GIF コメントの取得経路 | 完了 | JPEG は COM セグメント、GIF はコメント拡張ブロック（`probe.rs`）。比較で `gif prompt` が一致 |
| MPO と SOF 精度が 8 でない JPEG の成否 | 完了 | §3.6、e2e テスト |
| PNG メタデータの取得範囲 | 完了 | §3.6 |
| PNG の CRC 不一致の扱い | 完了 | §3.6 |
| UNC パスの正規形 | 完了 | §3.6。管理共有 `\\localhost\C$` で入力・出力・開発ビルド派生先を実測（`unc_paths_are_supported_for_input_and_output`）。WAL 不可時のフォールバックも実装 |
| 超過時の扱い（ジョブ単位の失敗） | 完了 | e2e テスト `dimension_and_pixel_limits_fail_the_job`、`tests/behaviour.rs::budget_estimate_larger_than_total_fails_the_job` |

Phase 0 の検証項目はすべて結論済み。UNC は管理共有で実測し、WAL が使えない場合のフォールバックを追加した（意図的な追加事項）。
