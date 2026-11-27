# img2webp Rust 版 完了申告の照合証跡

設計書 [`img2webp-rust-design-spec.md`](img2webp-rust-design-spec.md)（確定版）の完了条件に対する、申告内容の照合結果。**push は行っていない**（本ファイルは未追跡のまま）。

- 照合日: 2026-11-27
- 対象: `dev` = `c964601`（`rust-v0.1.0` の対象）、`python-version-final` = `ee1805d`
- 照合環境: WSL2 Ubuntu 24.04 から Windows 側ツールチェーン（cargo/rustc 1.91.1, MSVC）を使用。実行したバイナリは `rust/target/release/img2webp.exe`（2026-11-27 18:43:13 ビルド）
- 本ファイルは**証跡の提示のみ**を目的とし、コード・設計書・既存ドキュメントは一切変更していない。作業用に作成したのは (1) 本ファイル、(2) Windows の一時フォルダ `%TEMP%\img2webp-smoke-20261127-195049`、(3) `/tmp` 配下のログのみ。

---

## 0. 所在の確認

**ご理解のとおりで正しい。** このリポジトリ内にローカル実在する。

```
$ git rev-parse c964601^{commit}
c9646017a6ad25a9176bbb54dd86ad7d2a3c3c4b
$ git cat-file -t c964601
commit
$ git rev-parse rust-v0.1.0^{}
c9646017a6ad25a9176bbb54dd86ad7d2a3c3c4b
$ git rev-parse python-version-final^{}
ee1805dff743a65a8a8d58c2a7c22c4f468005d3
$ git cat-file -p rust-v0.1.0
object c9646017a6ad25a9176bbb54dd86ad7d2a3c3c4b
type commit
tag rust-v0.1.0
...
Rust port release
$ git cat-file -p python-version-final
object ee1805dff743a65a8a8d58c2a7c22c4f468005d3
type commit
tag python-version-final
...
Final state of the Python implementation before the Rust port
```

`git status --short` は空（本ファイル追加前）。`origin/dev` は `ee1805d` のままで、`dev` は `[origin/dev: ahead 1]`。

---

## 1. Phase 0

### 1.1 決定点 11 件の結論と根拠（`doc/img2webp-rust-phase0.md` §2）

`python3 doc/tools/check_spec_references.py` の出力で決定点は 11 件（D-CMS, D-ENC, D-EXIF, D-INI, D-LIMIT, D-LOCK, D-MEM, D-MUX, D-PAR, D-SIG, D-TIME）、`all cross references resolve`。全 11 件に「決着」「根拠」が記入されている。

| 決定点 | 決着（要旨） | 根拠の種類 | 根拠の実体 |
| --- | --- | --- | --- |
| D-ENC | `libwebp-sys` 0.14.4 を直接 FFI（`webp` クレート不採用） | 版・ABI の一次確認＋画素比較 | `webp` 0.3.1 が libwebp 1.3.1 固定 / `webp::WebPConfig::new()` が decoder ABI を渡して失敗。実装 `src/webp_encode.rs:62-82`。`compare_with_python.py` で画素一致 |
| D-MUX | 自前 RIFF 書き換え（VP8X フラグ・EXIF/ICCP/XMP を自前計算） | 実測（Pillow 読み戻し） | `src/webp_encode.rs`、`compare_with_python.py` の exif/icc/xmp 比較（icc のみ後述の差） |
| D-EXIF | 自前の最小 TIFF パーサ／ライタ | 単体テスト | `src/tiff.rs::round_trip_preserves_unknown_tags` |
| D-CMS | `lcms2` 6.3.0 静的リンク、sRGB はスレッドローカル 1 回生成 | 実測（画素一致） | CMYK+ICC の画素一致（`compare_with_python.py` の `cmyk-icc.jpg`） |
| D-INI | 自前パーサで configparser の受理範囲を再現 | 単体テスト | `src/config.rs` の単体テスト群（受理範囲の固定） |
| D-PAR | 自前の優先度付きスレッドプール（rayon 不採用） | 設計理由＋テスト | `src/dispatch.rs`、`behaviour` の予算待ちテスト |
| D-TIME | `filetime` ＋ Windows 作成日時は `SetFileTime`（置換前の一時ファイルへ） | 実測 | e2e `png_metadata_and_timestamps`（atime/mtime）。作成日時は手動確認 |
| D-SIG | `SetConsoleCtrlHandler`／POSIX `signal` でフラグのみ、確認点 6 箇所 | 実測（終了コード 130） | `src/interrupt.rs`、`src/dispatch.rs`、`behaviour` の中断テスト |
| D-LIMIT | 画素数上限 178,956,970、辺 16,383、予算 0.5×を 256MiB〜8GiB、安全係数 2.0（暫定）、ERROR 4,096 バイト、警告 8 件、停滞 600 秒、掃除 3,600 秒／10 倍 | 実測＋Pillow 閾値の突き合わせ | e2e `dimension_and_pixel_limits_fail_the_job`、`behaviour::budget_estimate_larger_than_total_fails_the_job` |
| D-MEM | `sysinfo::available_memory()` を起動時 1 回 | 設計 | `src/budget.rs` |
| D-LOCK | `build.rs` が `Cargo.lock` を直接パース | 実測（`cargo metadata` のオフライン失敗） | `rust/build.rs`、`src/state.rs::convert_signature_contains_identity_and_settings` |

### 1.2 D-ENC 内包分（設計書 §6 Phase 0 が明示列挙している 4 項目）

| 項目 | 証跡の有無 | 内容 |
| --- | --- | --- |
| Pillow 側 bgcolor 実効値の確認 | **あり** | `phase0.md` §3.6 に実効値 `(index, index, index, 255)` と ANIM bgcolor 一致の実測記録。`compare_with_python.py` の GIF 系（`anim.gif` ほか）が一致 |
| `WebPConfig` の全フィールド対応表 | **なし（欠落）** | `doc/*.md` 全体で `WebPConfig` の記述は設計書 8 箇所・phase0 1 箇所（D-ENC 行）・implementation 1 箇所のみで、**フィールド対応表は存在しない**。コードは `quality/lossless/method/exact/alpha_quality/thread_level` の 6 フィールドのみ設定（`src/webp_encode.rs:77-82`）し、残りは `WebPConfigInitInternal` の既定値。対応表に相当する文書もコメントも無い |
| FFI パニック時の Drop 挙動の確認 | **なし（欠落）** | `impl Drop for Picture`（`src/webp_encode.rs:148-152`、`WebPPictureFree`）はあるが、「巻き戻し中の Drop 挙動を Phase 0 で確認した」記録は docs にもコードコメントにも無い。Phase 5 の記録（`FFI_ACTIVE` のスレッドローカル化、`classify_worker_panic`）は**パニックの分類**の話で、Drop 挙動の確認ではない |
| アニメーションの lossless／method のフレーム側 config への反映の試作確認 | **間接のみ** | Phase 0 の試作記録は無い。Phase 4 の実装（`src/webp_encode.rs::encode_animation`）と `compare_with_python.py` の lossless 変種で等価を確認、という形で後追いになっている |

設計書 §6 前文には「決定点に内包される検証事項は、決定点の決着をもって結論済みとし、個別の結論項目としては列挙しない」とあるため**手続き上は許容**される。ただし実体として (2)(3) は成果物が存在せず、(4) は Phase 0 ではなく Phase 4 で代替されている。

### 1.3 設計書 §6 Phase 0 が列挙する結論項目（15 件）の全件チェック

