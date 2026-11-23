# loudnorm 開発ガイド

フォルダ（サブフォルダ含む）内の楽曲のラウドネス（EBU R128）を測り、目標から外れた曲だけを補正する Rust + egui 製の GUI ツール。主な利用環境は Windows。UI の文言は日本語。

## 構成

| ファイル | 役割 |
|---|---|
| `src/main.rs` | 起動、ウィンドウ設定、`--cli`（テスト・スクリプト用） |
| `src/app.rs` | egui の画面（メニューバー、環境設定・バージョン情報のダイアログ、設定欄、グラフ、一覧、確認ダイアログ） |
| `src/core.rs` | 走査、ラウドネス解析（symphonia + ebur128、失敗時 ffmpeg）、統計と補正計画 |
| `src/apply.rs` | 補正の適用（ffmpeg で再エンコード、タグの引き継ぎ、バックアップ） |
| `src/lossless.rs` | 無劣化の補正（MP3 global_gain、Opus ヘッダゲイン）と MP3 のタグ位置の判定 |
| `src/ffmpeg.rs` | ffmpeg / ffprobe の検出と実行 |
| `src/settings.rs` | 設定ファイル（INI）の読み書き |
| `src/update.rs` | 更新の確認（GitHub Releases の最新版の取得と版の比較） |
| `build.rs` | Windows で exe にアイコン（`assets/icon.ico`）を埋め込む |
| `assets/` | アイコン（SVG 原画、.ico / .icns / .png） |
| `tools/make_icons/` | アイコンを SVG から作り直す Rust 製ツール（resvg）。本体のビルドには含まれず、独自の `Cargo.toml` / `Cargo.lock` を持つ |
| `packaging/` | 配布物の素材（Linux の `.desktop` と AppImage を作る `make-appimage.sh`、macOS の `Info.plist` の雛形） |

## バージョンとリリース

- 変更のたびに `Cargo.toml` の `version` を上げ、`Cargo.lock` も更新する（CI は `--locked` でビルドする）。
  - バグ修正などの軽微な変更: パッチ（3 桁目）
  - 機能追加: マイナー（2 桁目）
  - UI の根本的な変更、計算アルゴリズムの一新などの大きな変更: メジャー（1 桁目）
  - CI やドキュメントだけの変更では上げなくてよい。
- `main` に push され、その版のリリースがまだなければ `release.yml` が全プラットフォームをビルドして `v<version>` を Releases に登録する。
- 配布対象は最低限 Windows x64 と Linux x64。加えて Windows ARM64、Linux ARM64、macOS（Intel / Apple シリコン）も出す。Linux は cargo-zigbuild で glibc 2.17 を下限にリンクし、ランナーの Ubuntu の版に依存させない（CI で必要な glibc の版を検査する）。
- 配布形式は OS ごとに決まっている（ファイル名は `loudnorm-v<version>-<target>.<拡張子>`）。
  - Windows: exe と README の zip。
  - macOS: `loudnorm.app`（`packaging/macos/Info.plist` の雛形に版と最低 OS 版を埋め、`assets/icon.icns` を入れる）と README を `ditto` で zip にしたもの。Developer ID の署名・公証はしない（ad-hoc 署名のみ）。
  - Linux: AppImage（`packaging/linux/make-appimage.sh`）。AppDir は仕様どおり `AppRun`・`.desktop`・アイコン・`.DirIcon`（アイコンへのシンボリックリンク）を持つ。
    - appimagetool と AppImage のランタイムは版を固定し、ダウンロードの sha256 を `build.yml` のマトリクスに書いて検証する。上げるときは両方更新する。
    - ランタイムは静的リンクで、glibc の下限を上げず、libfuse2 も要らない。
- 作業はブランチで行う。`main` 以外への push でも `ci.yml` が同じビルドとテストを全プラットフォームで実行する。
- プルリクエストは、変更内容を表す適切なタイトルだけを付け、本文は空のまま作成する。

## 実装方針

### 補正
- できるだけ多くのファイルに手を加えない。目標から許容幅（既定 ±1.0 dB）に収まる曲は変更しない。
- 目標は中央値（既定）、平均値、トリム平均、変更が最少になる値、カスタムから選ぶ。グラフのクリックや数値入力でカスタムにできる。
- 調整方法は既定が再エンコード。ほかに無劣化優先（MP3 global_gain 1.5 dB 単位、Opus ヘッダゲイン）と ReplayGain タグのみ。
- MP3 の再エンコードは LAME で CBR 256 kbps 以上か同等の VBR（V0）。FLAC / ALAC / WAV は元のビット深度のまま。
- 音量を上げるときは真のピークが上限（既定 −1.0 dBTP）を超えないよう抑える。
- 解析だけではファイルを変えない。適用は確認ダイアログを経てから。上書き時は元ファイルを `_loudnorm_backup` に同じ階層で移す。更新日時は既定で元のまま保つ。

### タグ
- 元ファイルのタグ（歌詞、ジャケット画像、独自フレームを含む）は必ず引き継ぐ。
  - MP3 は ID3v2 / APEv2 / ID3v1 をバイト単位でそのまま移す。
  - ほかの形式は lofty の具体型（FlacFile、Mp4File、VorbisFile、WavFile）で読み書きし、汎用 Tag 経由の変換で情報を落とさない。

