# REST API

すべて `JOB_BASE_PATH + /api/v1` 配下です。別アプリケーションは `Authorization: Bearer <API token>` を付けます。管理画面は前段プロキシの認証を使い、更新操作には `X-Job-Request: 1` を付けます。入力・結果は JSON、スクリプトは文字列です。

機械可読な定義は [openapi.json](openapi.json) と、認証済みの `GET /openapi.json` で取得できます。

| メソッド | パス | 内容 |
| --- | --- | --- |
| GET / POST | `/definitions` | 定義一覧 / 新規定義 |
| PUT | `/definitions/{id}` | 定義の新リビジョンを保存 |
| GET / POST | `/runs` | 実行履歴 / 投入 |
| GET | `/runs/{id}` | 状態・結果・失敗理由 |
| POST | `/runs/{id}/cancel` | キャンセル要求 |
| POST | `/runs/{id}/retry` | 同じ定義リビジョンと入力で別 Run を明示的に作成 |
| GET | `/runs/{id}/logs` | `{text, truncated}` |
| GET | `/runs/{id}/artifacts` | 成果物一覧と一時ダウンロード URL |
| POST | `/artifacts/uploads` | 入力ファイルのアップロード URL を取得 |
| POST | `/artifacts/{id}/complete` | 入力ファイルのアップロード完了確認 |
| GET | `/workers` | ワーカー一覧・接続状態・タグ |
| PATCH | `/workers/{id}` | `{enabled: false}` で新規割り当てを停止 |
| GET / POST | `/tokens` | 発行済みトークンのメタデータ / 新規発行 |
| DELETE | `/tokens/{id}` | 発行済みトークンの失効 |
| GET / PATCH | `/settings` | 非機密設定の取得 / `{scheduler_paused:true}` で割り当て停止 |

## JOB 定義

`POST /definitions` / `PUT /definitions/{id}`:

```json
{
  "name": "sum",
  "description": "二つの数値を加算",
  "driver": "node",
  "script": "import fs from 'node:fs'; const v=JSON.parse(fs.readFileSync(process.env.JOB_INPUT_FILE,'utf8')); fs.writeFileSync(process.env.JOB_RESULT_FILE,JSON.stringify({sum:v.a+v.b}));",
  "input_schema": {"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]},
  "required_tags": ["os=linux", "env=base"],
  "timeout_seconds": 300
}
```

レスポンスは `{id, revision, spec, created_at, updated_at}` です。`driver` は `node` / `python`。タグは完全一致の AND 条件です。

## 投入と完了待ち

`POST /runs`:

```json
{"definition_id":"DEFINITION_ID","input":{"a":2,"b":3},"files":[]}
```

レスポンスは Run オブジェクトです。`id` を保存し `GET /runs/{id}` をポーリングしてください。主要フィールドは `id`, `revision`, `status`, `worker_id`, `result`, `error`, `exit_code`, `cancel_requested`, `created_at`, `started_at`, `finished_at` です。

`Idempotency-Key` ヘッダーはクライアントが要求ごとに一意に作成します。同じキー・同じ内容を再送すると元の Run を返し、同じキー・異なる内容は 409 です。キーはサーバー全体で共有されるため、アプリ名や UUID を含めてください。

`queued → assigned → running → succeeded / failed / canceled` の順に遷移します。ワーカー接続の切断やハートビート期限切れは `failed` です。ネットワーク断の検知には時間がかかることがあり、ハートビート期限は初期値30秒です。キャンセル要求直後は `cancel_requested=true` で停止確認を待つ場合があります。切断したワーカーが外部に与えた副作用は取り消せません。

## ファイル転送

1. `POST /artifacts/uploads` に `{"name":"input.txt","size":123}` を送る。
2. 返された `url` に、ファイルのバイト列を別の HTTP(S) PUT で送る。
3. `POST /artifacts/{id}/complete` に `{}` を送る。
4. JOB 投入時に `files: [{"name":"input.txt","artifact_id":"返されたid"}]` を付ける。
5. ワーカー内では `JOB_INPUT_DIR/input.txt` で読み取る。

URL が相対形式なら、API 呼び出しに使ったサーバーの origin を基準に解決します。署名付き URL へのリクエストに API トークンを付ける必要はありません。入力ファイルの PUT URL が期限切れになった場合は、`POST /artifacts/uploads` で新しい ID と URL を取得して再アップロードします。成果物の GET URL が期限切れの場合は、`GET /runs/{id}/artifacts` を再度呼ぶと新しい URL を取得できます。URL のホストはワーカー・呼び出し元アプリ・ブラウザーのそれぞれから到達できる必要があります。

成果物はスクリプトが `JOB_ARTIFACT_DIR` に保存します。成功後に `GET /runs/{id}/artifacts` を呼び、各 `url` から GET してください。

## ワーカー接続

`JOB_BASE_PATH + /worker` に WSS 接続します。接続時の Bearer トークンはワーカー用に分離されています。最初の JSON メッセージ `hello` でワーカー ID・アプリバージョン・プロトコルバージョン・タグ・ドライバを送信します。バージョンはサーバーと完全一致が必要です。

通常の利用では同梱の `job-worker` を使ってください。プロトコル型の正本は `crates/protocol/src/lib.rs` です。