設計書の行を機械的に分解して得た要求項目と、`phase0.md` §7 のチェック表の対応:

| # | 要求項目 | 結論 | 根拠の実体 | 判定 |
| --- | --- | --- | --- | --- |
| 1 | §3.1 画素等価性の許容基準 | あり | §3.1（CMS 経路のみ ICC 4 バイト差を許容） | OK |
| 2 | BUG-06 の保持可否 | あり | §3.2（置換前 `SetFileTime` で保持） | OK（実機は手動確認） |
| 3 | BUG-10 の 8bit 化方式 | あり | §3.3（上位バイト / グレーは `min(v,255)`） | OK（`plain16-gray0..6` で画素一致） |
| 4 | BUG-11 のフィクスチャ化 | あり | §3.4、`optimized.gif` / `apng-standard.png` / `apng-separate.png` が実在し比較で一致 | OK |
| 5 | pipeline: 3 の記録とコピー記録の双方向互換 | あり | `behaviour::python_database_copy_records_are_reused_and_conversions_rebuilt`（実行して通過） | OK |
| 6 | ctime の取得可否 | あり | §3.6（Windows は `Metadata::created()`、POSIX は ctime） | OK |
| 7 | GIF 合成と APNG の読み取り経路（default_image のみを含む） | あり | §3.5、e2e `apng_default_image_is_skipped` / `default_image_only_apng_fails`（通過） | OK |
| 8 | casefold 差分の列挙（SPEC-02） | あり | §3.6（差分文字の列挙、`src/paths.rs::casefold`） | OK（F 系の一部未対応を明記） |
| 9 | ERROR 行の写像表（型名＋ライブラリ由来文面＋パニック型の専用対応） | あり | §3.6 末尾（`ConversionError`/`UnidentifiedImageError`/`OSError`/`PermissionError`/`FileNotFoundError`/`DecompressionBombError`/`MemoryError`/`PanicError`） | OK |
| 10 | JPEG／GIF コメントの取得経路 | あり（記述のみ） | §7 の 1 行（「JPEG は COM セグメント、GIF はコメント拡張ブロック（`probe.rs`）」）。§3.x に独立した結論節は無く、`compare_with_python.py` の `gif prompt` 一致が唯一の実測 | 記述はあるが根拠は薄い |
| 11 | MPO と SOF 精度が 8 でない JPEG の成否 | あり | §3.6、e2e `mpo_and_non_eight_bit_jpeg_are_rejected_when_converted`（通過） | OK |
| 12 | PNG メタデータの取得範囲 | あり | §3.6（`tail-text.png` で IDAT 後方も取得、Pillow より広い） | OK |
| 13 | PNG の CRC 不一致の扱い | あり（記述のみ） | §3.6（`image`/`png` は CRC 検証、`png_raw.rs` は非検証、Pillow も失敗） | **フィクスチャ・テスト無し**。`broken.png` は「not a PNG at all」で CRC 不一致ではない（`generate_fixtures.py:330`） |
| 14 | UNC パスの正規形 | あり | §3.6、e2e `unc_paths_are_supported_for_input_and_output`（通過） | OK（実行されたことを別途確認。下記 1.4） |
| 15 | 超過時の扱い | あり | e2e `dimension_and_pixel_limits_fail_the_job`、`behaviour::budget_estimate_larger_than_total_fails_the_job`（通過） | OK |

**結論: 15 件すべてに結論の記述はある。** ただし #13 は結論の裏付けとなるフィクスチャ／テストが無く、#10 は記述が 1 行のみ。#1〜#15 を「全件結論済み」と言うこと自体は文書上成立する。

### 1.4 UNC テストが実際に走ったことの確認

`unc_paths_are_supported_for_input_and_output` は管理共有に到達できないと**無言で return（成功扱い）**する構造。今回は Windows 側で共有が生きていることを確認した:

```
$ powershell.exe -NoProfile -Command "Test-Path '\\localhost\C$'; ..."
True
ADMIN$,C$,D$,E$,IPC$
```

したがってスキップ分岐は踏んでおらず、実 UNC パスで通過している。ただしこの「無言スキップ」構造自体は、共有が無い環境での緑を実力を伴わない緑にする（証跡としての弱さ）。

---

## 2. 開発ビルド隔離

### 2.1 リリースビルドが `--no-default-features` であること

`rust/Cargo.toml`:

```toml
[features]
default = ["dev-db"]
dev-db = []

[profile.release]
panic = "unwind"
```

配布 exe と同一時刻の cargo フィンガープリントが、**有効 feature が空**であることを記録している:

```
$ stat -c '%y %n' rust/target/release/img2webp.exe
2026-11-27 18:43:13.041286000 +0900 rust/target/release/img2webp.exe
$ stat -c '%y %n' rust/target/release/.fingerprint/img2webp-9087335e08185a74/bin-img2webp.json
2026-11-27 18:43:13.086285300 +0900 .../bin-img2webp.json
$ python3 -c "import json;d=json.load(open('.../bin-img2webp.json'));print(d['features'], d['declared_features'])"
[] ['default', 'dev-db']
```

開発ビルド側は `["default", "dev-db"]`（`rust/target/debug/.fingerprint/img2webp-25857d04f3a1a2e1/bin-img2webp.json`）。

**注意（誤読しやすい点）**: 配布 exe にも `img2webp-dev`（2 回）と `.img2webp-dev-output`（1 回）の文字列が入っている。これは `src/state.rs:16-18` の定数を `src/plan.rs` が**衝突回避の予約名**として feature 非依存で使っているため。`strings` による dev 混入判定は誤検知になる。

### 2.2 配布 exe 個体のスモークテスト（設計書 §6 Phase 6 の要求どおり、ビルド記録ではなく個体で検証）

実施: 空の一時フォルダに exe と `config.ini`（リポジトリ同梱内容）だけを置いて実行。

```
=== RUN in C:\Users\y\AppData\Local\Temp\img2webp-smoke-20261127-195049 ===
STDOUT| 完亁E 変換 0 / コピ�E 0 / 再利用 0 / 登録 0 / 失敁E0
EXITCODE=0  ELAPSED=0.24s
=== FILES AFTER RUN ===
inputs / outputs / config.ini (321B) / img2webp.exe (4,290,048B)
=== outputs contents ===
.img2webp.sqlite3
```

（`完亁E` 等の文字化けは、UTF-8 出力を PowerShell が CP932 で復号した表示上の問題。Python 版・Rust 版とも UTF-8 出力で、設計書 §5 の意図的な変更 (11) のとおり。）

判定:

- 生成 DB 名 = `.img2webp.sqlite3`（実名）→ dev-db 無効
- 出力先 = `outputs`（`outputs-dev` は作られない）、`.img2webp-dev-output` マーカー無し
- 開発ビルドの起動行（標準エラー）は**出ていない**（標準出力・標準エラーを併合して捕捉した全出力が上記 1 行のみ）
- 終了コード 0、`convert/copy/skip/adopt/fail` すべて 0 件の空実行

### 2.3 開発ビルドが実 DB・実出力先に触れていないこと

