# デプロイ

## イメージと自動更新

公開イメージは `ghcr.io/uiui611/job-manage-server` と `ghcr.io/uiui611/job-manage-worker` です。`main` ブランチの検証が成功すると、両方の Linux amd64 イメージを `:main` と `:sha-<full commit SHA>` で公開します。再現できる構成には同じコミットのタグを両方に指定してください。PR は検証だけを行い、イメージ公開やデプロイ通知はしません。

[公開ワークフロー](../.github/workflows/publish-image.yml) は両イメージの公開後、`job-manage-server` と `job-manage-worker` をそれぞれ一度だけ OIDC webhook に通知します。接続先と audience はリポジトリ変数 `DEPLOY_WEBHOOK_URL` と `DEPLOY_OIDC_AUDIENCE` で変更できます。認証には GitHub の短寿命 OIDC トークンを使い、デプロイ用の長命 Secret は保存しません。通知は再試行や rollout 完了待ちをしないため、通知結果と実際の起動状態を別途確認してください。

受信側にはサービス ID と Deployment の対応を事前に設定します。受信側は既存 Deployment の再起動を要求するだけで、namespace・manifests・Secret・DB・バケットを作り直しません。サンプルの `imagePullPolicy: Always` と `Recreate` は `:main` を再取得して単一サーバーを更新する設定です。固定タグを運用する場合は、管理している manifests 側のタグも更新してください。

ローカルでビルドする場合は、公開タグと区別します。

```sh
docker build -f Dockerfile.server -t ghcr.io/uiui611/job-manage-server:local .
docker build -f Dockerfile.worker -t ghcr.io/uiui611/job-manage-worker:local .
```

サーバーには管理画面を同梱します。ワーカー基本イメージは Node 24 と Python 3.11 を含み、どちらも UID/GID 10001 で動作します。Rust と管理画面の依存は lockfile に固定します。

## サーバー設定

| 環境変数 | 設定内容 |
| --- | --- |
| `JOB_DATABASE_URL` | 必須。PostgreSQL/YSQL 接続 URL |
| `JOB_API_TOKEN` | API の初期トークン。32文字以上 |
| `JOB_WORKER_TOKEN` | ワーカーの初期トークン。API と別の32文字以上 |
| `JOB_AUTH_MODE` | 既定 `proxy`。`none` は隔離したローカル開発だけ |
| `JOB_PROXY_TOKEN` | 認証済みプロキシの `X-Job-Proxy-Token` と一致する32文字以上 |
| `JOB_BASE_PATH` | 例 `/jobs`。空文字はルート。起動時変更可能 |
| `JOB_BIND` | 既定 `0.0.0.0:8080` |
| `JOB_WEB_DIR` | コンテナでは `/app/web` |
| `JOB_STORAGE_DIR` | ローカルファイル保存先。コンテナでは `/data` |
| `JOB_SIGNING_KEY` | ローカル転送 URL の署名鍵。永続的なランダム値を設定 |
| `JOB_HEARTBEAT_TIMEOUT_SECONDS` | 初期値30秒。5〜300秒 |
| `JOB_S3_ENDPOINT` | S3 互換ストレージの endpoint。未設定ならローカル保存 |
| `JOB_S3_PUBLIC_ENDPOINT` | 署名付き URL の endpoint。省略時は上記と同じ |
| `JOB_S3_BUCKET` | 事前作成済みバケット名 |
| `JOB_S3_REGION` | 例 `us-east-1` |
| `JOB_S3_ACCESS_KEY` / `JOB_S3_SECRET_KEY` | バケットへアクセスする資格情報 |

DB 接続先には例えば `postgres.example.invalid:5432`、S3 接続先には `https://rustfs.example.invalid` を実際の接続先に置き換えて指定します。接続文字列・トークン・署名鍵は Secret 管理機構から渡し、Git に保存しません。

S3 設定を使う場合は、アプリ専用の非公開バケットとアクセス資格情報を事前に作成します。署名付き URL の endpoint はワーカー・API 利用アプリ・成果物を開くブラウザーから到達可能にします。`JOB_S3_PUBLIC_ENDPOINT` には `https://storage.example.invalid` のようにホストのルートを指定し、`/s3` などの前置パスやバケット名を含めません。バケット名はアプリが URL に追加します。プロキシは署名対象の Host・パス・query をそのまま転送し、署名後に差し替えてはいけません。クラスタ内のワーカーもこの公開 endpoint を使用するため、DNS・HTTPS の到達性と証明書の信頼を確認してください。

ブラウザーから S3 に直接アップロードする場合は、バケットの CORS に管理画面の origin を許可します。S3 API の `PutBucketCors` に渡す例です。`AllowedOrigins` は実際の管理画面に限定してください。

```json
{
  "CORSRules": [{
    "AllowedOrigins": ["https://jobs.example.invalid"],
    "AllowedMethods": ["GET", "PUT", "HEAD"],
    "AllowedHeaders": ["content-type"],
    "ExposeHeaders": ["ETag", "Content-Length"],
    "MaxAgeSeconds": 3600
  }]
}
```

