# Job Manage

Rust 製の小さな JOB 実行基盤です。サーバーは管理画面・REST API・割り振りを担当し、外向きに接続する Linux Docker ワーカーで Node.js / Python スクリプトを実行します。

- タグの完全一致による AND 条件で、実行可能な古い JOB から割り振ります。
- ワーカー 1 台の実行枠は 1 個です。対応するワーカーがいない JOB は後続を妨げません。
- **実行中の接続が切れた JOB は失敗になり、自動再試行しません。** 再接続後も失敗を維持します。
- サーバー再起動時も、割り当て済み・実行中の JOB を失敗にします。待機 JOB は保持します。
- JOB 定義を変更しても、投入済み JOB の定義リビジョンは変わりません。
- 制御・ログは 1 本の WebSocket、入力ファイル・成果物は別の HTTP(S) 通信で転送します。
- 他アプリケーションは同一クラスタの ClusterIP Service に API トークンでアクセスできます。
- 管理画面にログイン機能はありません。外部公開時は認証済みプロキシが共有ヘッダーを付けます。

## 構成

| 部品 | 実装 |
| --- | --- |
| サーバー | Rust / Axum / SQLx、管理画面を同梱 |
| ワーカー | Rust、Node/Python を子プロセスとして実行 |
| 管理画面 | Bun / Vite / TypeScript / Web Components / Oxfmt |
| 状態保存 | PostgreSQL または YugabyteDB YSQL |
| ファイル | S3 互換ストレージ（RustFS）、またはローカルディレクトリ |
| 配布 | `Dockerfile.server`、`Dockerfile.worker`、Compose、Kubernetes manifests |

NATS は使用しません。DB を状態の正本とし、単一サーバーで接続・割り振りを管理します。サーバーは **replicas=1 / Recreate** で運用してください。

## ローカルで起動

Docker / Docker Compose が利用可能な Linux 環境で実行します。

```sh
cp .env.example .env
# .env の JOB_API_TOKEN と JOB_WORKER_TOKEN を、それぞれ32文字以上のランダム文字列へ変更
docker compose pull
docker compose up -d --no-build
```

管理画面: `http://127.0.0.1:8080/jobs/`

Compose は簡易開発用 PostgreSQL と CPU ワーカーを含み、サーバーを **127.0.0.1 のみに公開**します。`JOB_AUTH_MODE=none` は、このローカル環境でブラウザーの認証を省略する明示的な設定です。LAN やインターネット向けに公開する構成では `proxy` に変更し、前段認証を設定してください。

```sh
docker compose logs -f server worker
docker compose down
```

`down` はデータを保持します。通常の運用で `down -v` を実行しないでください。

`main` ブランチへの変更は、検証後に `ghcr.io/uiui611/job-manage-server` と `ghcr.io/uiui611/job-manage-worker` の両イメージを自動ビルドします。Compose と Kubernetes の例は更新される `:main` タグを使用します。リリース内容を固定したい場合は、同じビルドの `:sha-<commit>` タグを両方に指定してください。

ソースからローカルビルドする場合は、公開イメージとタグを分けます。

```sh
JOB_IMAGE_TAG=local docker compose build
JOB_IMAGE_TAG=local docker compose up -d --no-build --pull never
```

## JOB の実行契約

各 JOB に専用の作業ディレクトリを作ります。任意のコマンド文字列ではなく、Node/Python のスクリプト本文と JSON 入力を登録します。

| 環境変数 | 意味 |
| --- | --- |
| `JOB_INPUT_FILE` | JSON 入力ファイル |
| `JOB_RESULT_FILE` | 構造化結果の出力先。必要に応じて JSON を書く |
| `JOB_INPUT_DIR` | 添付入力ファイルのディレクトリ |
| `JOB_ARTIFACT_DIR` | 保存したい成果物の出力ディレクトリ |

標準出力・標準エラーはログになります。実行結果は終了コード、結果 JSON、成果物アップロードの成否から確定します。ファイル名は英数字・`_`・`-`・`.` のみ、最大180文字で、階層はありません。1ファイル64MiB、結果 JSON 1MiB、保存ログ2MiBを上限とします。

Node の例:

```js
import fs from 'node:fs';
const input = JSON.parse(fs.readFileSync(process.env.JOB_INPUT_FILE, 'utf8'));
console.log('計算します');
fs.writeFileSync(process.env.JOB_RESULT_FILE, JSON.stringify({ sum: input.a + input.b }));
```

Python の例:

```python
import json, os
from pathlib import Path

data = json.loads(Path(os.environ['JOB_INPUT_FILE']).read_text())
Path(os.environ['JOB_RESULT_FILE']).write_text(json.dumps({'sum': data['a'] + data['b']}))
Path(os.environ['JOB_ARTIFACT_DIR'], 'report.txt').write_text('処理完了', encoding='utf-8')
```