| 確認方法 | 結果 |
| --- | --- |
| e2e ハーネスの作業場所 | `std::env::temp_dir()`（`rust/tests/e2e.rs:42`）を使い、リポジトリの `outputs/` を使わない |
| リポジトリの実 DB の mtime | `outputs/.img2webp.sqlite3` = **2026-11-21 17:32**（Rust 化作業の 11-27 より前、今回の `cargo test` 2 回でも不変） |
| リポジトリ直下の dev DB | `.img2webp-dev.sqlite3` は存在しない（テストは一時フォルダ側で作る） |
| 構造的分離のテスト | `e2e::dev_and_release_build_paths_are_separated`（通過）。dev 時は `outputs-dev`・マーカー・起動行、release 時は `outputs/.img2webp.sqlite3` を assert |
| `git status` | クリーン（本ファイル追加前） |

### 2.4 `--locked` の証跡

`rust/Cargo.lock` は `c964601` で追跡され、`git diff HEAD -- rust/Cargo.lock` は空（作業ツリーで変更なし）。**ビルドログ等の直接証跡は残っていない**（`--locked` で失敗した記録も無い）。間接証跡のみ。

### 2.5 単一 exe

`objdump -p rust/target/release/img2webp.exe` の import DLL は次のみで、`VCRUNTIME140.dll` への依存は無い（文字列カウント 0）:

```
KERNEL32.dll, advapi32.dll, api-ms-win-core-synch-l1-2-0.dll, bcryptprimitives.dll,
kernel32.dll, ntdll.dll, oleaut32.dll, pdh.dll, powrprof.dll, psapi.dll, shell32.dll
```

lcms2 / libwebp / SQLite は静的リンク（`.cargo/config.toml` の `+crt-static`）。

---

## 3. Phase 1〜5 のテスト結果と §4.3 本計測

### 3.1 テスト実行結果（今回の実測）

```
$ cargo test
     Running unittests src\lib.rs        test result: ok. 44 passed; 0 failed; 0 ignored
     Running unittests src\main.rs       test result: ok.  0 passed; 0 failed; 0 ignored
     Running tests\behaviour.rs          test result: ok. 14 passed; 0 failed; 0 ignored
     Running tests\e2e.rs                test result: ok. 33 passed; 0 failed; 0 ignored
   Doc-tests img2webp                     test result: ok.  0 passed; 0 failed; 0 ignored

$ cargo test --no-default-features
     Running unittests src\lib.rs        test result: ok. 44 passed; 0 failed; 0 ignored
     Running tests\behaviour.rs          test result: ok. 14 passed; 0 failed; 0 ignored
     Running tests\e2e.rs                test result: ok. 33 passed; 0 failed; 0 ignored
```

- 合計 **91 件、失敗 0**（既定 feature と `--no-default-features` の双方で同一件数）
- コミットメッセージの「91 tests」と一致
- **ドキュメントの不整合**: `implementation.md` §2 Phase 1 の表は e2e「32 件」・単体「41 件」と書いており、現行（33 / 44）と一致しない。§6 の「44 unit + 14 behaviour + 33 e2e」は一致

### 3.2 Python 版との出力比較（`rust/tests/compare_with_python.py` の実測）

```
$ .venv/bin/python rust/tests/compare_with_python.py
...
40/42 fixtures equivalent        (exit code 1)
[DIFF] apng-16bit.png          exit [0, 1]   apng-16bit.webp: missing in Rust output
[DIFF] cmyk-icc.jpg [jpeg-convert]           cmyk-icc.webp: icc differs (lengths [588, 588])
```

- ケース数は **42**（既定 29 ＋ jpeg-convert 5 ＋ lossless 4 ＋ metadata-off 2 ＋ exact 1 ＋ method0 1）。ドキュメントの「41 ケース」は** 1 件少ない**
- 差分は 2 件。1 件は設計どおりの意図的な失敗（16bit APNG）、1 件は §3.1 の合格基準で許容済みの ICC 4 バイト差（sRGB プロファイル作成日時）
- したがって `implementation.md` §7 の「41 ケース中 40 が完全等価、1 件は意図的な失敗」は**件数・内訳とも不正確**（実際は 42 件中 40 等価、差分 2 件でうち 1 件は基準内）
- 付随所見: このスクリプトは許容差を判定に持たないため、**合格基準内の差でも終了コード 1** を返す。CI に組み込むと常時失敗する

### 3.3 §4.3 本計測（`.bench/results.json`、コーパス 111 件、各 3 回の中央値）

生データ（`.bench/results.json`）から再計算:

| カテゴリ | Python | Rust | 倍率 |
| --- | --- | --- | --- |
| 1. AI 生成 PNG 40 件 | 0.41311 s | 0.07542 s | 5.478× |
| 2. Stealth PNG 20 件 | 0.27891 s | 0.08455 s | 3.299× |
| 3. 16bit PNG 10 件 | 0.31902 s | 0.07544 s | 4.229× |
| 4. JPEG 10 件 | 0.15345 s | 0.06349 s | 2.417× |
| 5. GIF 5 件 | 0.47249 s | 0.06346 s | 7.446× |
| 6. APNG 5 件 | 0.23667 s | 0.06372 s | 3.714× |
| 7. 4K 静止画 1 件 | 0.59412 s | 0.30134 s | **1.972×** |
| 8. コピー主体 20 件 | 0.16446 s | 0.06468 s | 2.543× |
| 全体 111 件 | 1.05092 s | 0.35103 s | **2.994×** |

- ピークメモリ: Python 141,856,768 B（135.3 MiB）/ Rust 111,558,656 B（106.4 MiB）
- 出力合計: Python 217,954 B / Rust 218,194 B（**+0.110%**）
- 版: Python 3.10.6 / Pillow 12.0.0 / libwebp 1.6.0

§4.2 判定基準との照合:

| 基準 | 判定 | 数値 |
| --- | --- | --- |
| 静止画バッチで現行比 2 倍以上 | **満たす** | 全体 2.994× |
| 単一ファイルで 5% 超遅くなるカテゴリを作らない | **満たす** | 最遅カテゴリ 7 でも 1.972×（Python より速い） |
| 出力増 3% 以内（案D 採用条件、参考） | 満たす | +0.110% |
| Stealth 負ケースの追加コストが変換本体の 10% 以内 | **測定として成立していない**（下記） | — |

### 3.4 Stealth 負ケースの比率（重要）

`phase0.md` §5 の評価は「カテゴリ 2 と 1 の差」で追加コストを出している:

```
Rust: カテゴリ 2 = 0.084553 s / 20 件 = 4.23 ms/件
      カテゴリ 1 = 0.075415 s / 40 件 = 1.89 ms/件
```

この 2 つの数値は**別コーパス**（Stealth 付き 8/16bit 20 件 vs Stealth 無し AI PNG 40 件）であり、負ケースの追加コストの比較になっていない。しかも文言どおり「差」を追加コストと読むなら **(4.23 − 1.89) / 1.89 ≒ +124%** で、10% 基準を大きく超える。逆に 16bit 負ケースのカテゴリ 3（7.54 ms/件）と比べれば Stealth 付きの方が速く、差は負になる。

つまり:

- 「Stealth 走査を切った同一ファイルでの A/B」という統制比較が**存在しない**（`benchmark.py` に Stealth 無効化の経路は無い）
- したがって設計書 §4.2 の「Stealth 負ケースの追加コストは変換本体の 10% 以内」は**未検証**。`implementation.md` §2 Phase 3 の「判定基準（変換本体の 10% 以内）は満たす」は、この数値では支持できない
- 併せて H3（16bit 負ケースで Stealth が 50% 目安を超える／反証条件は 10% 未満）の判定も記録が無い。設計書 §4.1 の H1〜H5 は、リポジトリ全体を grep しても**判定結果の記述が存在しない**

---

## 4. Phase 6