この設定はアプリから自動適用しません。API 利用アプリやワーカーの転送は、ブラウザーの CORS 制限を受けません。DB・S3・クラスタ内 API の通信保護は、利用するネットワークに合わせて設定します。外部ワーカーは HTTPS/WSS と証明書検証を標準とします。

## ワーカー設定

Windows で直接実行する場合は [Windows ワーカーの手順](windows-worker.md) を参照してください。設定・プロトコルは共通で、Node/Python を環境から提供します。

| 環境変数 | 設定内容 |
| --- | --- |
| `JOB_SERVER_URL` | ベースパスを含む例 `https://jobs.example.invalid/jobs` |
| `JOB_WORKER_TOKEN` | ワーカー専用トークン |
| `JOB_WORKER_ID` | 再起動しても変えない、環境ごとに固有の ID |
| `JOB_WORKER_NAME` | 表示名。省略時は ID |
| `JOB_WORKER_TAGS` | JSON 配列、例 `["os=linux","env=base"]` |
| `JOB_WORK_DIR` | コンテナでは `/work`。書き込み可能な領域 |
| `JOB_NODE_PATH` / `JOB_PYTHON_PATH` | 既定 `node` / `python3` |
| `JOB_ALLOW_INSECURE_HTTP` | 開発環境で HTTP 接続を使う場合のみ `true` |
| `JOB_CA_CERT_FILE` | 任意。HTTPS ファイル転送と WSS の両方で追加信頼する CA 証明書の PEM ファイル |
| `JOB_RUNTIME_ENV_ALLOWLIST` | スクリプトに渡す追加環境変数名をカンマ区切りで指定。`JOB_` 始まりは禁止 |

`JOB_ALLOW_INSECURE_HTTP` は TLS 証明書検証を無効にする設定ではありません。基本の PATH/HOME/TMPDIR/LANG と実行契約用変数のほか、GPU 用の `LD_LIBRARY_PATH`、`CUDA_VISIBLE_DEVICES`、`NVIDIA_VISIBLE_DEVICES`、`NVIDIA_DRIVER_CAPABILITIES`、`ROCR_VISIBLE_DEVICES`、`HIP_VISIBLE_DEVICES` を子プロセスへ渡せます。接続トークンや不要な親環境変数は渡しません。

プライベート CA を使う場合は、その公開証明書だけを read-only でマウントし、`JOB_CA_CERT_FILE=/etc/job-manage/ca.pem` を指定します。複数 CA を含む PEM bundle も使用できます。標準の信頼済み CA とホスト名検証は維持されます。ファイルは起動時に読み込み、読めない・証明書がない・不正な場合や1 MiBを超える場合は起動を中止します。証明書変更後はワーカーを再起動してください。CA の秘密鍵をマウントしたり、証明書をリポジトリへ追加したりする必要はありません。この設定は JOB の子プロセスに渡されず、スクリプト内で行う HTTPS 通信の信頼設定は別です。

Docker では `--mount type=bind,src=/secure/path/ca.pem,dst=/etc/job-manage/ca.pem,readonly` と `-e JOB_CA_CERT_FILE=/etc/job-manage/ca.pem` を追加します。Kubernetes では CA の公開証明書を ConfigMap などから read-only でマウントし、同じ環境変数を設定します。既定の非 root UID 10001 で読める権限にしてください。

この設定は接続先サーバーの証明書を信頼するためのもので、mTLS のクライアント証明書を設定するものではありません。

GPU ドライバや推論ライブラリはホスト/GPU ごとに選び、派生イメージへ固定してください。ワーカーの登録タグは、確認済みの実行環境を表すものにします。

## 別の Linux マシンでワーカーを起動

次の `worker.env` を Git の外に作ります。トークンには管理画面で発行した値、ID には他のワーカーと重複しない固定値を使います。Docker の env-file では JSON 配列全体をさらに引用符で囲まないでください。

```dotenv
JOB_SERVER_URL=https://jobs.example.invalid/jobs
JOB_WORKER_TOKEN=REPLACE_WITH_WORKER_TOKEN
JOB_WORKER_ID=compute-desktop-01
JOB_WORKER_NAME=Linux compute desktop
JOB_WORKER_TAGS=["os=linux","env=base"]
JOB_WORK_DIR=/work
```

```sh
chmod 600 worker.env
docker run -d --name job-worker --restart unless-stopped --init \
  --env-file ./worker.env \
  --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  --mount type=volume,src=job-worker-work,dst=/work \
  --tmpfs /tmp:rw,nosuid,noexec,size=256m,uid=10001,gid=10001,mode=1777 \
  --memory 8g --pids-limit 256 --cpus 4 \
  ghcr.io/uiui611/job-manage-worker:main
```

