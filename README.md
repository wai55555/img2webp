# img2webp

`inputs/` 以下の PNG・GIF を WebP に変換します。初期設定では JPEG・AVIF・WebP を再圧縮せずに `outputs/` へコピーします。JPEG は設定で WebP 変換に切り替えられます。フォルダ構造と元ファイルの日時を引き継ぎます。

## 準備

Python 3.10 以降を用意し、プロジェクト直下で依存ライブラリをインストールします。

```shell
python -m pip install -r requirements.txt
```

初回起動時に `inputs/` と `outputs/` を自動作成します。`inputs/` にファイルを置き、Windows では `img2webp.bat` を実行します。コマンドラインからは次のように実行できます。

```shell
python img2webp.py
```

設定ファイル、入力、出力は、実行時の作業フォルダに関係なく `img2webp.py` のあるフォルダを基準にします。失敗したファイルがある場合は終了コード `1`、設定に問題がある場合は `2` を返します。

## 対象ファイル

| 拡張子 | 動作 |
| --- | --- |
| `.png`（APNG を含む）、`.gif` | 品質設定に従って WebP に変換 |
| `.jpg`、`.jpeg` | `copy.jpeg = true` なら元の拡張子のままコピー、`false` なら WebP に変換 |
| `.avif`、`.webp` | 元のバイト列をコピー |
| その他 | `copy.other_files = true` の場合のみコピー |

拡張子は大文字・小文字を区別しません。変換対象は実際の画像形式も確認します。JPEG・AVIF・WebP のコピーでは画像を開かないため、ファイルの破損は検出しません。シンボリックリンクは処理しません。

通常、`photo.png` は `photo.webp`、`photo.jpg` は `photo.jpg` になります。`copy.jpeg = false` にして同じフォルダの `photo.png` と `photo.jpg` を変換する場合は、`photo.png.webp` と `photo.jpg.webp` に分けます。`photo.webp` も入力にあれば、コピーした `photo.webp` が元の名前を使います。入力の組み合わせや `copy.jpeg` を後から変えても、過去の出力は自動削除しません。

## 設定

プロジェクト直下の [`config.ini`](config.ini) を編集します。現在の設定値はこのファイルを確認してください。設定項目の欠落や不正な値は処理前にエラーになります。

| 項目 | 説明 |
| --- | --- |
| `paths.input_dir` | 入力フォルダ |
| `paths.output_dir` | 出力フォルダ |
| `webp.quality` | WebP の品質、0～100 |
| `animation.missing_duration_ms` | フレーム表示時間がない場合の補完値（ミリ秒） |
| `copy.jpeg` | `true` なら `.jpg` と `.jpeg` を元の拡張子のままコピー。`false` なら WebP に変換 |
| `copy.avif` | AVIF をコピー |
| `copy.webp` | WebP をコピー |
| `copy.other_files` | WebM などその他のファイルもコピー |
| `output.overwrite_existing` | `true` なら履歴と一致しない既存の出力を更新。`false` なら既存の出力を上書きしない |
| `output.reuse_unchanged` | `true` ならSQLiteの成功記録と一致する出力を再利用 |
| `output.adopt_untracked` | `true` なら履歴のない既存出力を現在の設定として登録する |
| `output.db_commit_every` | SQLiteの記録を何件ごとに確定するか。初期設定は32件 |
| `preserve.metadata` | 変換時に Exif・XMP・ICC と生成情報を引き継ぐ |
| `preserve.timestamps` | 変換・コピー時に元ファイルの日時を引き継ぐ |

相対パスはプロジェクト直下を基準にします。入力と出力を同じフォルダや親子のフォルダには設定できません。`preserve.metadata = false` は変換時にだけ適用されます。JPEG・AVIF・WebP・その他ファイルのコピーは内容を変更しないため、元の埋め込みメタデータもそのまま残ります。

処理履歴は出力フォルダの `.img2webp.sqlite3` に、ファイルごとに成功後だけ記録します。入力のサイズ・更新日時・作成/変更日時、出力のサイズ・更新日時、出力先、変換設定を比較し、すべて一致したファイルをスキップします。CRCやファイル全体のハッシュ計算は行いません。同じ設定での途中再開は再実行するだけです。品質を変更した場合は変換対象だけ作り直し、JPEG・AVIF・WebPなどのコピーには品質変更を反映しません。入力や出力が同じサイズ・日時のまま書き換えられた場合は検出できません。

SQLiteへの書き込みはまとめて確定します。通常の停止時は最後の記録まで確定します。プロセスの強制終了では、未確定分を次回作り直す場合があり、`db_commit_every = 32` なら最大31件です。1件ごとの確定を優先する場合は `1` にします。DBはSQLiteのWALモードと `synchronous=NORMAL` を使用します。停電では確定済みの直近の記録も失われる可能性がありますが、出力画像は残り、次回必要な分だけ処理し直します。

`overwrite_existing = true` は履歴がない、または履歴と一致しない既存出力の更新を許可します。`reuse_unchanged = false` にすると、履歴が一致しても作り直します。`overwrite_existing = false` は既存ファイルの上書きを禁止するため、入力や品質が変わっても古い出力が残ります。