### 4.1 `python-version-final` = `ee1805d` の妥当性

```
$ git rev-parse c964601^
ee1805dff743a65a8a8d58c2a7c22c4f468005d3
$ git ls-tree -r --name-only ee1805d | grep -c '^rust/'
0
$ git ls-tree -r --name-only ee1805d
.gitignore  LICENSE.md  README.md  config.ini  doc/plan.md  img2webp.bat
img2webp.py  requirements.txt  tests/test_img2webp.py  tests/test_stealth.py
$ git log --oneline --diff-filter=A -- rust/
c964601 Add Rust port of img2webp
$ git log --oneline ee1805d..c964601
c964601 Add Rust port of img2webp
```

- `ee1805d` のツリーに `rust/` は 1 ファイルも無く、Python 版のファイルのみ
- `rust/` を追加したコミットは `c964601` の 1 つだけ、かつ `c964601` の親が `ee1805d`
- よって「Rust 版フォルダ追加の直前時点で Python 版のみを含む最後のコミット」という条件を**満たす**
- タグは annotated（タグオブジェクト `68967a2`、メッセージ「Final state of the Python implementation before the Rust port」）

### 4.2 `img2webp.bat` の書き換え

`c964601` の `img2webp.bat` は書き換え済み:

```bat
if exist "%~dp0img2webp.exe" (
    "%~dp0img2webp.exe"
) else if exist "%~dp0rust\target\release\img2webp.exe" (
    "%~dp0rust\target\release\img2webp.exe"
) else (
    python "%~dp0img2webp.py"
)
set "result=%errorlevel%"
if not "%result%"=="0" pause
exit /b %result%
```

- exe 優先で起動、`pause` の挙動（0 以外で pause）を維持 → 設計書 §6 Phase 6 の要求を満たす
- 設計書の文言は「exe 起動へ書き換える」であり、**Python へのフォールバックは設計書に無い追加**（`implementation.md` §2 Phase 6 に明記済み）。移行期の保険としては妥当だが、exe が欠けたまま配布されると静かに Python 版で動く点は運用上の注意

### 4.3 配布 exe のスモークテスト記録

| 項目 | 記録 |
| --- | --- |
| 実施日時 | 2026-11-27 19:50 JST |
| 実施場所 | `C:\Users\y\AppData\Local\Temp\img2webp-smoke-20261127-195049` |
| 供試個体 | `rust/target/release/img2webp.exe` 4,290,048 B（2026-11-27 18:43:13、`c964601` ビルド） |
| 入力 | 空の `inputs`（config.ini はリポジトリ同梱の内容そのまま） |
| 結果 | 終了コード 0 / 0.24 s / `.img2webp.sqlite3` を `outputs` に生成 / `outputs-dev`・`.img2webp-dev-output` 無し / 起動行無し |
| 判定 | dev-db 無効ビルドとして期待どおり |

**注**: この記録は今回の照合で新規に取得した。リポジトリ内には従来この種のスモークテスト記録が残っていなかった（`implementation.md` §2 Phase 6 は「確認した」と書くのみで、日時・フォルダ・出力の生ログが無い）。

---

## 5. 逸脱・未実施・読み替えの一覧

### 5.1 申告側で明示されている逸脱（妥当）

| # | 内容 | 記録場所 |
| --- | --- | --- |
| 1 | `webp` クレート→`libwebp-sys` 直接 FFI（同梱 libwebp 1.6.0 のため） | phase0 §2, impl §5-1 |
| 2 | ピクチャは ARGB（`use_argb=1`）経路 | impl §5-2 |
| 3 | メタデータ格納は `img-parts` でなく自前 RIFF | impl §5-3 |
| 4 | `cargo metadata` でなく `Cargo.lock` 直接パース | impl §5-4 |
| 5 | sRGB プロファイルはスレッドローカル生成（ICC 4 バイト差） | impl §5-5 |
| 6 | 16bit グレースケールは `min(v,255)` クリップ | impl §5-6 |
| 7 | GIF bgcolor は `(i,i,i,255)` | impl §5-7 |
| 8 | UserComment UTF-16BE / XPComment UTF-16LE | impl §5-8 |
| 9 | casefold は主要差分のみ写像 | impl §5-9 |
| 10 | 文言は実装裁量（16bit APNG、寸法上限、SOF、停滞、パニック、起動時 WARN 等） | impl §5-10 |
| 11 | FFI フラグをスレッドローカル化（誤判定防止） | impl §5-11 |
| 12 | UNC 出力で WAL 不可時に `journal_mode=DELETE` へフォールバック | impl §5-12 |
| 13 | テスト用フック（`Options`）を追加、本番に差し替え経路なし | impl §5-13 |
| 14 | 意図的な変更 1〜14（権限 umask、署名 pipeline 3、16,384px 事前失敗、両方表示、予算、依存版で再変換、16bit APNG 未対応、ERROR 切詰め、競合失敗、2 回目中断で commit なし、UTF-8、整数表記制限、表示失敗を成否に数えない、パニック時 commit なし） | impl §4 |

いずれも設計書 §5 の枠内（または明示的な追加記録）で、**黙った逸脱ではない**。

### 5.2 申告側で「未達・未検証」と明示されている項目

- 実際の FFI パニックの注入（分類ロジックの単体テストで代替）— 設計書 §6 Phase 5 は「FFI パニックが起きたときの故障注入テストが通ること」を要求しており、**代替で通した扱い**
- 標準出力・標準エラーへの書き込み失敗（閉じたパイプ）の自動テスト
- A1111／NovelAI の実画像での読み戻し（同構造フィクスチャで代替）
- コールド I/O 状態での計測（§4.2 は「コールド／ウォームを区別して記録」を要求）
- 長尺アニメーション実コーパスでの安全係数の再確認

### 5.3 今回の照合で新たに見つかった未実施・不整合

