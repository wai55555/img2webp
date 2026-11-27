# img2webp Rust 版 移行手順とロールバック

対象: 既存の Python 版 img2webp を Rust 版へ置き換える利用者。

- Python 版の最終状態: タグ `python-version-final`（Rust 版フォルダ追加の直前のコミット）
- Rust 版のリリース: タグ `rust-v0.1.0`
- 実装の詳細: [`img2webp-rust-implementation.md`](img2webp-rust-implementation.md)

## 1. 前提

- 主対象は Windows。Rust 版は単一 exe（CRT・libwebp・lcms2・SQLite を静的リンク）。
- 設定ファイルの場所は変えない。**exe の配置フォルダが `config.ini` と状態 DB の基準フォルダ**になる（`config.ini` は exe の隣、状態 DB は出力フォルダ内）。移行前と配置が変わると、設定と履歴が別の場所を指して移行が静かに不発になる。**移行前と同じフォルダに exe を置くこと。**
- `config.ini` は現行のまま動く。新しい任意項目（`[webp] lossless`/`method`/`exact`、`[output] parallel_jobs`）は省略可能で、省略時は現行と同じ挙動（lossless 無効、method 静止画 4／アニメーション 0、exact 無効、並列度は自動）。

## 2. 移行手順

1. **処理を止める**（実行中の img2webp を終了する）。
2. **バックアップを取る**。出力フォルダの状態 DB 一式と出力フォルダ全体をコピーする。
   - 状態 DB: `.img2webp.sqlite3`、`.img2webp.sqlite3-wal`、`.img2webp.sqlite3-shm`、`.img2webp.sqlite3-journal`（存在するものだけ）
   - 出力フォルダ（画像を含む）
3. リポジトリから `rust/` を取得し、リリースビルドを作る（ソースからビルドする場合）:
   ```shell
   cd rust
   cargo build --release --no-default-features --locked
   ```
   - `--no-default-features` は必須。既定の feature `dev-db` は開発用の DB 名と出力先（`outputs-dev`）に切り替えるため、付けたままだと実 DB を使わない。
   - `--locked` は `Cargo.lock` のとおりにビルドする（リリースでは必須）。
   - `--all-features` は使わない。
4. `rust/target/release/img2webp.exe` を、**移行前の `img2webp.py` と同じフォルダ**へ置く（`img2webp.exe` という名前）。
5. 一度実行して確認する:
   - 標準エラーに「開発ビルド」の行が出ないこと。
   - 出力フォルダに `.img2webp.sqlite3` があること（`.img2webp-dev.sqlite3` ではない）。
   - `CONVERT`／`COPY`／`SKIP` の表示と最後の「完了: …」行が出ること。
6. 初回は変換対象（PNG・GIF・WebP 変換を選んだ JPEG）が**一度だけ**作り直される。コピー対象（JPEG・AVIF・WebP・その他）は状態 DB の記録がそのまま再利用される。作り直しは変換シグネチャの `pipeline` が 2→3 になり、依存版の識別が加わるためで、意図した一度きりの移行コスト。
7. 移行後は `img2webp.bat` が exe を起動する（exe が無い場合だけ Python 版へフォールバックする）。

## 3. 移行時の注意

- **状態 DB だけを差し替えないこと。** 起動時の一時ファイル掃除は「状態 DB に記録が無い一時ファイル形状の名前」を削除対象にする。DB と出力の対応が崩れていると、正規の出力が削除対象になりうる。DB を戻す場合は出力フォルダもバックアップから戻す。
- 一時ファイル形状（`.img2webp-` ＋ 8 文字 ＋ 拡張子）の名前を持つ未追跡ファイルは、初回起動の掃除で削除されうる。そのような名前のファイルを出力フォルダに置かない。
- 出力ファイルの権限が通常の作成権限（umask 準拠）になる（Python 版は 0600 だった）。
- 一辺 16,384px 以上の画像は、デコード前に WebP の上限超過として失敗する。
- 16bit APNG は未対応で、ジョブ単位の失敗になる（Python 版は変換できた）。8bit APNG は従来どおり。
- 依存（デコーダ・CMS・エンコーダ）の版が変わると、変換済みファイルが全件再変換される。`Cargo.lock` を更新した場合はその前提で運用する。
- メモリ予算（利用可能メモリ×0.5、256MiB〜8GiB）を超える見積もりのジョブは失敗する。極端に長尺なアニメーションや巨大な静止画は、Python 版で成功していても失敗しうる。
- 標準出力・標準エラーは UTF-8 で書く。日本語 Windows のコンソールで文字化けする場合は `chcp 65001` を設定する。
- エラー行のメッセージは 4,096 バイトで切り詰められる（末尾に `…`）。

## 4. ロールバック（Python 版へ戻す）

1. 処理を止める。
2. 移行時に取ったバックアップから、状態 DB 一式と出力フォルダを復元する。
   - 「Rust 版の出力をそのまま残す」場合は復元しない。ただし `overwrite_existing = false` のままだと宛先が存在するため Python 版は再変換しない（`true` にすると再変換される）。
3. `config.ini` から新しい任意項目（`lossless`／`method`／`exact`／`parallel_jobs`）を削除する。Python 版は未知キーを設定エラーにする。
4. `img2webp.exe` を退避し、タグ `python-version-final` の `img2webp.py`・`img2webp.bat`・`config.ini` を元のフォルダへ戻す。
   ```shell
   git show python-version-final:img2webp.py > img2webp.py
   git show python-version-final:img2webp.bat > img2webp.bat
   ```
5. `python img2webp.py` で起動し、終了コードと表示を確認する。

- 状態 DB のスキーマは `user_version = 1` のまま。Rust 版が追加する `img2webp_meta` テーブルは Python 版からは無視され、DB を開ける。Python 版はこのテーブルを更新しないため、次に Rust 版を起動すると「前回正常終了でない」とみなして掃除を実行する（件数が変わっていれば記録を信用しない）。
- Rust 版の変換記録（`pipeline: 3`）は Python 版ではシグネチャ不一致として扱われ、再変換対象になる。コピー記録はバイト一致のため再利用される。

## 5. リリース確認項目（ビルド担当者向け）

- `cargo build --release --no-default-features --locked` でビルドした。
- `[profile.release] panic = "unwind"` のまま。
- 配布 exe を空のフォルダで実行し、`config.ini` を置いた状態で `.img2webp.sqlite3` が作られ、出力先が `config.ini` の `output_dir` どおりで、開発ビルドの起動行が出ないことを確認した。
- `objdump -p img2webp.exe` の依存が Windows のシステム DLL のみ（VCRUNTIME140.dll を含まない）。
- 生成した出力ファイルの ACL が、同じフォルダに通常作成したファイルと同じ（ディレクトリ継承）であることを `icacls` で確認した。