待ち受けポートや Docker socket のマウントは不要です。必要な通信はサーバーへの外向き HTTPS/WSS と、署名付きファイル URL への HTTPS です。メモリ・CPU・PID 数は実行内容に合わせて調整します。GPU 利用時は対応する派生イメージへ差し替え、必要なデバイスと補助グループを設定します。

`/work` の名前付き volume は再起動後も残ります。容量制限が必要なら専用ファイルシステムやクォータを設けます。強制停止後に残った `attempt-*` ディレクトリは、実行中 JOB がないことを確認してから整理します。

## Kubernetes

[`deploy/base`](../deploy/base) は namespace `job-manage` を使う設定例です。S3 endpoint は例示用の無効なホスト名なので、そのままでは動作しません。実際の接続先・イメージタグ・namespace・リソース制限は、公開リポジトリの外の overlay で管理してください。CPU ワーカーは任意の試験用で、GPU アクセス権を持ちません。

初回だけ次の準備を行います。

1. 対象の Kubernetes context と namespace を確認し、専用 namespace を作成する。
2. PostgreSQL/YSQL の専用 DB と利用者、S3 の非公開バケットと資格情報を作成する。
3. namespace 内に `job-manage-secrets` を作成する。既存のアプリ用 Secret がある場合は保持する。
4. サーバーの設定例を実環境に合わせ、必要に応じて CPU ワーカーを含める。
5. manifests を dry run で確認してから適用し、サーバーの ready とワーカー接続を確認する。

Secret のキーは `JOB_DATABASE_URL`、`JOB_API_TOKEN`、`JOB_WORKER_TOKEN`、`JOB_PROXY_TOKEN`、`JOB_SIGNING_KEY`、`JOB_S3_ACCESS_KEY`、`JOB_S3_SECRET_KEY` です。値は既存の Secret 管理機構、またはアクセス権を制限した Git 外の env ファイルから登録します。例えば初回作成は次の形式です。

```sh
kubectl config current-context
kubectl apply -f deploy/base/namespace.yaml
kubectl -n job-manage create secret generic job-manage-secrets \
  --from-env-file=/secure/path/job-manage.env
# endpoint などを設定済みの private overlay に対して実行
kubectl apply --dry-run=server -k /path/to/private-overlay
kubectl apply -k /path/to/private-overlay
kubectl -n job-manage rollout status deployment/job-server
```

アプリは起動時に専用 DB 内のテーブルを初期化します。既存アプリの DB は流用しません。通常のイメージ更新で DB・バケット・Secret を再作成したり、初期化をやり直したりする必要はありません。自動デプロイもこれらを変更しません。

S3 利用時のサーバー `/data` は一時領域です。ローカル保存モードで運用する場合は S3 設定を外し、`/data` を PVC に変更します。DB と、バケットまたはローカル成果物の両方をバックアップしてください。

## 前段プロキシ・サブパス

`JOB_BASE_PATH=/jobs` の場合、プロキシは `/jobs/...` を保持して転送します。[nginx 設定例](../deploy/proxy-nginx.conf.example) は TLS とユーザー認証を設定した server ブロックに追加する例です。クライアントが送った `X-Job-Proxy-Token` は必ず上書きしてください。

`/jobs/worker` はブラウザー向け Cookie 認証から除外し、サーバーのワーカー Bearer 認証へ委ねます。ローカル保存モードの `/jobs/api/v1/blobs/` も Cookie 認証から除外し、短寿命の署名検証へ委ねます。管理画面の署名付き PUT は Cookie を送らず、ワーカーもログイン Cookie を持ちません。除外するのはこれらのパスだけで、通常の API や画面は認証が必要です。例の `auth_request off` / `auth_basic off` は使用する認証方式に合わせて調整します。S3 モードの転送はストレージ側の署名検証で認証されます。

WebSocket の Upgrade 転送と、ハートビート間隔より長いタイムアウトを設定します。画面は相対アセットと起動時設定を使うため、同じイメージでベースパスを変更できます。`/jobs` は `/jobs/` へ転送し、画面内の遷移はハッシュ方式です。ヘルスチェックの `/healthz` と `/readyz` はベースパス外です。

## 更新・終了

サーバーとワーカーのアプリバージョンは一致が必要です。更新前にスケジューラまたは各ワーカーの受付を停止して新規割り当てを止め、実行中 JOB の完了を待ちます。その後サーバーとワーカーを更新し、受付を再開してください。

自動デプロイ通知は JOB の完了を待たずに再起動を要求します。実行中に更新するとその JOB は失敗するため、必要な待機操作は main への反映前に行います。失敗した JOB は自動的にはやり直さず、必要なものだけ管理画面/API から再実行します。待機 JOB、定義、トークンは外部 DB に保持します。

リソースを廃止する際は、履歴や成果物が不要であることを確認してください。namespace の削除だけでは外部 DB や S3 バケットは消えません。共有している DB サーバーやストレージ基盤を削除対象に含めないでください。