| # | 種別 | 内容 | 影響 |
| --- | --- | --- | --- |
| A | **未実施** | `WebPConfig` 全フィールド対応表が存在しない（設計書 §6 Phase 0 の D-ENC 明示要求） | Phase 0 完了条件の一部が成果物として欠落。コードは 6 フィールドのみ設定し、残りは既定値という判断の記録が無い |
| B | **未実施** | FFI パニック時の Drop 挙動の確認記録が無い（同上） | `impl Drop for Picture` は存在するが、巻き戻し時の安全性（二重解放・`WebPPictureFree` の再入）を確認した証跡が無い |
| C | **読み替え** | アニメーション lossless／method の「Phase 0 での試作確認」が、Phase 4 の実装＋比較テストに置き換わっている | 設計書 §6 前文の内包規定で手続き上は許容。ただし Phase 0 時点での決着ではない |
| D | **未検証（重要）** | Stealth 負ケースの 10% 基準が統制比較で測られていない。引用された数値（4.23 vs 1.89 ms/件）は別コーパスで、素直に差を取ると +124% | Phase 3 完了条件「負ケースのコストが 10% 以内」が実質未確認。`implementation.md` の「満たす」は根拠不足 |
| E | **未実施** | 設計書 §4.1 の H1〜H5 の判定結果がどこにも記録されていない | §4.3 は「仮説の検証」を目的にしており、判定表が無いまま本計測のみ記録されている |
| F | **未実施** | §4.2 が要求する「最適化 GIF の頻度の標本調査（実入力から数百件抽出）」が未実施・未記録 | BUG-01 の頻度根拠が「不明」のまま（`phase0.md` は言及なし） |
| G | **矛盾** | 安全係数: `implementation.md` §2 Phase 5 は「暫定 2.0 を維持すると決定＝確定」、同 §5 の項目 12 は「2.0 の暫定のまま（Phase 5 の確定項目は未達）」 | どちらが正か不明。設計書 §6 Phase 0 は「安全係数は暫定値の決定をもって決着」なので、Phase 5 で確定させる必要は元々無い（記述の整理漏れ） |
| H | **不整合** | `implementation.md` §2 Phase 1 のテスト件数（e2e 32 / unit 41）が現行（33 / 44）と不一致 | 証跡の数値が古い |
| I | **不整合** | `compare_with_python.py` は 42 ケース・差分 2 件。docs は「41 ケース・40 等価・1 件意図的失敗」 | §3.1／§7 の等価性証跡が件数・内訳とも不正確。加えて合格基準がスクリプトに実装されておらず終了コード 1 |
| J | **証跡なし** | `--locked` のビルド証跡がビルドログ等として残っていない（Cargo.lock 未変更のみ） | Phase 6 完了条件「`--locked` でビルドされたことも確認する」に対する直接証跡が無い |
| K | **証跡なし（文書）** | PNG の CRC 不一致の結論にフィクスチャ／テストが無い（`broken.png` は非 PNG データ） | 結論自体は妥当だが、実測裏付けが薄い |
| L | **弱い証跡** | JPEG／GIF コメント取得経路の結論が `phase0.md` §7 の 1 行のみ | §3.x に結論節が無い |
| M | **弱い証跡** | 窓口テスト `unc_paths_are_supported_for_input_and_output` は管理共有不達時に無言スキップ | 今回は共有到達を確認して実行されたが、CI 等では緑が実力を保証しない |
| N | **文書欠陥** | `implementation.md` §5 の番号が「12」「13」の後に再び「12」 | 参照時の混同要因 |

### 5.4 総合判定

- **完了条件の骨格は満たしている**: Phase 1〜5 の自動テスト 91 件が両 feature 構成で通過、画素等価性は許容基準内、§4.3 本計測は判定基準（2 倍以上・劣化カテゴリ無し）を満たし、Phase 6 のタグ・bat・配布 exe 個体の dev-db 非混入も実測で確認できた。
- **ただし完了申告として未了なのは次の 4 点**:
  1. **D**: Stealth 負ケース 10% 基準の測定（Phase 3 完了条件）
  2. **A / B**: D-ENC 内包分の成果物 2 件（`WebPConfig` 対応表、FFI パニック時の Drop 確認）
  3. **E / F**: 仮説 H1〜H5 の判定と GIF 頻度の標本調査（§4.1／§4.2 の要求）
  4. **G〜N**: 記録の不整合・欠落の整理（数値の更新、安全係数の記述統一、`--locked` と CRC の証跡補強）

「完了」と宣言する前に、D の A/B 計測（Stealth 走査の on/off、または Stealth 無しコーパスでの 16bit 負ケース単独計測）と、A／B／E／F の実施または「未実施として完了条件から除外する」判断の記録が必要。

> **追記**: 上記 A〜N への対応結果は本ファイル末尾の「追記（2026-11-27、A〜N の実施結果）」§6〜§10 にある。A／B／D／E は実施して証跡を取得、F は実入力が無いため未実施（決定依頼）、G〜N は docs と実装を実態に合わせて修正した。G〜N の修正に伴い、K（PNG の CRC 不一致）については**新しい非互換**が見つかった（§7 の補足と §8-2）。

---

# 追記（2026-11-27、A〜N の実施結果）

§5.3 で挙げた未実施・不整合 A〜N への対応結果。**push は行っていない。**

- 計測環境: Intel i5-13600KF（14 コア／20 スレッド）、物理メモリ 63.8 GiB、Windows 11 + WSL2 Ubuntu 24.04、コーパスは NTFS（`/mnt/c`、drvfs 経由）。`cargo`/`rustc` 1.91.1（stable-x86_64-pc-windows-msvc）
- 追加した計測ツール（再現可能な形で残した）
  - `rust/examples/profile_pipeline.rs` — 静止画 1 件の段階別所要時間（probe / decode / stealth / prepare / color / encode / mux / write / verify）を中央値で出す。`--stealth on|off` で同一ファイルの統制比較ができる
  - `rust/tests/bench_parallel.py` — 同一コーパスで `parallel_jobs` を変えて総時間を測る（H1／H5）
  - `rust/tests/bench_memory.py` — 長尺 APNG のピークワーキングセット（H4）。Windows Python で実行
- 使用コーパス（`.bench/corpus/`、いずれも `benchmark.py` の生成物を流用）
  - `cat1` = AI 生成 PNG 40 件（Stealth 無し 8bit）、`cat2` = Stealth PNG 20 件、`cat3` = 16bit PNG 10 件、`cat3big` = 16bit 1024×1024 3 件、`cat1x10` = cat1 の 10 倍 400 件、`cat8x10` = コピー 200 件、`apng-long` = 60/300 フレーム APNG

## 6. 実施結果

### 6.1 D: Stealth 負ケースの統制比較（最優先項目）

方法: `profile_pipeline` で**同一ファイル**を `--stealth on` と `--stealth off` の 2 条件で実行し、各段階の中央値（7 回、`cat3big` のみ 5 回）を比較した。負ケース（ペイロード無し）では `off` と `on` の差がそのまま走査コストになる。判定は設計書 §4.2 の「変換本体（＝Stealth を除く処理）の 10% 以内」。

| コーパス | 変換本体（off の総時間） | Stealth 走査 | 本体比 | 総時間比 | 判定 |
| --- | --- | --- | --- | --- | --- |
| `cat1`（AI PNG 40 件・8bit・ペイロード無し） | 125.537 ms | 0.117 ms | **0.09%** | 0.09% | 基準内 |
| `cat3`（16bit 256×256 10 件・ペイロード無し） | 52.629 ms | 10.089 ms | **19.0%** | 16.0% | **基準超過** |
| `cat3big`（16bit 1024×1024 3 件・ペイロード無し） | 191.899 ms | 39.724 ms | **20.6%** | 17.1% | **基準超過** |
| `cat2`（Stealth あり 20 件・参考） | 45.002 ms | 3.327 ms | 7.2% | 6.7% | 参考（負ケースではない） |

結論:

- **8bit 負ケースは 0.09% で基準を大きく満たす**（8bit の走査はデコード済み画素の LSB 読みだけで、40 件で 0.12 ms）。
- **16bit 負ケースは 19.0〜20.6% で基準（10%）を満たさない**。16bit 経路は IDAT を全展開してからヘッダ範囲のフィルタを復元するため、ペイロードが無くても展開コストを払う（H3 が指摘した構図のとおり）。
- 旧記録の「カテゴリ 2 と 1 の差」による評価は、別コーパス間の比較で基準の判定に使えない（その数値は 4.23 ms/件 vs 1.89 ms/件で、差を取ると +124%）。**Phase 3 完了条件は未達**として扱い、是正／基準の扱いを決定依頼（§8-1）に分離した。
- H3（16bit 負ケースで Stealth が 50% 目安を超える／反証は全サンプル 10% 未満）は**反証されない**（16% は 10% 未満ではない）。

### 6.2 A: `WebPConfig` 全フィールド対応表

