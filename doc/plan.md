# img2webp 実装計画

## 目的と配置

- `img2webp/` をプロジェクトのルートとし、サブプロジェクトは作らない。
- 画像と生成情報を WebP へ整理する。
- 元ファイルは変更せず、プロジェクト直下の `inputs/` から `outputs/` へ再帰的に処理する。
- `inputs/` と `outputs/` がない場合は初回起動時に作成する。
- 本計画書は `doc/plan.md` に置き、実行ファイルと設定ファイルはプロジェクト直下に置く。

## 対象形式と処理

| 入力形式 | 処理 | 出力拡張子 |
| --- | --- | --- |
| PNG (`.png`) | WebP に変換。APNG はアニメーションを保持 | `.webp` |
| JPEG (`.jpg`, `.jpeg`) | `copy.jpeg = true` なら元のままコピー。`false` なら WebP に変換 | `.jpg` / `.jpeg` または `.webp` |
| GIF (`.gif`) | WebP に変換。アニメーションは表示内容、表示時間、ループを保持 | `.webp` |
| AVIF (`.avif`) | バイナリをそのままコピー | `.avif` |
| WebP (`.webp`) | バイナリをそのままコピー | `.webp` |

拡張子の大文字・小文字は区別しない。変換対象は実際の画像形式も確認し、拡張子と内容が合わないファイルは失敗として報告する。コピー対象の JPEG・AVIF・WebP は画像を開かず、再エンコードもメタデータの書き換えも行わずにコピーする。その他のファイルをコピーするかどうかは `config.ini` で指定する。

## `config.ini` の設計

`config.ini` をプロジェクト直下に置き、変更可能な値はここで管理する。初期値をコード中に重複して持たず、ファイルがない場合や値が不正な場合は、処理開始前に項目名を示して終了する。相対パスはプロジェクト直下を基準に解決し、入力先と出力先が同一または親子関係になる設定は拒否する。

| セクション・項目 | 初期値 | 動作 |
| --- | --- | --- |
| `paths.input_dir` | `inputs` | 入力フォルダ |
| `paths.output_dir` | `outputs` | 出力フォルダ |
| `webp.quality` | `90` | PNG・APNG・GIF、および変換を選んだ JPEG を WebP にする際の品質 |
| `animation.missing_duration_ms` | `80` | GIF・APNGにフレーム表示時間がない場合の補完値 |
| `copy.jpeg` | `true` | `.jpg` と `.jpeg` を元の拡張子でコピーする。`false` なら WebP に変換する |
| `copy.avif` | `true` | AVIFを無変換でコピーする |
| `copy.webp` | `true` | WebPを無変換でコピーする |
| `copy.other_files` | `true` | 対象外のファイルも無変換でコピーする。`webm` など画像以外のファイルも含む |
| `output.overwrite_existing` | `true` | 履歴と一致しない既存出力の更新を許可する。`false` なら既存出力を上書きしない |
| `output.reuse_unchanged` | `true` | SQLiteの成功記録と一致するファイルを再利用する |
| `output.adopt_untracked` | `false` | 履歴のない既存出力を現在の設定として登録する。移行時のみ有効にする |
| `output.db_commit_every` | `32` | SQLiteの成功記録をまとめて確定する件数。`1` なら毎件確定 |
| `preserve.metadata` | `true` | 変換時に対応可能なメタデータを引き継ぐ |
| `preserve.timestamps` | `true` | 変換・コピーした全ファイルで更新日時・アクセス日時、Windowsでは作成日時を引き継ぐ |

設定ファイルの形は次のとおりとする。

```ini
[paths]
input_dir = inputs
output_dir = outputs

[webp]
quality = 90

[animation]
missing_duration_ms = 80

[copy]
jpeg = true
avif = true
webp = true
other_files = true

[output]
overwrite_existing = true
reuse_unchanged = true
adopt_untracked = false
db_commit_every = 32

[preserve]
metadata = true
timestamps = true
```

`copy.other_files` は表にない形式にだけ適用する。`copy.jpeg = false` は JPEG を WebP 変換に切り替える。`copy.avif` または `copy.webp` が `false` の場合、その形式はコピーせずにスキップする。設定した品質などの数値は使用するエンコーダーの許容範囲で検証する。今後、処理に調整可能な数値を追加する場合も `config.ini` に項目を設け、コード中のマジックナンバーを避ける。

`preserve.timestamps = true` を初期設定とし、JPEG・AVIF・WebP・その他ファイルのコピーも含めて元ファイルの日時を引き継ぐ。処理前に元ファイルの日時を取得し、出力の書き込みとメタデータ処理が終わってから適用する。`false` の場合は内容だけをコピーし、出力の日時は新規作成時の値を使う。