入力仕様に例えば次の JSON Schema を指定します。

```json
{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"],"additionalProperties":false}
```

## 同一クラスタのアプリから利用

サンプル namespace を使う場合は `http://job-server.job-manage.svc.cluster.local:8080/jobs/api/v1` に Bearer トークンを送ります。前段のユーザー認証プロキシを経由する必要はありません。

```sh
curl -H "Authorization: Bearer $JOB_API_TOKEN" \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: my-request-001' \
  -d '{"definition_id":"DEFINITION_ID","input":{"a":2,"b":3}}' \
  http://job-server.job-manage.svc.cluster.local:8080/jobs/api/v1/runs
```

返された `id` を使い `GET /runs/{id}` をポーリングします。終端状態は `succeeded` / `failed` / `canceled` です。API とファイル転送の詳細は [docs/api.md](docs/api.md) を参照してください。

## デプロイと運用

- [Kubernetes 導入手順](docs/deployment.md)
- [設計と制約](docs/design.md)
- [REST API](docs/api.md)
- [OpenAPI 定義](docs/openapi.json)（実行中サーバーでは `/jobs/api/v1/openapi.json`）
- [検証手順と確認範囲](docs/verification.md)

GPU 利用時は、ワーカーの派生イメージにモデル実行に必要な依存パッケージを固定し、必要な GPU デバイスだけをコンテナへ渡します。基本イメージに GPU 推論ライブラリは含みません。GPU の機種や実行環境の動作確認を済ませてから、その環境を表すタグを付けてください。

既に利用できている CUDA / ROCm 対応 Python イメージがある場合は、ワーカーバイナリだけを持ち込めます。以下は構成例であり、GPU 実機で検証したイメージではありません。

```dockerfile
ARG GPU_BASE_IMAGE
FROM ghcr.io/uiui611/job-manage-worker:main AS agent
FROM ${GPU_BASE_IMAGE}
USER root
COPY --from=agent /usr/local/bin/job-worker /usr/local/bin/job-worker
RUN mkdir -p /work && chown 10001:10001 /work
ENV JOB_WORK_DIR=/work JOB_PYTHON_PATH=python3 PYTHONUNBUFFERED=1
WORKDIR /work
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/job-worker"]
```

`GPU_BASE_IMAGE` に、実機で確認した Linux amd64・glibc 2.36 以降を含む GPU イメージを指定してください。Python の場所が違う場合は `JOB_PYTHON_PATH` を合わせ、CA 証明書も含めます。Node がなければ Python ドライバだけを申告します。デバイスへのアクセス権や推論モデルの置き場所はそのイメージに合わせて設定してください。Docker socket を渡す必要はありません。

Node のスクリプトは JOB 専用ディレクトリの `.mjs` として実行されるため、追加依存を npm の global 領域に入れるだけでは ESM import を解決できません。派生イメージで `USER root` に切り替え、固定済み `package.json` と `package-lock.json` を `/` に配置して `RUN npm ci --prefix / --omit=dev` を実行すると、探索対象の `/node_modules` に配置できます。インストール後は `USER 10001:10001` に戻してください。Python の仮想環境は `JOB_PYTHON_PATH=/opt/venv/bin/python` のように指定できます。

Windows の直接実行・Windows Sandbox 対応は将来の範囲です。Windows の WSL やコンテナは前提にしていません。

## 開発・検証

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cd web
bun install --frozen-lockfile
bun run format:check
bun run build
```

接続できる試験環境と `env=base` タグの実ワーカーを用意して、以下を実行します。

```sh
export JOB_TEST_URL=http://127.0.0.1:8080/jobs
export JOB_TEST_API_TOKEN=...
export JOB_TEST_WORKER_TOKEN=...
python3 tests/smoke.py
```

認証検証のため、smoke test は `JOB_AUTH_MODE=proxy` の試験環境向けです。定義・実行履歴・入力ファイルを作るので、開発専用 DB / バケットで実行してください。API、Node/Python 実行、ファイル転送、タグ照合、キャンセル、定義固定、切断時失敗、古い結果の拒否、バージョン不一致を検証します。

同梱 Kubernetes の例では `python3 tests/run_cluster_smoke.py --namespace job-manage` で、Secret の値を表示せずにクラスタ内で実行できます。`--node <node-name>` は試験先ノードを明示したい場合だけ指定します。独立した Docker 環境での検証と、試験の前提・確認範囲は [検証手順](docs/verification.md) を参照してください。

## ライセンス

[MIT License](LICENSE) で公開しています。