`doc/img2webp-rust-webpconfig-mapping.md` を作成した。libwebp 1.6.0（`libwebp-sys` 0.14.4 の `vendor/src/webp/encode.h` と `vendor/src/enc/config_enc.c`）を一次資料とし、**29 フィールドすべて**について「libwebp 既定／Pillow 静止画／Pillow アニメ／img2webp 静止画／img2webp アニメ」を並べた。

判明した確定事項:

- 明示設定は 6 フィールドのみ（`quality`/`lossless`/`method`/`exact`/`alpha_quality`/`thread_level`）。Pillow も同じ 6 フィールド（アニメの `exact` を除く）しか設定しない。
- **アニメーションで `exact` を無視するのは仕様どおり**。Pillow の `_save_all` は `exact` を `WebPAnimEncoder.add` に渡さない（`PIL/WebPImagePlugin.py` で確認）ため、Pillow 側も 0 になる。照合時に疑義が出たが欠落ではない。
- `method` の既定は静止画 4／アニメ 0。Pillow の既定（`_save` 4、`_save_all` 0）と一致する。

### 6.3 B: FFI パニック時の Drop 挙動

コード監査の結果（`rust/src/webp_encode.rs`）:

| 資源 | 取得 | 解放 | 巻き戻し時の挙動 |
| --- | --- | --- | --- |
| `WebPPicture` | `Picture::new`（`WebPPictureAlloc`） | `impl Drop for Picture` → `WebPPictureFree` | **Drop が走るので解放される**。`Picture` を構築できなかった場合は構築前に 1 回だけ `Free` して `Err` を返すため二重解放にならない |
| `WebPMemoryWriter` のバッファ | `WebPMemoryWriterInit` | `WebPMemoryWriterClear`（成功時と libwebp エラー時） | **RAII が無いため、初期化と clear の間で巻き戻ると確保済みバッファが漏れる**（未定義動作ではない） |
| `WebPAnimEncoder` | `WebPAnimEncoderNewInternal` | 各エラー経路と正常経路で `WebPAnimEncoderDelete` | **RAII が無い**ため、同一スコープ内でパニックすると漏れる |
| `FFI_ACTIVE` フラグ | `set_ffi_active(true)` | `set_ffi_active(false)` | **RAII が無い**ため、`true` の間にパニックするとフラグが立ったままになり、以後の純 Rust パニックが「FFI パニック（プロセス終了）」と誤分類されうる |

ただし `true`→`false` の間に存在するのは libwebp の C 呼び出しだけで、**libwebp は Rust へコールバックしない**（書き込みコールバックも C 側の `WebPMemoryWrite`）。したがって現行コードでこの窓から Rust パニックが始まる経路は無く、「到達不能な潜在欠陥」である。

確認テストを追加した（`cargo test` で実行、いずれも通過）:

- `webp_encode::tests::picture_is_freed_when_the_thread_unwinds` — `WebPPicture` を生かしたまま `catch_unwind` でパニックさせ、巻き戻し後に (1) `ffi_active()` が false、(2) その後の `encode_still` が正常に `RIFF` を返す（ヒープが壊れていない）ことを検証
- `webp_encode::tests::ffi_flag_is_thread_local` — フラグがスレッドローカルであること（別スレッドから観測できないこと）を検証

RAII ガードの追加は**行っていない**（`rust-v0.1.0` の成果物を変更しないため）。必要なら決定依頼（§8-4）。

### 6.4 E: 仮説 H1〜H5 の判定

| 仮説 | 反証条件（§4.1） | 実測 | 判定 |
| --- | --- | --- | --- |
| H1 静止画バッチはジョブ並列でコア数近くまで縮む | 物理コア数まで上げても短縮 20% 未満 | `cat1x10`（400 件）: jobs=1 で 1.5111 s → jobs=14 で 0.3307 s（**4.57×、短縮 78%**）。jobs=2 0.8336、4 0.4889、8 0.3566 | **反証されない**（短縮は 20% を大きく超える）。ただし「コア数近く」（14 コアで 14×）は**支持されない**。1→2 で 1.81×、2→4 で 1.71×、4→8 で 1.37×、8→14 で 1.08× と逓減する |
| H2 1 ファイルの大半は WebP エンコードで、デコード・メタデータは少数派 | デコード＋Stealth＋検証が処理時間の 25% 以上 | 段階別実測（Stealth on）: `cat1` (9.02+0.12+24.85)/126.01 = **27.0%**、`cat2` 39.2%、`cat3` 44.9% | **反証された**（25% 以上）。エンコードは 55〜63% で依然最大だが、「少数派」とは言えない |
| H3 16bit 負ケースで Stealth が 50% 目安を超える | 全サンプルで 10% 未満 | `cat3` 16.0%、`cat3big` 17.1%（総時間比） | **反証されない**（50% には届かないが 10% 未満でもない）。§6.1 のとおり §4.2 の 10% 基準は超過 |
| H4 アニメーションは全フレーム保持がメモリ上限になり並列度を制限する | 長尺 APNG でもメモリが問題にならない | 60f/512: ピーク 75.8 MiB・見積り 216 MiB・0.39 s。300f/512: ピーク **319.5 MiB**・見積り **936 MiB**・1.70 s。60f/1024: ピーク **277.5 MiB**・見積り **768 MiB**・1.38 s。予算（8 GiB にクランプ）に対する同時実行可能数は 8／10／37 で、20 スレッドを埋められない | **反証されない**（長尺では予算が実際に同時実行数を制限する）。ただし失敗はせず、制限として現れる |
| H5 コピー主体バッチは I/O 律速で並列化しても短縮しない | 並列化で総時間が 10% 以上短縮 | `cat8x10`（200 件）: jobs=1 0.2245 s → jobs=14 0.1639 s（**短縮 27%**、1.37×） | **反証された**（10% 以上短縮した） |

補足: H2 の `verify` 段階は保存後の再読み込みを含むため NTFS（drvfs）の I/O を拾う。ただし本番コードも同じ再読み込みを行うので、段階構成としては本番同等である。H1／H5 の測定はプロセス起動オーバーヘッド（約 20〜30 ms）を含む。

### 6.5 F: GIF 頻度の標本調査

**実施できなかった。** 設計書 §4.2 は「利用者の実際の入力フォルダ群全体を母集団として無作為に数百件程度を抽出し、GIF の最適化有無を数える」ことを要求するが、この PC 上に利用者の実入力は存在しない:

```
$ ls -la inputs/
.img2webp.bat - ショートカット.lnk   （ファイル 1 個のみ）
```

`.bench/corpus` と `rust/tests/fixtures` はいずれも合成データで、母集団の代用にならない（最適化 GIF の割合を意図的に作ったもの）。実施には入力フォルダの場所の指定が必要（§8-3）。未実施であることを `doc/img2webp-rust-phase0.md` 側にも明記した。

## 7. G〜N の修正（docs と実装の突き合わせ）