### UI
- exe（`build.rs` が `assets/icon.ico` を Windows リソースとして埋め込む）とウィンドウ（`assets/icon-256.png`）には、同じ `assets/` のアイコンを使う。
  - 原画は `icon.svg`（棒 4 本）と `icon-small.svg`（棒 3 本。16 px でも棒を見分けられるよう減らした）。
  - SVG を直したら、リポジトリのルートで `cargo run --release --locked --manifest-path tools/make_icons/Cargo.toml` を実行し、`icon.ico`・`icon.icns`・`icon-256.png` を作り直す。
    - 描画は resvg（ブラウザや Node.js は要らない）。同じサイズと SVG の画像は 1 度だけ描いて共有する。同じ CPU アーキテクチャと同じ `Cargo.lock` なら同じバイト列になる（コミット済みの資産は x86_64 で作ったもの）。
    - 16・20・24・32 px は `icon-small.svg`、40・48・64・256 px と `icon-256.png` は `icon.svg` から作る（20・40・64 px は Windows の拡大表示用）。
    - `.ico` の各エントリは PNG で、256 px のエントリは `icon-256.png` と同一にする。
    - `.icns`（macOS の `.app` 用）は PNG のエントリ `ic07`〜`ic14`（32〜1024 px）を持ち、SVG の選び方は `.ico` と同じ。`ic08`・`ic13`（256 px）は `icon-256.png` と同一にする。
  - Windows のタイトルバー・タスクバー・Alt+Tab には eframe が `icon-256.png`（棒 4 本）を縮小して使い、エクスプローラーの 16 px には `.ico` の小サイズ（棒 3 本）が使われる。
    - この差は許容する。
  - Wayland ではウィンドウのアイコンを `with_icon` でなく、app_id（`loudnorm`）に一致する `.desktop` ファイルから決める。
    - 配布物に同梱する `loudnorm.desktop`（元は `packaging/linux/loudnorm.desktop`）には `StartupWMClass=loudnorm` を書く。
- 日本語 UI フォントを 1 つだけ選んで最優先にする（Yu Gothic UI → Meiryo UI → MS UI Gothic の順）。英字用の既定フォントと混在させるとベースラインがずれる。
- 一覧はフォルダとファイル名を別の列にし、長い名前は中間を省略する（`abc…xyz`）。ツールチップにはその列の省略前の値を出す。
- 設定欄が長くなっても適用ボタンは常に見える位置に置く。
- 画面の設定値（ffmpeg の場所、方法、許容幅、目標、ピーク上限、再エンコード設定、並列数、出力、更新日時の保持、ウィンドウサイズ、列幅、グラフの表示など）は `settings.ini` に保存し、次回起動時に読み込む。新しい設定を足すときも同様にする。
  - 場所は Windows が `%APPDATA%\loudnorm\settings.ini`、Linux / macOS が `$XDG_CONFIG_HOME/loudnorm/settings.ini`（未設定なら `~/.config/loudnorm/settings.ini`）。exe と同じフォルダに `settings.ini` があればそちらを優先する。
    - AppImage では隣の `settings.ini` は読まない（他のアプリと同じフォルダに置かれやすく、別のアプリの設定を上書きしうるため）。
      - 持ち運び用は AppImage 標準の `--appimage-portable-config`（`XDG_CONFIG_HOME` を `<AppImage>.config` に向ける）に任せる。
    - macOS では exe のフォルダ（`.app` の中）の `settings.ini` を見ない（署名済みのバンドルにファイルを足すと署名が壊れ、隔離されたまま起動したときは `.app` の隣も見えないため）。
- ffmpeg の場所は「設定」→「環境設定…」のダイアログで、フォルダ選択とファイル選択の両方で指定できるようにする。
  - ffmpeg の探索では、`APPDIR` の下で動いているときだけ、`APPIMAGE` の親フォルダを exe のフォルダとして扱う（`ffmpeg::exe_dir`）。
  - macOS では exe のフォルダを探さず、PATH の後に Homebrew の 2 か所（`/opt/homebrew/bin`、`/usr/local/bin`）を探す（Finder から起動した GUI アプリの PATH に Homebrew の場所は入らないため）。

### 処理
- 解析と適用は曲単位で並列に実行する。並列数の既定は論理コア数 − 1（最低 1）。
- GPU はオーディオのエンコード・デコードに使えない（NVENC / NVDEC は映像専用）。

### Windows
- 起動時にコンソールを開かない（`windows_subsystem = "windows"`）。`--cli` のときだけ親プロセスのコンソールに接続する。
- 子プロセス（ffmpeg）は `CREATE_NO_WINDOW` で起動する。
- deprecated な API は使わない（例: filetime ではなく std の `File::set_times`）。

## 確認

- `cargo test` で統計・設定まわりのユニットテストと、アイコン資産（`.ico` と `.icns` の各サイズ、256 px のエントリと `icon-256.png` の一致）の検査を実行する。
- 補正の結果は `--cli <フォルダ> [--apply]` で確かめられる（ラウドネス、タグ、サンプル数の比較）。
- Windows 固有のコード（フォント、ダイアログ、コンソール、exe へのアイコン埋め込み、タイトルバー・タスクバーのアイコン）は Linux では確認できないため、その旨を明記する。
- macOS の `.app` の組み立て（`otool`・`plutil`・`ditto`）は Linux では確認できず、CI で確かめる。x86_64 の AppImage は Linux で `make-appimage.sh` を実行して確かめられる（環境変数 `APPIMAGE_ARCH=x86_64` と、`build.yml` で固定している版の appimagetool・ランタイムが要る）。
