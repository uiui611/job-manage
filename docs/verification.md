# 検証手順と確認範囲

開発時には Linux amd64 サーバーと CPU ワーカーで、PostgreSQL のローカル保存構成および YugabyteDB YSQL / RustFS 構成を確認しています。以下は再実行手順です。試験用の DB・バケット・namespace を使い、実運用の履歴や資格情報と分けてください。

## ソースの検証

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cd web
bun install --frozen-lockfile
bun run format:check
bun run build
```

Rust のテストはプロトコル、サブパスのルート・アセット・リダイレクト、ワーカーのキャンセル・タイムアウト・切断・無応答時の子孫プロセス停止、ログの上限、結果ファイル・symlink 拒否、転送 URL の TLS 制約を対象にします。ワーカーの実プロセス試験は Linux で実行します。管理画面の build には TypeScript の型検査を含みます。

## 独立した Docker 試験

[`tests/local_smoke.py`](../tests/local_smoke.py) は専用ネットワーク上に PostgreSQL、ローカル保存モードのサーバー、CPU ワーカーを作成します。Docker と Python 3 が使える Linux ホストで、試験対象の両イメージを取得して実行します。

```sh
docker pull ghcr.io/uiui611/job-manage-server:main
docker pull ghcr.io/uiui611/job-manage-worker:main
python3 tests/local_smoke.py
```

ローカルビルドを試験する場合は、イメージを明示します。

```sh
python3 tests/local_smoke.py \
  --server-image ghcr.io/uiui611/job-manage-server:local \
  --worker-image ghcr.io/uiui611/job-manage-worker:local
```

試験対象は次の通りです。

| 対象 | 確認する内容 |
| --- | --- |
| 任意のサブパス | `/nested/jobs/` の画面、JS/CSS、config、末尾スラッシュへの転送 |
| 署名付き入力転送 | 4 MiB の PUT とアップロード完了。JSON API の 2 MiB 制限との干渉がないこと |
| 実 Python 処理 | 入力取得、4 MiB の成果物保存、JSON 結果取得 |
| 署名付き成果物取得 | GET と入力データのバイト一致 |
| 失敗 | 終了コード 7、1秒の実行期限超過が `failed` になること |
| サーバー再起動 | 実行中 JOB が失敗し、待機 JOB・入力 JSON・定義が DB に残ること |
| ワーカー再接続 | 失敗した JOB を再開しないこと |

トークンと署名鍵は試験ごとに生成し、出力しません。終了時はこの試験が作成したコンテナとネットワークだけを削除し、既存のリソースは変更しません。サーバーの公開先は localhost の一時ポートです。DB と作業領域は試験用で、永続 volume は作りません。

この試験は CPU・HTTP・ローカル保存が対象です。再起動検証では稼働を続ける DB の状態を確認し、tmpfs 上の成果物ファイルの永続化は対象にしません。

## API とクラスタ構成の試験

[`tests/smoke.py`](../tests/smoke.py) は `JOB_AUTH_MODE=proxy` の試験サーバーと、`env=base` タグの実ワーカーを使います。Node/Python、ログ、JSON Schema、冪等キー、手動再実行、タグ照合、キャンセル、入力・成果物転送、定義リビジョン固定、切断時失敗、再接続時の非再実行、古い結果と異なるバージョンの拒否を確認します。

```sh
export JOB_TEST_URL=https://jobs.example.invalid/jobs
export JOB_TEST_API_TOKEN=...
export JOB_TEST_WORKER_TOKEN=...
python3 tests/smoke.py
```

実際のトークンは Secret 管理機構などから環境変数へ渡し、履歴に値を残さないでください。試験では定義・実行履歴・入力ファイルを作成します。

Kubernetes のサンプルと同じ Service・Secret 名を使う場合は、Secret の値を表示せずにクラスタ内で実行できます。

```sh
kubectl config current-context
python3 tests/run_cluster_smoke.py --namespace job-manage
```

`--namespace` は専用の試験 namespace に変更できます。既存の Secret 名が異なる場合は `--secret-name <secret-name>` を指定します。接続先 Service の `job-server` は、指定した namespace 内で解決されます。特定ノードで試験する場合のみ `--node <node-name>` を追加します。`tests/cluster-job.yaml` のイメージもサーバーと同じビルドのタグに合わせてください。テスト Job は期限付きで削除されますが、試験の定義・履歴・入力ファイルと ConfigMap は残ります。再利用する試験環境では必要に応じて整理します。

## 管理画面の操作確認

実ブラウザーでは、定義作成・編集、JSON 入力、Node 実行、結果・ログ・成果物、タグ不一致の待機表示、キャンセル・再実行、検索・状態フィルター、トークンの一度だけの表示と失効、ワーカー受付とスケジューラの停止・再開を確認します。狭い画面での横方向のはみ出しと、ブラウザー実行時エラーも確認します。試験後は受付設定を復元し、試験用トークンを失効させます。

開発時の CPU 試験だけでは、GPU 実行、Windows 実行、外部 HTTPS endpoint と実証明書、ブラウザーから S3 への CORS 転送は確認できません。利用する GPU・ネットワーク・認証プロキシに合わせた追加検証が必要です。