| # | 元の不整合 | 対応 | 反映先 |
| --- | --- | --- | --- |
| G | 安全係数が「Phase 5 で確定」と「暫定のまま（未達）」で矛盾 | Phase 5 の決定（実測比較のうえ 2.0 据え置き）を正とし、§5 の記述を「2.0 で確定」に統一。Phase 0 の決着範囲（暫定値の決定）とも整合させた | `img2webp-rust-implementation.md` §5 |
| H | Phase 1 のテスト件数が 32／41 で現行と不一致 | 照合時の実測値（e2e 34 件、単体 46 件）に更新し、差分の理由（追加した 3 テスト）を併記 | 同 §2 Phase 1 |
| I | 比較ケース数 41・「40 等価／1 意図的失敗」 | 42 ケース、40 等価、差分 2 件（16bit APNG＝意図的、`cmyk-icc.jpg`＝§3.1 の基準内 ICC 4 バイト差）に修正。合格基準がスクリプトに実装されておらず終了コード 1 になる点も明記 | `img2webp-rust-implementation.md` §7、`img2webp-rust-phase0.md` §3.1 |
| J | `--locked` の直接証跡が無い | `cargo build --release --no-default-features --locked` を実行して成功を確認（`Finished release profile ... in 3.08s`）。`Cargo.lock` が `c964601` から未変更であることも確認。結果を証跡欄に追記 | `img2webp-rust-implementation.md` §2 Phase 6 |
| K | PNG CRC の結論に裏付けが無く、しかも内容が誤り | フィクスチャ `crc-mismatch.png` を追加（`generate_fixtures.py::write_crc_mismatch`）し、e2e テスト `png_with_a_bad_chunk_crc_fails_the_job` を追加。**実測で Pillow は CRC を検証せず変換成功、Rust は失敗**と判明したため結論を訂正（互換性の後退として記録） | `img2webp-rust-phase0.md` §3.6、`rust/tests/e2e.rs`、`rust/tests/fixtures/` |
| L | JPEG／GIF コメントの結論がチェック表の 1 行だけ | 取得経路（JPEG は COM=`0xFE`、GIF はコメント拡張ブロック）と裏付け（`probe.rs::gif_frames_loop_and_comment`、比較での UserComment 一致）、限界（JPEG の COM フィクスチャが無い）を §3.6 に追記 | `img2webp-rust-phase0.md` §3.6 |
| M | UNC テストが共有不達時に無言スキップ | スキップ時に `eprintln!` で理由を出すように変更。今回は `--nocapture` で `RUN ... using \\localhost\C$\...` が出力されることを確認（実測で実行された） | `rust/tests/e2e.rs` |
| N | `img2webp-rust-implementation.md` §5 の番号重複（12 が 2 つ） | 安全係数の項目を 14 に改番（11 UNC、12 テスト用フック、13 …の順序に整理） | 同 §5 |

補足（K で判明した新しい論点）: CRC の扱いは**未決の互換性問題**である。Python 版が成功するファイルを Rust 版が失敗させるため、設計書 §5「互換方針」に対する後退にあたる。`png` クレートは `set_ignore_crc` を持つため合わせる実装は可能だが、成果物（タグ済み）を変更することになるため未実施とし、決定依頼に分離した（§8-2）。

## 8. 決定依頼（読み替えずに判断をお願いしたい項目）

1. **D: Stealth 16bit 負ケースの 10% 基準をどう扱うか**（Phase 3 完了条件）
   実測は 19.0%（256×256）、20.6%（1024×1024）で基準超過。選択肢は (a) 是正実装（16bit 走査で IDAT 全展開を避ける／展開結果を再利用する等。設計変更を伴う）、(b) 基準を「8bit は 10%、16bit は別枠（例: 25%）」に改訂（§4.2 の緩和は設計書改訂＝再レビュー対象）、(c) 未達のまま完了扱いにしない（現状）。**推奨は (a) の検討着手**。根拠: 16bit は負ケースでも全展開しており、`png_raw` の走査範囲を「ヘッダが必要とする行まで」に絞る余地がある。
2. **K: PNG の CRC 不一致で Pillow と挙動が違う点をどうするか**
   Python 版は変換成功（終了コード 0）、Rust 版はジョブ失敗（終了コード 1）。選択肢は (a) `png` クレートの CRC 検証を無効化して Pillow に合わせる、(b) 意図的な変更として設計書 §5 の一覧に追加（互換の後退を明記）、(c) 現状維持で未決のまま。（(a) か (b) の即断を推奨。放置は「記録のない非互換」になる）
3. **F: GIF 頻度の標本調査の実施手段**
   実入力フォルダのパス指定（または対象外とする判断）が必要。指定があれば「数百件を無作為抽出し、GIF の最適化有無と disposal を数える」スクリプトを用意して実施する。
4. **B: FFI 周りの RAII ガード追加の可否**
   `WebPMemoryWriter`・`WebPAnimEncoder`・`FFI_ACTIVE` に RAII ガードを入れれば、巻き戻し時のリークと誤分類の可能性を構造的に消せる。ただし `rust-v0.1.0`（`c964601`）の成果物を変更することになるため未実施。追加する場合はタグの切り直し（`rust-v0.1.1` 等）と再ビルド・再スモークテストを伴う。
5. **I: 比較スクリプトに合格基準を実装するか**
   現状は基準内の差（ICC 4 バイト）でも終了コード 1 になり、CI に置けない。許容差を引数化して「基準内なら 0」にすると証跡として使いやすくなる（テストコードのみの変更）。
6. **J の副作用の承認: 配布 exe の再生成**
   `--locked` の確認ビルドが既存の `rust/target/release/img2webp.exe` を再生成した（2026-11-27 20:12、4,290,048 バイト）。**本番コードの差分はゼロ**（`rust/src` の変更は `#[cfg(test)]` の追加 41 行のみ、`git diff -U0 -- rust/src` で確認）なので機能は同一だが、18:43 のファイルそのものではなくなった。再生成後の個体で dev-db 非混入のスモークテストと比較（40/42）を再実施し、同じ結果を得ている。18:43 の個体を厳密に保持したい場合は、タグからの再ビルド手順を別途合意したい。

## 9. 更新後の状態（再現コマンド）

```
$ cargo test
  46 unit + 14 behaviour + 34 e2e = 94 件、失敗 0        （既定 feature）
$ cargo test --no-default-features
  同じ 94 件、失敗 0
$ python3 doc/tools/check_spec_references.py
  all cross references resolve
$ .venv/bin/python rust/tests/compare_with_python.py
  40/42 fixtures equivalent（差分は apng-16bit.png と cmyk-icc.jpg、いずれも説明済み）
$ python rust/tests/bench_parallel.py --corpus .bench/corpus/cat1x10 --jobs 1 2 4 8 14 --runs 5
  speedup 4.569x（H1）
$ .venv/bin/python rust/tests/compare_with_python.py crc-mismatch.png   # CRC の差の再確認
  Python 版 exit 0 / Rust 版 exit 1
```

`cargo test` の増分は、照合で追加した 3 件（`picture_is_freed_when_the_thread_unwinds`、`ffi_flag_is_thread_local`、`png_with_a_bad_chunk_crc_fails_the_job`）。

## 10. 変更ファイル一覧（すべて未コミット・未 push）

```
 M doc/img2webp-rust-implementation.md      … G/H/I/J/N の修正
 M doc/img2webp-rust-phase0.md              … I/K/L の修正、D-ENC 補完へのリンク、D の再計測注記
 M rust/src/webp_encode.rs                  … #[cfg(test)] のテスト 2 件のみ（本番コード変更なし）
 M rust/tests/e2e.rs                        … CRC テスト追加、UNC スキップの可視化
 M rust/tests/fixtures/generate_fixtures.py  … write_crc_mismatch()
?? doc/img2webp-rust-completion-evidence.md … 本ファイル
?? doc/img2webp-rust-webpconfig-mapping.md  … A（29 フィールド対応表）
?? rust/examples/profile_pipeline.rs        … D/H2/H3/H4 の段階別計測
?? rust/tests/bench_parallel.py             … H1/H5
?? rust/tests/bench_memory.py               … H4（ピークメモリ）
?? rust/tests/fixtures/crc-mismatch.png     … K のフィクスチャ
```