以前のバージョンで作った出力には履歴がありません。初回は安全のため作り直します。既存出力を現在の品質・設定で作ったものとして**自分で確認済み**の場合だけ、`adopt_untracked = true` にして一度実行すると、ファイルを作り直さず履歴へ登録できます。内容や品質は検査しないため、登録後は `false` に戻してください。処理に失敗したファイルは記録されず、次回の実行で再試行されます。

## メタデータとアニメーション

PNG の `parameters` は WebP の Exif `UserComment` に元の文面で格納します。その他の PNG テキスト、GIF のコメント、WebP 変換を選んだ JPEG のコメントも保持を試みます。元画像に `UserComment` がある場合はその内容を優先し、追加テキストは `XPComment` に格納します。重要な生成情報を読み戻せなかったファイルは失敗として扱い、既存の出力を残します。

PNG の RGB またはアルファチャンネルに埋め込まれた Stealth PNG Info も抽出し、テキストとして Exif に保存します。非圧縮と gzip／zlib 圧縮に対応します。16 ビット PNG は非インターレースの RGB・RGBA・グレースケール＋アルファに対応します。

ステルス情報の走査は画像の各辺 16,384 ピクセル以内、画素データ約 512 MiB 以内、情報の展開前後それぞれ 8 MiB 以内に制限します。制限を超えた情報や壊れた情報は抽出されません。

WebP 変換時は Exif の向きを画素に反映します。CMYK の JPEG に ICC プロファイルがある場合は sRGB へ変換します。GIF と APNG は各フレームの見た目、表示時間、ループ回数を引き継ぎます。変換後の画素値・ファイルサイズ・フレーム数が完全一致することは保証しません。Automatic1111 と NovelAI での読み戻しは、利用する実画像とアプリのバージョンで確認してください。

日時は、Windows では作成・アクセス・更新日時、それ以外ではアクセス・更新日時を引き継ぎます。`preserve.timestamps = false` にすると出力時の日時になります。ファイルシステムごとの日時精度には差があります。

## Rust 版（img2webp.exe）

同じ処理を Rust で実装した版を `rust/` に置いています。単一 exe で動作し、設定・出力命名・状態 DB・メッセージ形式は Python 版と互換です。主対象は Windows です。

```shell
cd rust
cargo build --release --no-default-features --locked
```

- `--no-default-features` は必須です。既定の feature `dev-db` は開発用の状態 DB 名と出力先（`outputs-dev`）に切り替えるため、付けたままビルドした exe は実 DB を使いません。
- `--locked` は `Cargo.lock` のとおりにビルドします（リリースでは必須）。
- `img2webp.bat` は exe があればそれを起動し、無ければ Python 版を起動します。

設定は現行の `config.ini` がそのまま動きます。次の任意項目を追加できます（省略時は現行と同じ挙動）。

| 項目 | 説明 |
| --- | --- |
| `webp.lossless` | `true` で lossless 圧縮（既定 `false`） |
| `webp.method` | libwebp の method、0〜6（省略時は静止画 4／アニメーション 0） |
| `webp.exact` | 透明部分の RGB を保持（既定 `false`、静止画のみ） |
| `output.parallel_jobs` | 同時に処理するジョブ数（省略時は CPU コア数、超過はコア数にクランプ） |
| `preserve.stealth` | Stealth PNG Info を読み取る（既定 `true`。`false` で無効） |
| `preserve.stealth_max_pixels` | これを超える画素数の 16bit PNG は Stealth 走査を省略（既定 `16777216`、`0` で無制限） |

Python 版との違いは [`doc/img2webp-rust-implementation.md`](doc/img2webp-rust-implementation.md) §4〜5 にまとめています。主なものは次のとおりです。

- 出力ファイルの権限が通常の作成権限（umask 準拠）になる。
- 変換対象は移行時に一度だけ作り直される（コピーは状態 DB の記録を再利用）。
- 16bit APNG は未対応（ジョブ単位の失敗）。
- 一辺 16,384px 以上の画像は処理前に失敗する。
- メモリ予算を超えるジョブは失敗しうる。
- 標準出力・標準エラーは UTF-8。
- チャンク CRC が壊れた PNG は、Python 版（Pillow）と同じく CRC を計算し直して変換し、`WARN` を出します。

移行とロールバックの手順は [`doc/img2webp-rust-migration.md`](doc/img2webp-rust-migration.md)、設計と検証の記録は [`doc/img2webp-rust-design-spec.md`](doc/img2webp-rust-design-spec.md)・[`doc/img2webp-rust-phase0.md`](doc/img2webp-rust-phase0.md) を参照してください。

## ライセンス

[MIT License](LICENSE.md)。

## 謝辞

本プロジェクトの開発にあたり、[Takenoko3333/png2webp-for-a1111-and-NAI](https://github.com/Takenoko3333/png2webp-for-a1111-and-NAI) を参考にしました。元作者に感謝します。元プロジェクトのMITライセンス表示は [LICENSE.md](LICENSE.md) に含めています。
