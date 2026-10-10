# Windows ワーカー

Windows 10/11 x64 上で `job-worker.exe` を直接実行します。Windows Sandbox 内でも同じ実行ファイルと設定を使います。Node.js・Python・GPU 用ライブラリは実行環境から提供してください。ワーカーの配布物にはランタイムを同梱しません。

## 実行ファイルの取得

GitHub Actions の `Check and package Windows worker` が成功すると、`job-worker-windows-x64` artifact に実行ファイル、ライセンス、この手順と SHA-256 が保存されます。PR の artifact はその PR の試験用ビルドです。運用時は接続先サーバーと同じアプリバージョンのビルドを使ってください。

ソースからビルドする場合は、Rust の Windows MSVC ツールチェーンと Visual Studio C++ Build Tools が必要です。

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build -p job-worker --release --locked
.\target\release\job-worker.exe --version
```

CI は CRT を静的リンクします。Node/Python は `.exe` 自身が動作する環境を別途用意してください。

## 起動

PowerShell で環境変数を設定してから起動します。トークンは管理画面で発行したワーカー用トークンに置き換え、設定や PowerShell の履歴を公開リポジトリへ追加しないでください。

```powershell
Get-Command node.exe, python.exe
$env:JOB_SERVER_URL = 'https://jobs.example.invalid/jobs'
$env:JOB_WORKER_TOKEN = 'REPLACE_WITH_WORKER_TOKEN'
$env:JOB_WORKER_ID = 'windows-compute-01'
$env:JOB_WORKER_NAME = 'Windows compute desktop'
$env:JOB_WORKER_TAGS = '["os=windows","env=base"]'
.\job-worker.exe
```

既定では PATH 上の `node` と `python` を使います。短い確認用スクリプトが正常に実行され、期待する出力が得られたランタイムだけを申告します。片方だけでも起動でき、両方使えない場合は接続を開始せず終了します。Microsoft Store の実行エイリアスで Python のインストール画面が開く場合や、複数の Python を使い分ける場合は、実際の実行ファイルを指定してください。

```powershell
$env:JOB_NODE_PATH = 'C:\Runtimes\node\node.exe'
$env:JOB_PYTHON_PATH = 'C:\Runtimes\python\python.exe'
$env:JOB_WORK_DIR = 'C:\JobWork'
```

作業フォルダーの既定値は `%LOCALAPPDATA%\JobManage\work` です。`LOCALAPPDATA` がない環境では一時フォルダーの下を使用します。空白・日本語を含む絶対パスも使用できます。Node のスクリプトは `.mjs`、Python は `.py` として保存し、シェルを介さず起動します。

Node/Python の追加パッケージも環境側で提供します。Python は必要なら仮想環境の `python.exe` を指定します。Node ESM の import は通常の親フォルダーの `node_modules` 探索に従うため、作業フォルダーの親に依存を置くなど、実行ファイルの global install と区別して設定してください。

## 証明書と環境変数

プライベート CA を使う場合は、CA の公開証明書を PEM 形式で配置します。

```powershell
$env:JOB_CA_CERT_FILE = 'C:\Certificates\job-ca.pem'
```

この CA は HTTPS ファイル転送と WSS の両方で追加信頼されます。Windows の証明書ストアへ入れただけではワーカーの追加 CA 設定になりません。標準 CA・ホスト名検証は有効なままです。CA の秘密鍵やブラウザー用クライアント証明書は不要です。

JOB に渡す Windows の `SystemRoot`、`SystemDrive`、`WINDIR`、`PATHEXT` と PATH を保持し、`USERPROFILE`・HOME と `TEMP`・`TMP`・`TMPDIR` を JOB 専用フォルダーへ設定します。GPU 用の既存許可変数も渡します。追加の `PYTHONPATH` などが必要なら `JOB_RUNTIME_ENV_ALLOWLIST=PYTHONPATH` を指定してください。`JOB_` で始まる変数は大文字・小文字を問わず追加許可できません。トークン・CA 設定は JOB に継承しません。

## 停止と Windows Sandbox

Ctrl+C でワーカーを終了します。キャンセル、タイムアウト、切断、サーバー無応答時には、Windows Job Object でランタイムと子孫プロセスを停止します。正常終了後の残存子プロセスも停止します。ワーカーが強制終了した場合も外側の Job Object がプロセスを終了させます。Job Object への登録ができない環境では起動または実行を失敗させます。JOB の自動再実行はありません。

Job Object はプロセスの寿命管理です。ホスト上で直接実行したスクリプトは、その Windows 利用者の権限で動作します。隔離が必要なら Windows Sandbox または専用 VM 内で起動してください。

Sandbox に実行ファイルと環境側で提供する Node/Python を渡す場合は、必要なフォルダーだけを read-only で共有し、作業フォルダーは Sandbox 内に作成します。ワーカートークンは Sandbox 内で設定します。CA は公開証明書だけを渡します。Sandbox を閉じると、その中の設定や作業データは消えます。ホストと Sandbox を同時に登録する場合は固有の `JOB_WORKER_ID` を使ってください。

GPU ドライバ・Python ライブラリ・モデルは環境側で準備し、動作確認した条件をタグに設定します。Sandbox の仮想 GPU で実行できる処理はホストの GPU やドライバに依存します。この実装の Windows プロセス試験と、AMD/NVIDIA の実機 GPU 試験は別です。

## 検証

Windows CI は Node/Python の実行と JSON 結果、日本語・空白パス、利用可能ランタイムの申告、正常終了時の子孫停止、キャンセル・タイムアウト・切断・無応答・強制終了時の子孫停止、HTTPS/WSS の CA 検証を行います。手元で試験する場合も Node/Python を提供してください。

```powershell
$env:JOB_TEST_PYTHON = (Get-Command python.exe).Source
cargo test -p job-protocol -p job-worker --locked
cargo clippy -p job-protocol -p job-worker --all-targets --locked -- -D warnings
```