作業用の一時物（リポジトリ外／gitignore 済み）: `%TEMP%\img2webp-smoke2-20261127-201233`、`.bench/corpus/{cat1,cat2,cat3,cat3big,cat1x10,cat8x10,apng-long,empty}`、`.bench/tmp/`、`.bench/parallel-*.json`、`/tmp/cmp*.log`。

---

# 11. Stealth 走査の是正（A＋設定項目）

2026-11-27 の利用者決定「A（早期打ち切り）＋ config.ini オプション」に基づく実装と再計測。§6.1 で未達と判定した Phase 3 完了条件（Stealth 負ケースのコストが変換本体の 10% 以内）を、基準を緩めずに満たした。

## 11.1 実装

| 変更 | ファイル | 内容 |
| --- | --- | --- |
| 16bit 走査の 2 段化 | `rust/src/png_raw.rs`、`rust/src/stealth.rs` | ペイロードのシグネチャは先頭 120 ビット（15 バイト）で判定できる。第 1 段はその 120 ビットが載る行数（`ceil(120 / slots)`、最大 `height`）だけを復号し、シグネチャが一致しなければそこで打ち切る（IDAT の全展開も全行走査もしない）。一致したときだけ従来どおり全体を復号して長さとペイロードを読む。`png_raw::png_16bit_rows` が行数指定の復号を提供し、`png_16bit_pixels(header_only)` は同じ内部実装を使う |
| 設定項目 | `rust/src/config.rs`、`config.ini`、`rust/tests/fixtures/config-fixture.ini` | `[preserve] stealth`（既定 `true`、`false` で走査せず WARN）、`[preserve] stealth_max_pixels`（既定 `16,777,216` 画素、超過する 16bit は走査を省略して WARN、`0` で無制限）。どちらも省略可能（設計書 §5 の新項目の規則どおり） |
| 適用箇所 | `rust/src/convert.rs::skip_stealth_scan` | 上限判定は 16bit PNG のみ。8bit の走査は元から十分安いので上限の対象外（`stealth = false` は全体に効く） |
| 変換シグネチャ | `rust/src/state.rs` | 生成テキストが変わるため `stealth`／`stealth_max_pixels` を署名に含め、`pipeline` を **4** に上げた（既存の変換記録は一度だけ作り直し） |

`git diff` の本番コードはこの 5 ファイル（`config.rs`／`convert.rs`／`png_raw.rs`／`stealth.rs`／`state.rs`）のみ。

## 11.2 再計測（同一ファイルの Stealth on/off、`profile_pipeline`、7 回の中央値）

| コーパス | 変換本体（off） | Stealth 走査 | 本体比 | 是正前 |
| --- | --- | --- | --- | --- |
| `cat1`（AI PNG 40 件・8bit・ペイロード無し） | 109.366 ms | 0.113 ms | **0.10%** | 0.09% |
| `cat3`（16bit 256×256 10 件・ペイロード無し） | 46.377 ms | 2.589 ms | **5.58%** | 19.0% |
| `cat3big`（16bit 1024×1024 3 件・ペイロード無し） | 191.186 ms | 7.159 ms | **3.74%** | 20.6% |

すべて §4.2 の基準（10% 以内）を満たす。**基準は変更していない**（緩和なし）。

上限設定の動作確認（`stealth_max_pixels = 1000` で 1024×1024 の 16bit 3 件）:

```
CONVERT big16-0.png -> big16-0.webp
WARN big16-0.png: Stealth PNG Info の走査を省略しました（1024x1024 = 1048576 画素は上限 1000 画素を超えています）
完了: 変換 3 / コピー 0 / 再利用 0 / 登録 0 / 失敗 0
```

変換は成功し、隠しテキストだけが引き継がれない（WARN で明示）。`stealth = false` でも同じく WARN を出して省略する。

## 11.3 回帰確認

```
$ cargo test                       # 48 unit + 14 behaviour + 36 e2e = 98 件、失敗 0
$ cargo test --no-default-features # 同じ件数、失敗 0
$ python3 doc/tools/check_spec_references.py    # all cross references resolve
```

- 画素等価性: `compare_with_python.py` は**比較可能な 36 ケース中 34 ケースが完全等価**、差分 2 件（16bit APNG＝意図的な失敗、`cmyk-icc.jpg`＝基準内の ICC 4 バイト差）。16bit Stealth の 5 フィクスチャはすべて一致（ペイロード抽出結果も一致）
- 追加テスト: `stealth::sixteen_bit_prefix_matches_the_full_decode`（部分復号と全体復号の画素一致）、`config::stealth_keys_default_and_accept_zero_as_unlimited`、`e2e::stealth_scan_is_skipped_above_the_pixel_cap`、`e2e::stealth_scan_can_be_disabled_in_the_config`、`state::convert_signature_shape_is_pinned`（pipeline 4 のゴールデン）

## 11.4 測定側で見つかった欠陥（比較スクリプト）

`compare_with_python.py` は、テンプレートに無い設定（`lossless`／`exact`／`method`）のパッチを黙って捨てていた。つまり §6.2 まで「lossless／exact／method0 の変種で一致」としていた記述は、**既定設定どうしを比較していただけ**で、その設定を検証していなかった。

- 修正: パッチを該当セクションに挿入する（`patched_config(..., for_python=...)`）。Python 版が表現できない設定は Python 側の設定から除去し、それを使う 6 ケースは `[RUST-ONLY]` として比較対象外にした（`tests/e2e.rs` が個別に検証する）
- したがって件数の読み替え: 旧「42 ケース中 40 等価」→ 新「比較可能 36 ケース中 34 等価（＋Rust 専用 6 ケース）」。等価でない 2 件の内訳は変わらない
- 残る改善候補（決定依頼 §8-5）: 合格基準（ICC の 4 バイト差の許容）をスクリプトに実装して終了コードを意味のあるものにする

## 11.5 仕様上の注意（利用者向け）

- `stealth = false` または上限超過で走査を省略すると、**Stealth PNG Info の隠しテキストは WebP に引き継がれない**（画素・Exif・ICC・XMP は通常どおり）。WARN 行で必ず通知する
- 既定（`stealth = true`／16,777,216 画素）では、4096×4096 までの 16bit PNG が従来どおり走査される
- 設定を変えると変換シグネチャが変わるため、次回実行時に変換済みファイルが一度だけ作り直される（コピーは再利用される）

## 11.6 依然として残る決定依頼（§8 の更新）

| # | 項目 | 状態 |
| --- | --- | --- |
| 8-1 | Stealth 16bit 負ケースの 10% 基準 | **解決**（A の実装で 5.3%/3.7% を達成。基準は据え置き） |
| 8-2 | PNG の CRC 不一致で Pillow と挙動が違う | 未決（現状は Rust が失敗側。`png` クレートの CRC 検証を切るか、意図的な変更として §5 に載せるか） |
| 8-3 | GIF 頻度の標本調査 | 未実施（実入力フォルダの指定が必要） |
| 8-4 | FFI 周りの RAII ガード | 未実施（タグ済み成果物を変えるため保留。§6.3 のとおり現行は到達不能） |
| 8-5 | 比較スクリプトの終了コード | 未実施（§11.4） |
| 8-6 | 配布 exe の再生成 | 記録済み（`--locked` 確認ビルドで 2026-11-27 20:12 に再生成。本番コードの差分なし。今回の Stealth 変更でさらに再ビルドされる） |