出力フォルダのSQLite DBに、入力のサイズ・更新日時・作成/変更日時、出力のサイズ・更新日時、出力先、変換設定を成功後に記録する。`reuse_unchanged = true` では、記録と現状が一致するファイルだけ再利用する。CRCやファイル全体のハッシュは計算しない。品質変更時は変換対象を再処理し、コピー対象は再利用する。履歴のない既存出力は初回に作り直すが、ユーザーが内容と設定を確認済みの場合は `adopt_untracked = true` で登録できる。DB更新は `db_commit_every` 件ごとにまとめ、通常終了時には残りも確定する。強制終了では直近の未確定分を再処理する可能性、および同じサイズ・日時での内容変更を検出できないことをREADMEに明記する。

## 共通の動作

1. 設定された入力フォルダ以下を再帰的に走査し、出力フォルダに相対ディレクトリ構造を再現する。
2. 変換時は `config.ini` の品質を使用する。透過画像は透過を維持する。
3. `preserve.metadata = true` のとき、変換元の Exif・XMP・ICC を WebP で扱える範囲で保持する。既存の Exif `UserComment` が空なら、PNG の `parameters` は内容をそのまま、その他のテキスト情報は `キー: 値` の形式で `UserComment` に格納する。既存の `UserComment` がある場合はその値を優先し、追加情報は別のメタデータ領域へ保存できるか確認する。JPEG・GIF のコメントも同様に扱う。Automatic1111・NovelAI の生成情報や既存の `UserComment` を出力から読み戻せない場合、そのファイルは失敗として既存の出力を残す。その他の保持できない情報は警告し、黙って捨てない。
4. Exif の向き情報がある画像は、画素の向きを正規化し、出力側に矛盾する向き情報を残さない。CMYK の JPEG など色空間が異なる画像は WebP 用の RGB または RGBA に変換し、元の ICC プロファイルと画素の色空間が食い違わないようにする。安全に変換できない場合は失敗として報告する。GIF・APNG は表示用に合成された各フレームを保存し、表示時間とループを引き継ぐ。ループ指定のない GIF は 1 回再生として扱う。APNG にアニメーションとは別の既定画像がある場合、その画像をアニメーションのフレームとして加えない。
5. `preserve.timestamps` の設定に従い、変換対象とコピー対象の両方で元ファイルの日時を扱う。コピー対象のファイル内容は設定に関係なく変更しない。
6. 衝突しない変換ファイルは参考コードどおり `sample.png` → `sample.webp` とする。同じ出力名に複数の入力が対応する場合だけ、変換側の名前に元拡張子を付ける（例: `sample.png.webp`、`sample.jpg.webp`）。JPEG・WebP のコピーは元の名前を優先する。それでも衝突する場合は連番で区別する。衝突判定は処理前に行い、設定変更前などに作られた不要な出力は自動削除しない。
7. 入力ファイルは読み取り専用で扱う。上書きが必要な出力は、同じ出力フォルダの一時ファイルを完成させてから置き換える。途中で失敗した場合は既存の出力を残す。
8. 個別ファイルの読み込み・変換・コピーに失敗しても残りのファイルを処理し、最後に変換・コピー・スキップ・失敗の件数を表示する。

## 作業手順

1. 入出力、メタデータ、日時処理を整理し、形式ごとの変換・コピー処理に分ける。
2. プロジェクト直下に `img2webp.py` と Windows 用 `img2webp.bat` を実装する。Windows 専用ライブラリは Windows 上でのみ読み込む。
3. `config.ini`、`requirements.txt`、`.gitignore`、MIT ライセンスの表示、導入方法と設定項目・制限を記した `README.md` を用意する。
4. PNG・APNG、JPEG、静止・アニメーション GIF、AVIF、WebP の処理を検証する。特に表示内容と透過、表示時間・ループ、Exif `UserComment` の競合と読み戻し、CMYK/ICC の色、変換・コピー双方の日時、同名衝突、上書き途中の失敗、破損ファイルと形式不一致、設定値の検証、`copy.other_files` による `webm` のコピーを確認する。
5. Automatic1111 と NovelAI の実画像が用意できる場合は、変換後に各アプリで生成情報を読み戻せるか確認する。

## 完了条件

- 上表の各形式が指定どおり変換またはコピーされ、入力ファイルは変更されない。
- `config.ini` の各項目が動作に反映され、設定ミスは処理前に分かる。
- `copy.other_files` を有効にすると `webm` などもコピーされ、無効ならスキップされる。
- 初期設定では変換・コピーした全ファイルに元の日時が引き継がれる。
- 出力のディレクトリ構造、ファイル名、日時、対象メタデータが仕様どおりになる。
- 一部のファイルが失敗しても処理が継続し、失敗内容を特定できる。
- Windows でバッチから起動でき、Python からも直接起動できる。
