import "./style.css";
import type {
  Artifact,
  Config,
  Definition,
  DefinitionSpec,
  Run,
  RunStatus,
  Settings,
  Token,
  Worker,
} from "./types";

type Child = Node | string | null | undefined | false;
type Action = () => void | Promise<unknown>;

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className = "",
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.className = className;
  for (const child of children) {
    if (child !== null && child !== undefined && child !== false) node.append(child);
  }
  return node;
}

const statuses: Record<RunStatus, string> = {
  queued: "待機中",
  assigned: "割り当て済み",
  running: "実行中",
  succeeded: "成功",
  failed: "失敗",
  canceled: "キャンセル",
};

class JobStatus extends HTMLElement {
  static observedAttributes = ["status", "label"];
  connectedCallback() {
    this.render();
  }
  attributeChangedCallback() {
    this.render();
  }
  private render() {
    const value = this.getAttribute("status") ?? "queued";
    const label = this.getAttribute("label") ?? statuses[value as RunStatus] ?? value;
    this.className = `status status-${value.replace(/[^a-z-]/g, "")}`;
    this.replaceChildren(el("span", "status-dot"), document.createTextNode(label));
  }
}
customElements.define("job-status", JobStatus);

function badge(status: string, label?: string) {
  const node = document.createElement("job-status");
  node.setAttribute("status", status);
  if (label) node.setAttribute("label", label);
  return node;
}

function icon(name: string) {
  const paths: Record<string, string> = {
    runs: "M8 5v14l11-7z",
    definitions: "M6 3h9l4 4v14H6z M14 3v5h5 M9 12h7 M9 16h7",
    workers: "M3 4h18v12H3z M8 21h8 M12 16v5 M7 8h.01 M11 8h6 M7 12h.01 M11 12h6",
    tokens: "M15 8a5 5 0 1 1-3 4L3 21H1v-4l9-9 M16 6h.01",
    settings: "M4 6h16 M4 12h16 M4 18h16 M9 3v6 M15 9v6 M9 15v6",
    arrow: "M5 12h14 M14 7l5 5-5 5",
    plus: "M12 5v14 M5 12h14",
    close: "M6 6l12 12 M18 6 6 18",
    search: "M21 21l-5-5 M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0",
    download: "M12 3v12 M7 10l5 5 5-5 M4 17v4h16v-4",
    terminal: "M5 7l5 5-5 5 M13 17h6",
  };
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.7");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(svg.namespaceURI, "path");
  path.setAttribute("d", paths[name] ?? paths.runs!);
  svg.append(path);
  return svg;
}

const fmtDate = (value?: string | null) => {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf())
    ? value
    : new Intl.DateTimeFormat("ja-JP", {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      }).format(date);
};
const shortId = (value?: string | null) => (value ? value.slice(0, 8) : "—");
const json = (value: unknown) => JSON.stringify(value, null, 2) ?? "null";
const message = (error: unknown) => (error instanceof Error ? error.message : String(error));
const terminal = (run: Run) => ["succeeded", "failed", "canceled"].includes(run.status);
const waitingReasons: Record<string, string> = {
  no_matching_worker: "実行環境・必須タグが一致するワーカーがありません。",
  workers_disabled: "対象ワーカーの JOB 受付が停止されています。",
  workers_offline: "対象ワーカーが接続されるのを待っています。",
  waiting_for_slot: "条件に一致するワーカーの空きを待っています。",
  scheduler_paused: "サーバーの JOB 割り当てが一時停止されています。",
};
const waitingReason = (reason: string) => waitingReasons[reason] ?? reason;
const bytes = (size: number) => {
  if (size < 1024) return `${size} B`;
  if (size < 1024 ** 2) return `${(size / 1024).toFixed(1)} KB`;
  if (size < 1024 ** 3) return `${(size / 1024 ** 2).toFixed(1)} MB`;
  return `${(size / 1024 ** 3).toFixed(1)} GB`;
};

function toast(text: string, failed = false) {
  const node = el("div", `toast${failed ? " toast-error" : ""}`, text);
  node.setAttribute("role", failed ? "alert" : "status");
  document.querySelector(".toast-region")?.append(node);
  window.setTimeout(() => node.remove(), 6500);
}

function button(label: string, action: Action, kind = "secondary", iconName?: string) {
  const node = el("button", `button button-${kind}`, iconName ? icon(iconName) : null, label);
  node.type = "button";
  node.addEventListener("click", async () => {
    node.disabled = true;
    try {
      await action();
    } catch (error) {
      toast(message(error), true);
    } finally {
      node.disabled = false;
    }
  });
  return node;
}

function link(text: string, path: string, className = "text-link") {
  const node = el("a", className, text);
  node.href = `#${path}`;
  return node;
}

function tags(values: string[]) {
  return el("div", "tags", ...values.map((value) => el("span", "tag", value)));
}

function empty(title: string, description: string, action?: HTMLElement) {
  return el(
    "div",
    "empty-state",
    el("span", "empty-icon", icon("terminal")),
    el("h3", "", title),
    el("p", "", description),
    action,
  );
}

function table(headers: string[], rows: HTMLElement[][]) {
  const head = el("thead", "", el("tr", "", ...headers.map((header) => el("th", "", header))));
  const body = el(
    "tbody",
    "",
    ...rows.map((cells) => el("tr", "", ...cells.map((cell) => el("td", "", cell)))),
  );
  return el("div", "table-scroll", el("table", "data-table", head, body));
}

function input(value = "", placeholder = "", type = "text") {
  const node = el("input", "input");
  node.type = type;
  node.value = value;
  node.placeholder = placeholder;
  return node;
}

function textarea(value: string, rows = 8, code = false) {
  const node = el("textarea", `input${code ? " code-input" : ""}`);
  node.value = value;
  node.rows = rows;
  node.spellcheck = !code;
  return node;
}

function select(options: Array<[string, string]>, value = "") {
  const node = el("select", "input");
  for (const [key, label] of options) {
    const option = el("option", "", label);
    option.value = key;
    node.append(option);
  }
  node.value = value;
  return node;
}

let fieldCounter = 0;
function field(label: string, control: HTMLElement, help?: string) {
  control.id = `field-${++fieldCounter}`;
  const labelNode = el("label", "field-label", label);
  labelNode.htmlFor = control.id;
  const helpNode = help ? el("p", "field-help", help) : null;
  if (helpNode) {
    helpNode.id = `${control.id}-help`;
    control.setAttribute("aria-describedby", helpNode.id);
  }
  return el("div", "field", labelNode, control, helpNode);
}

function parseObject(value: string, label: string): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    throw new Error(`${label}を正しい JSON で入力してください。`);
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error(`${label}は JSON オブジェクトにしてください。`);
  }
  return parsed as Record<string, unknown>;
}

function dialog(title: string, description: string, wide = false) {
  const node = el("dialog", `dialog${wide ? " dialog-wide" : ""}`);
  let submitting = false;
  node.addEventListener("cancel", (event) => {
    if (submitting) event.preventDefault();
  });
  const close = button("閉じる", () => node.close(), "icon", "close");
  close.setAttribute("aria-label", "ダイアログを閉じる");
  const titleNode = el("h2", "", title);
  titleNode.id = `dialog-title-${++fieldCounter}`;
  node.setAttribute("aria-labelledby", titleNode.id);
  const body = el("div", "dialog-body");
  const error = el("div", "form-error");
  error.hidden = true;
  error.setAttribute("role", "alert");
  const footer = el("div", "dialog-footer");
  const form = el("form", "", body, error, footer);
  node.append(
    el("div", "dialog-header", el("div", "", titleNode, el("p", "", description)), close),
    form,
  );
  node.addEventListener("close", () => node.remove());
  document.body.append(node);
  return {
    node,
    body,
    footer,
    show: () => node.showModal(),
    onSubmit(label: string, action: () => Promise<void>) {
      const submit = el("button", "button button-primary", label);
      submit.type = "submit";
      const cancel = button("戻る", () => node.close());
      footer.append(cancel, submit);
      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        if (!form.reportValidity()) return;
        if (submitting) return;
        submitting = true;
        submit.disabled = true;
        cancel.disabled = true;
        close.disabled = true;
        error.hidden = true;
        try {
          await action();
        } catch (cause) {
          error.textContent = message(cause);
          error.hidden = false;
        } finally {
          submitting = false;
          submit.disabled = false;
          cancel.disabled = false;
          close.disabled = false;
        }
      });
    },
  };
}

const nodeSample = `import { readFile, writeFile } from 'node:fs/promises';

const input = JSON.parse(await readFile(process.env.JOB_INPUT_FILE, 'utf8'));
console.log('処理を開始しました');
const result = { message: input.message ?? 'Hello from Node.js' };
await writeFile(process.env.JOB_RESULT_FILE, JSON.stringify(result));
console.log('処理が完了しました');
`;
const pythonSample = `import json
import os

with open(os.environ['JOB_INPUT_FILE'], encoding='utf-8') as f:
    params = json.load(f)
print('処理を開始しました', flush=True)
result = {'message': params.get('message', 'Hello from Python')}
with open(os.environ['JOB_RESULT_FILE'], 'w', encoding='utf-8') as f:
    json.dump(result, f, ensure_ascii=False)
print('処理が完了しました', flush=True)
`;

class JobApp extends HTMLElement {
  private config!: Config;
  private apiBase!: URL;
  private main = el("main", "main");
  private nav = el("nav", "nav");
  private routeVersion = 0;
  private timer?: number;
  private currentRefresh?: () => Promise<void>;
  private refreshBusy = false;
  private connection = el("span", "connection-note", "接続中");

  connectedCallback() {
    void this.start();
  }

  disconnectedCallback() {
    window.clearInterval(this.timer);
    window.removeEventListener("hashchange", this.onHashChange);
  }

  private onHashChange = () => this.route();

  private async start() {
    this.append(el("div", "boot-state", el("div", "spinner"), "JOB Platform を読み込んでいます…"));
    try {
      const response = await fetch(new URL("./config.json", document.baseURI), {
        credentials: "same-origin",
        cache: "no-store",
      });
      if (!response.ok) throw new Error(`設定を取得できませんでした（HTTP ${response.status}）。`);
      this.config = (await response.json()) as Config;
      if (typeof this.config.api_base !== "string")
        throw new Error("API の接続先が設定されていません。");
      this.apiBase = new URL(`${this.config.api_base.replace(/\/$/, "")}/`, location.origin);
      if (this.apiBase.origin !== location.origin)
        throw new Error("API の接続先は同一オリジンに設定してください。");
      this.shell();
      window.addEventListener("hashchange", this.onHashChange);
      this.route();
    } catch (error) {
      this.replaceChildren(
        empty(
          "管理画面を読み込めません",
          message(error),
          button("再読み込み", () => location.reload(), "primary"),
        ),
      );
    }
  }

  private shell() {
    const items = [
      ["runs", "実行一覧", "RUNS"],
      ["definitions", "JOB 定義", "DEFINITIONS"],
      ["workers", "ワーカー", "WORKERS"],
      ["tokens", "トークン", "ACCESS"],
      ["settings", "設定", "SETTINGS"],
    ];
    this.nav.setAttribute("aria-label", "メインナビゲーション");
    for (const [path, label] of items) {
      const item = link(label!, `/${path}`, "nav-link");
      item.prepend(icon(path!));
      item.dataset.route = path;
      this.nav.append(item);
    }
    const brand = link("", "/runs", "brand");
    brand.append(
      el("span", "brand-mark", icon("runs")),
      el("span", "", "JOB", el("small", "", "PLATFORM")),
    );
    const sidebar = el(
      "aside",
      "sidebar",
      brand,
      el("p", "nav-caption", "WORKSPACE"),
      this.nav,
      el(
        "div",
        "sidebar-bottom",
        el("div", "environment-label", el("span", "environment-dot"), "ホームラボ"),
        el("small", "", `v${this.config.version}`),
      ),
    );
    const topbar = el(
      "header",
      "topbar",
      el(
        "span",
        "topbar-label",
        "ワークスペース",
        el("span", "topbar-divider", "/"),
        "管理コンソール",
      ),
      el("div", "connection", el("span", "live-dot"), this.connection),
    );
    const region = el("div", "toast-region");
    region.setAttribute("aria-live", "polite");
    this.replaceChildren(sidebar, el("div", "workspace", topbar, this.main), region);
  }

  private async api<T>(
    path: string,
    method = "GET",
    body?: unknown,
    extraHeaders: Record<string, string> = {},
  ): Promise<T> {
    const url = new URL(path.replace(/^\//, ""), this.apiBase);
    const response = await fetch(url, {
      method,
      credentials: "same-origin",
      cache: "no-store",
      headers: {
        ...extraHeaders,
        Accept: "application/json",
        ...(method !== "GET" ? { "X-Job-Request": "1" } : {}),
        ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
      },
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    if (!response.ok) {
      let detail = `リクエストに失敗しました（HTTP ${response.status}）。`;
      try {
        const data = (await response.json()) as { error?: string };
        if (data.error) detail = data.error;
      } catch {
        // A proxy may return an HTML error page; retain the HTTP status above.
      }
      throw new Error(detail);
    }
    if (response.status === 204) return undefined as T;
    const text = await response.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  private header(eyebrow: string, title: string, description: string, action?: HTMLElement) {
    const heading = el(
      "div",
      "page-heading",
      el(
        "div",
        "",
        el("p", "eyebrow", eyebrow),
        el("h1", "", title),
        el("p", "page-description", description),
      ),
      action,
    );
    this.main.append(heading);
    return heading;
  }

  private route() {
    const current = ++this.routeVersion;
    window.clearInterval(this.timer);
    this.currentRefresh = undefined;
    this.refreshBusy = false;
    const segments = location.hash.replace(/^#\/?/, "").split("/");
    const screen = segments[0] || "runs";
    document.title = "JOB Platform";
    for (const item of this.nav.querySelectorAll<HTMLAnchorElement>("a")) {
      const active = item.dataset.route === screen;
      item.classList.toggle("active", active);
      if (active) item.setAttribute("aria-current", "page");
      else item.removeAttribute("aria-current");
    }
    this.main.replaceChildren();
    this.main.setAttribute("tabindex", "-1");
    const isCurrent = () => current === this.routeVersion;
    switch (screen) {
      case "runs":
        if (segments[1]) this.runDetail(segments[1], isCurrent);
        else this.runs(isCurrent);
        break;
      case "definitions":
        this.definitions(isCurrent);
        break;
      case "workers":
        this.workers(isCurrent);
        break;
      case "tokens":
        this.tokens(isCurrent);
        break;
      case "settings":
        this.settings(isCurrent);
        break;
      default:
        this.main.append(
          empty(
            "ページが見つかりません",
            "ナビゲーションからページを選んでください。",
            link("実行一覧へ", "/runs", "button button-primary"),
          ),
        );
    }
  }

  private refresh(task: () => Promise<void>, poll: boolean, isCurrent: () => boolean) {
    const notice = el("div", "refresh-error");
    notice.hidden = true;
    notice.setAttribute("role", "alert");
    this.main.append(notice);
    const run = async () => {
      if (this.refreshBusy || !isCurrent()) return;
      this.refreshBusy = true;
      try {
        await task();
        if (!isCurrent()) return;
        notice.hidden = true;
        this.connection.textContent = poll ? "4 秒ごとに更新" : "サーバーに接続済み";
        this.connection.parentElement?.classList.remove("connection-error");
      } catch (error) {
        if (!isCurrent()) return;
        for (const state of this.main.querySelectorAll(".empty-state")) {
          if (state.querySelector("h3")?.textContent === "読み込み中") {
            state.replaceWith(
              empty("データを取得できませんでした", "接続を確認してから再試行してください。"),
            );
          }
        }
        notice.replaceChildren(el("span", "", message(error)), button("再試行", run, "small"));
        notice.hidden = false;
        this.connection.textContent = "接続を確認してください";
        this.connection.parentElement?.classList.add("connection-error");
      } finally {
        if (isCurrent()) this.refreshBusy = false;
      }
    };
    this.currentRefresh = run;
    void run();
    if (poll) {
      this.timer = window.setInterval(() => {
        if (document.visibilityState === "visible" && !document.querySelector("dialog[open]"))
          void run();
      }, 4000);
    }
  }

  private runs(isCurrent: () => boolean) {
    this.header(
      "EXECUTIONS",
      "実行一覧",
      "直近 100 件の JOB の進行状況と結果を確認できます。",
      button("JOB を実行", () => this.runDialog(), "primary", "plus"),
    );
    const metrics = el("div", "metrics");
    const search = input("", "JOB 名・実行 ID で検索");
    search.setAttribute("aria-label", "JOB 名または実行 ID で検索");
    const filter = select([["", "すべての状態"], ...Object.entries(statuses)]);
    filter.setAttribute("aria-label", "実行状態で絞り込む");
    const rows = el("div", "", empty("読み込み中", "実行状況を確認しています。"));
    const count = el("span", "list-count");
    const panel = el(
      "section",
      "panel",
      el(
        "div",
        "panel-toolbar",
        el("div", "search-field", icon("search"), search),
        el("div", "filter-group", count, filter),
      ),
      rows,
    );
    this.main.append(metrics, panel);
    let runs: Run[] = [];
    const draw = () => {
      const query = search.value.toLocaleLowerCase();
      const shown = runs.filter(
        (run) =>
          (!filter.value || run.status === filter.value) &&
          `${run.definition_name} ${run.id}`.toLocaleLowerCase().includes(query),
      );
      count.textContent = `${shown.length} 件`;
      metrics.replaceChildren(
        this.metric(
          "実行中",
          runs.filter((run) => ["running", "assigned"].includes(run.status)).length,
          "進行している JOB",
          "running",
        ),
        this.metric(
          "待機中",
          runs.filter((run) => run.status === "queued").length,
          "ワーカーへの割り当て待ち",
          "queued",
        ),
        this.metric(
          "成功",
          runs.filter((run) => run.status === "succeeded").length,
          "完了した JOB",
          "succeeded",
        ),
        this.metric(
          "失敗",
          runs.filter((run) => run.status === "failed").length,
          "確認が必要な JOB",
          "failed",
        ),
      );
      if (!shown.length) {
        rows.replaceChildren(
          empty(
            runs.length ? "条件に一致する実行はありません" : "最初の JOB を実行しましょう",
            runs.length
              ? "検索文字や状態の条件を変更してください。"
              : "JOB 定義を作成し、入力パラメータを指定して実行できます。",
            runs.length
              ? undefined
              : button("JOB 定義を作成", () => this.definitionDialog(), "primary", "plus"),
          ),
        );
        return;
      }
      rows.replaceChildren(
        table(
          ["JOB / 実行 ID", "状態", "ワーカー", "開始 / 作成日時", ""],
          shown.map((run) => [
            el(
              "div",
              "cell-stack",
              link(run.definition_name || "名称未設定", `/runs/${run.id}`, "row-title"),
              el("span", "mono muted", `${shortId(run.id)} · rev.${run.revision}`),
            ),
            el(
              "div",
              "cell-stack",
              badge(run.status),
              run.cancel_requested && !terminal(run)
                ? el("small", "muted", "停止を要求しています")
                : null,
              run.waiting_reason
                ? el("small", "muted wrap", waitingReason(run.waiting_reason))
                : null,
            ),
            el("span", "mono muted", shortId(run.worker_id)),
            el("span", "time", fmtDate(run.started_at ?? run.created_at)),
            link("詳細 →", `/runs/${run.id}`, "text-link nowrap"),
          ]),
        ),
      );
    };
    search.addEventListener("input", draw);
    filter.addEventListener("change", draw);
    this.refresh(
      async () => {
        const data = await this.api<Run[]>("runs");
        if (!isCurrent()) return;
        runs = data;
        draw();
      },
      true,
      isCurrent,
    );
  }

  private metric(title: string, value: number, caption: string, status: string) {
    return el(
      "article",
      `metric metric-${status}`,
      el("div", "metric-label", title, el("span", "metric-dot")),
      el("strong", "metric-value", String(value)),
      el("small", "muted", caption),
    );
  }

  private runDetail(id: string, isCurrent: () => boolean) {
    this.main.append(link("← 実行一覧", "/runs", "back-link"));
    const actions = el("div", "actions");
    const heading = this.header("EXECUTION DETAIL", "実行の詳細", id, actions);
    heading.querySelector(".page-description")?.classList.add("mono", "break-all");
    const content = el("div", "detail-content", empty("読み込み中", "実行結果を取得しています。"));
    const logs = el("pre", "log-view", "ログを読み込んでいます…");
    logs.tabIndex = 0;
    logs.setAttribute("aria-label", "実行ログ");
    const logNotice = el("span", "muted");
    const follow = input("", "", "checkbox");
    follow.checked = true;
    const logPanel = el(
      "section",
      "panel",
      el(
        "div",
        "panel-header",
        el("h2", "", "実行ログ"),
        el("label", "checkbox-label", follow, "末尾を表示"),
      ),
      logs,
      el("div", "panel-footnote", logNotice),
    );
    const artifacts = el("div", "artifact-list");
    const artifactPanel = el(
      "section",
      "panel",
      el("div", "panel-header", el("h2", "", "成果物")),
      artifacts,
    );
    this.main.append(content, logPanel, artifactPanel);
    this.refresh(
      async () => {
        const encoded = encodeURIComponent(id);
        const [runResult, logResult, artifactResult] = await Promise.allSettled([
          this.api<Run>(`runs/${encoded}`),
          this.api<{ text: string; truncated: boolean }>(`runs/${encoded}/logs`),
          this.api<Artifact[]>(`runs/${encoded}/artifacts`),
        ]);
        if (!isCurrent()) return;
        if (runResult.status === "rejected") throw runResult.reason;
        const run = runResult.value;
        heading.querySelector("h1")!.textContent = run.definition_name || "実行の詳細";
        document.title = `${run.definition_name || "実行"} · JOB Platform`;
        actions.replaceChildren(badge(run.status));
        if (terminal(run)) {
          actions.append(
            button(
              "同じ入力で再実行",
              async () => {
                const next = await this.api<Run>(`runs/${encoded}/retry`, "POST");
                toast("新しい実行を作成しました。");
                location.hash = `/runs/${next.id}`;
              },
              "primary",
              "runs",
            ),
          );
        } else {
          const cancel = button(
            run.cancel_requested ? "停止を要求済み" : "キャンセル",
            () =>
              this.confirm(
                "JOB のキャンセル",
                "ワーカーに停止を要求します。接続中でない場合は、再接続または期限切れまで状態の確定を待ちます。",
                "停止を要求",
                async () => {
                  await this.api(`runs/${encoded}/cancel`, "POST");
                  toast("キャンセルを要求しました。");
                  await this.currentRefresh?.();
                },
              ),
            "danger",
          );
          cancel.disabled = run.cancel_requested;
          actions.append(cancel);
        }
        const summary = el(
          "section",
          "panel detail-summary",
          ...[
            ["定義リビジョン", String(run.revision)],
            ["ワーカー", run.worker_id || "未割り当て"],
            ["作成日時", fmtDate(run.created_at)],
            ["開始日時", fmtDate(run.started_at)],
            ["完了日時", fmtDate(run.finished_at)],
            ["終了コード", run.exit_code === null ? "—" : String(run.exit_code)],
          ].map(([label, value]) =>
            el("div", "detail-fact", el("span", "muted", label), el("strong", "break-all", value)),
          ),
        );
        const messages: HTMLElement[] = [];
        if (run.cancel_requested && !terminal(run))
          messages.push(
            el(
              "div",
              "info-banner",
              "停止要求を受け付けました。ワーカーによる停止確認を待っています。",
            ),
          );
        if (run.waiting_reason)
          messages.push(el("div", "info-banner", waitingReason(run.waiting_reason)));
        if (run.error) messages.push(el("div", "error-banner", run.error));
        const resultBody =
          run.result === null || run.result === undefined
            ? el("p", "muted panel-body", "結果データはまだありません。")
            : el("pre", "json-view", json(run.result));
        const inputPanel = el(
          "section",
          "panel",
          el("div", "panel-header", el("h2", "", "入力パラメータ")),
          el("pre", "json-view", json(run.input)),
        );
        if (run.files?.length)
          inputPanel.append(
            el(
              "div",
              "panel-footnote",
              "添付ファイル: ",
              ...run.files.map((file) => el("span", "tag", file.name)),
            ),
          );
        content.replaceChildren(
          ...messages,
          summary,
          el(
            "div",
            "two-column",
            inputPanel,
            el("section", "panel", el("div", "panel-header", el("h2", "", "結果")), resultBody),
          ),
        );
        if (logResult.status === "fulfilled") {
          const text = logResult.value.text || "ログはまだありません。";
          if (logs.textContent !== text) {
            logs.textContent = text;
            if (follow.checked) logs.scrollTop = logs.scrollHeight;
          }
          logNotice.textContent = logResult.value.truncated
            ? "ログの一部のみ表示しています（保存上限を超過）。"
            : "標準出力・標準エラーを表示しています。";
        } else {
          logNotice.textContent = `ログを取得できません: ${message(logResult.reason)}`;
        }
        if (artifactResult.status === "fulfilled") {
          const artifactItems = artifactResult.value.map((artifact) => {
            const a = el(
              "a",
              "artifact-row",
              el("span", "artifact-icon", icon("download")),
              el(
                "div",
                "cell-stack",
                el("strong", "", artifact.name),
                el("span", "muted", bytes(artifact.size)),
              ),
              el("span", "text-link", "ダウンロード"),
            );
            const url = new URL(artifact.url, location.origin);
            if (!["http:", "https:"].includes(url.protocol))
              throw new Error("成果物の URL が不正です。");
            a.href = url.href;
            a.target = "_blank";
            a.rel = "noopener noreferrer";
            return a;
          });
          if (!artifactItems.length)
            artifacts.replaceChildren(
              el(
                "p",
                "panel-body muted",
                terminal(run)
                  ? "この実行の成果物はありません。"
                  : "成果物は処理の完了後に表示されます。",
              ),
            );
          else artifacts.replaceChildren(...artifactItems);
        } else {
          artifacts.replaceChildren(
            el(
              "p",
              "panel-body error-text",
              `成果物を取得できません: ${message(artifactResult.reason)}`,
            ),
          );
        }
      },
      true,
      isCurrent,
    );
  }

  private definitions(isCurrent: () => boolean) {
    this.header(
      "JOB DEFINITIONS",
      "JOB 定義",
      "スクリプトと実行条件を定義し、パラメータを変えて利用します。",
      button("定義を作成", () => this.definitionDialog(), "primary", "plus"),
    );
    const list = el("div", "definition-grid", empty("読み込み中", "JOB 定義を取得しています。"));
    this.main.append(list);
    this.refresh(
      async () => {
        const definitions = await this.api<Definition[]>("definitions");
        if (!isCurrent()) return;
        list.replaceChildren(
          ...(definitions.length
            ? definitions.map((definition) => {
                const spec = definition.spec;
                return el(
                  "article",
                  "panel definition-card",
                  el(
                    "div",
                    "definition-card-top",
                    el(
                      "span",
                      `driver-icon driver-${spec.driver}`,
                      spec.driver === "python" ? "Py" : "JS",
                    ),
                    el("span", "revision", `rev.${definition.revision}`),
                  ),
                  el("h2", "", spec.name),
                  el("p", "definition-description", spec.description || "説明はありません。"),
                  tags(spec.required_tags),
                  el(
                    "div",
                    "definition-meta",
                    el("span", "", spec.driver === "python" ? "Python" : "Node.js"),
                    el("span", "", `制限時間 ${spec.timeout_seconds} 秒`),
                  ),
                  el(
                    "div",
                    "definition-footer",
                    button("編集", () => this.definitionDialog(definition)),
                    button("実行", () => this.runDialog(definition.id), "primary", "runs"),
                  ),
                );
              })
            : [
                empty(
                  "JOB 定義を作成しましょう",
                  "Node.js または Python のスクリプトから、最初の JOB を作成できます。",
                  button("定義を作成", () => this.definitionDialog(), "primary", "plus"),
                ),
              ]),
        );
      },
      false,
      isCurrent,
    );
  }

  private definitionDialog(existing?: Definition) {
    const modal = dialog(
      existing ? "JOB 定義を編集" : "JOB 定義を作成",
      existing
        ? "変更を保存すると新しいリビジョンが作成されます。既存の実行内容には影響しません。"
        : "実行するスクリプト、入力形式、ワーカーの条件を設定します。",
      true,
    );
    const name = input(existing?.spec.name ?? "", "例: GPU 画像処理");
    name.required = true;
    name.maxLength = 160;
    const description = textarea(existing?.spec.description ?? "", 2);
    const driver = select(
      [
        ["node", "Node.js"],
        ["python", "Python"],
      ],
      existing?.spec.driver ?? "node",
    );
    const timeout = input(String(existing?.spec.timeout_seconds ?? 3600), "", "number");
    timeout.min = "1";
    timeout.max = "604800";
    timeout.step = "1";
    timeout.required = true;
    const requiredTags = input(
      existing?.spec.required_tags.join(", ") ?? "",
      "例: gpu:nvidia, cuda:12, python:3.12",
    );
    const script = textarea(existing?.spec.script ?? nodeSample, 14, true);
    script.required = true;
    const schema = textarea(
      json(
        existing?.spec.input_schema ?? {
          type: "object",
          properties: { message: { type: "string" } },
          additionalProperties: true,
        },
      ),
      7,
      true,
    );
    schema.required = true;
    let previousDriver = driver.value;
    driver.addEventListener("change", () => {
      const previousSample = previousDriver === "node" ? nodeSample : pythonSample;
      if (script.value === previousSample || !script.value.trim())
        script.value = driver.value === "node" ? nodeSample : pythonSample;
      previousDriver = driver.value;
    });
    const contract = el(
      "details",
      "contract-help",
      el("summary", "", "スクリプトから入力と結果を扱うには"),
      el(
        "p",
        "",
        "JOB_INPUT_FILE の JSON を読み込み、結果を JOB_RESULT_FILE に JSON で保存します。添付入力は JOB_INPUT_DIR、成果物の保存先は JOB_ARTIFACT_DIR です。通常の標準出力はログに記録されます。依存ライブラリはワーカーの実行環境にあらかじめ用意してください。",
      ),
    );
    modal.body.append(
      field("名前", name),
      field("説明", description),
      el("div", "two-column", field("実行環境", driver), field("制限時間（秒）", timeout)),
      field(
        "必須タグ",
        requiredTags,
        "カンマで区切って入力します。すべてのタグを持つワーカーが対象です。空欄ならタグによる制限はありません。",
      ),
      field("スクリプト", script),
      contract,
      field("入力 JSON Schema", schema, "実行時の入力パラメータを検証する形式です。"),
    );
    modal.onSubmit(existing ? "新しいリビジョンを保存" : "定義を作成", async () => {
      const spec: DefinitionSpec = {
        name: name.value.trim(),
        description: description.value.trim(),
        driver: driver.value as "node" | "python",
        timeout_seconds: Number(timeout.value),
        required_tags: [
          ...new Set(
            requiredTags.value
              .split(",")
              .map((tag) => tag.trim())
              .filter(Boolean),
          ),
        ],
        script: script.value,
        input_schema: parseObject(schema.value, "入力 JSON Schema"),
      };
      if (!spec.name) throw new Error("名前を入力してください。");
      if (new TextEncoder().encode(spec.name).length > 160)
        throw new Error(
          "名前は UTF-8 で 160 バイト以内にしてください（日本語のみなら約 53 文字）。",
        );
      if (!spec.script.trim()) throw new Error("スクリプトを入力してください。");
      if (!Number.isSafeInteger(spec.timeout_seconds) || spec.timeout_seconds < 1)
        throw new Error("制限時間は 1 秒以上の整数にしてください。");
      await this.api(
        existing ? `definitions/${encodeURIComponent(existing.id)}` : "definitions",
        existing ? "PUT" : "POST",
        spec,
      );
      modal.node.close();
      toast(existing ? "新しいリビジョンを保存しました。" : "JOB 定義を作成しました。");
      if (location.hash === "#/definitions") await this.currentRefresh?.();
      else location.hash = "/definitions";
    });
    modal.show();
  }

  private async runDialog(selectedId?: string) {
    const definitions = await this.api<Definition[]>("definitions");
    if (!definitions.length) {
      this.definitionDialog();
      return;
    }
    const modal = dialog(
      "JOB を実行",
      "定義を選び、今回の入力パラメータを指定します。実行は空きワーカーへ順次割り当てられます。",
      true,
    );
    const definition = select(
      definitions.map((item) => [item.id, `${item.spec.name} · rev.${item.revision}`]),
      selectedId ?? definitions[0]!.id,
    );
    const info = el("div", "run-definition-info");
    const params = textarea("{}", 9, true);
    params.required = true;
    const schemaText = el("pre", "json-view compact");
    const schema = el(
      "details",
      "contract-help",
      el("summary", "", "入力 JSON Schema を表示"),
      schemaText,
    );
    const files = input("", "", "file");
    files.multiple = true;
    const uploadProgress = el("p", "field-help");
    const update = () => {
      const selected = definitions.find((item) => item.id === definition.value)!;
      info.replaceChildren(
        el("span", "tag", selected.spec.driver === "node" ? "Node.js" : "Python"),
        el("span", "muted", `制限時間 ${selected.spec.timeout_seconds} 秒`),
        tags(selected.spec.required_tags),
      );
      schemaText.textContent = json(selected.spec.input_schema);
    };
    definition.addEventListener("change", update);
    update();
    modal.body.append(
      field("JOB 定義", definition),
      info,
      field("入力パラメータ（JSON）", params),
      schema,
      field(
        "入力ファイル（任意）",
        files,
        "最大 32 ファイル。ファイル名は半角英数字・ハイフン・アンダースコア・ピリオドで指定してください。同名ファイルは追加できません。ワーカーの JOB_INPUT_DIR 内へ渡されます。",
      ),
      uploadProgress,
    );
    const uploaded = new Map<File, { name: string; artifact_id: string }>();
    let previousSubmission: { body: string; key: string } | undefined;
    modal.onSubmit("実行を作成", async () => {
      const values = parseObject(params.value, "入力パラメータ");
      const selectedFiles = Array.from(files.files ?? []);
      if (selectedFiles.length > 32) throw new Error("入力ファイルは 32 個以内にしてください。");
      const invalidFile = selectedFiles.find(
        (file) =>
          !/^[a-zA-Z0-9_.-]{1,180}$/.test(file.name) ||
          file.name.endsWith(".") ||
          /^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i.test(file.name),
      );
      if (invalidFile)
        throw new Error(
          `使用できないファイル名です: ${invalidFile.name}。半角英数字・ハイフン・アンダースコア・ピリオドで 180 文字以内にし、末尾のピリオドや Windows の予約名を避けてください。`,
        );
      if (
        new Set(selectedFiles.map((file) => file.name.toLowerCase())).size !== selectedFiles.length
      )
        throw new Error("入力ファイルの名前が重複しています。");
      const attachments: Array<{ name: string; artifact_id: string }> = [];
      for (const [index, file] of selectedFiles.entries()) {
        const cached = uploaded.get(file);
        if (cached) {
          attachments.push(cached);
          continue;
        }
        uploadProgress.textContent = `ファイルをアップロード中 (${index + 1}/${selectedFiles.length}): ${file.name}`;
        const upload = await this.api<{ id: string; url: string }>("artifacts/uploads", "POST", {
          name: file.name,
          size: file.size,
        });
        const url = new URL(upload.url, location.origin);
        if (!["http:", "https:"].includes(url.protocol))
          throw new Error("アップロード先の URL が不正です。");
        const response = await fetch(url, { method: "PUT", credentials: "omit", body: file });
        if (!response.ok)
          throw new Error(
            `ファイルをアップロードできませんでした（HTTP ${response.status}）: ${file.name}`,
          );
        await this.api(`artifacts/${encodeURIComponent(upload.id)}/complete`, "POST");
        const attachment = { name: file.name, artifact_id: upload.id };
        uploaded.set(file, attachment);
        attachments.push(attachment);
      }
      uploadProgress.textContent = "実行を作成しています…";
      const submission = {
        definition_id: definition.value,
        input: values,
        files: attachments,
      };
      const serialized = JSON.stringify(submission);
      if (previousSubmission?.body !== serialized)
        previousSubmission = {
          body: serialized,
          key: Array.from(crypto.getRandomValues(new Uint8Array(16)), (value) =>
            value.toString(16).padStart(2, "0"),
          ).join(""),
        };
      const run = await this.api<Run>("runs", "POST", submission, {
        "Idempotency-Key": previousSubmission.key,
      });
      modal.node.close();
      toast("実行を作成しました。");
      location.hash = `/runs/${run.id}`;
    });
    modal.show();
  }

  private workers(isCurrent: () => boolean) {
    this.header(
      "WORKER POOL",
      "ワーカー",
      "接続中の実行環境と、JOB を受け付ける状態を管理します。",
    );
    const content = el("section", "panel", empty("読み込み中", "ワーカーの状態を確認しています。"));
    this.main.append(
      content,
      el(
        "p",
        "page-footnote",
        "受付を停止しても、すでに実行している JOB は続行されます。ワーカーは再接続時も同じ ID を使用します。",
      ),
    );
    this.refresh(
      async () => {
        const workers = await this.api<Worker[]>("workers");
        if (!isCurrent()) return;
        if (!workers.length) {
          content.replaceChildren(
            empty(
              "ワーカーはまだ接続されていません",
              "ワーカー用トークンを作成し、実行環境からサーバーに接続してください。",
              link("トークンを管理", "/tokens", "button button-primary"),
            ),
          );
          return;
        }
        content.replaceChildren(
          table(
            ["ワーカー", "接続 / 受付", "実行環境・タグ", "現在の JOB", "最終通信", ""],
            workers.map((worker) => [
              el(
                "div",
                "cell-stack",
                el("strong", "", worker.name),
                el("span", "mono muted", `${shortId(worker.id)} · v${worker.version}`),
              ),
              el(
                "div",
                "cell-stack",
                badge(
                  worker.status === "offline" ? "offline" : "online",
                  worker.status === "offline" ? "オフライン" : "オンライン",
                ),
                el(
                  "small",
                  worker.enabled ? "muted" : "warning-text",
                  worker.enabled ? "JOB 受付中" : "受付停止中",
                ),
              ),
              el(
                "div",
                "cell-stack",
                el("span", "muted", worker.drivers.join(" / ") || "ドライバなし"),
                tags(worker.tags),
              ),
              worker.active_run_id
                ? link(
                    shortId(worker.active_run_id),
                    `/runs/${worker.active_run_id}`,
                    "text-link mono",
                  )
                : el("span", "muted", "—"),
              el("span", "time", fmtDate(worker.last_seen)),
              button(
                worker.enabled ? "受付を停止" : "受付を再開",
                async () => {
                  await this.api(`workers/${encodeURIComponent(worker.id)}`, "PATCH", {
                    enabled: !worker.enabled,
                  });
                  toast(
                    worker.enabled
                      ? "新しい JOB の受付を停止しました。"
                      : "JOB の受付を再開しました。",
                  );
                  await this.currentRefresh?.();
                },
                "small",
              ),
            ]),
          ),
        );
      },
      true,
      isCurrent,
    );
  }

  private tokens(isCurrent: () => boolean) {
    this.header(
      "ACCESS TOKENS",
      "トークン",
      "ワーカーの接続と、外部アプリケーションからの API 利用に使います。",
      button("トークンを作成", () => this.tokenDialog(), "primary", "plus"),
    );
    const content = el("section", "panel", empty("読み込み中", "トークンを取得しています。"));
    this.main.append(
      content,
      el(
        "p",
        "page-footnote",
        "トークンの値は作成時に一度だけ表示されます。用途ごとに作成し、不要になったら失効してください。",
      ),
    );
    this.refresh(
      async () => {
        const tokens = await this.api<Token[]>("tokens");
        if (!isCurrent()) return;
        content.replaceChildren(
          tokens.length
            ? table(
                ["名前", "用途", "状態", "作成日時", ""],
                tokens.map((token) => [
                  el(
                    "div",
                    "cell-stack",
                    el("strong", "", token.name),
                    el("span", "mono muted", shortId(token.id)),
                  ),
                  el("span", "tag", token.kind === "worker" ? "ワーカー" : "API"),
                  badge(token.revoked ? "offline" : "online", token.revoked ? "失効済み" : "有効"),
                  el("span", "time", fmtDate(token.created_at)),
                  token.revoked
                    ? el("span", "muted", "—")
                    : button(
                        "失効",
                        () =>
                          this.confirm(
                            "トークンを失効",
                            `「${token.name}」を使用する新しい接続や API リクエストは認証できなくなります。取り消しはできません。`,
                            "失効する",
                            async () => {
                              await this.api(`tokens/${encodeURIComponent(token.id)}`, "DELETE");
                              toast("トークンを失効しました。");
                              await this.currentRefresh?.();
                            },
                          ),
                        "danger-small",
                      ),
                ]),
              )
            : empty(
                "トークンを作成しましょう",
                "最初にワーカー用トークンを作成し、実行環境を接続します。",
                button("トークンを作成", () => this.tokenDialog(), "primary", "plus"),
              ),
        );
      },
      false,
      isCurrent,
    );
  }

  private tokenDialog() {
    const modal = dialog("トークンを作成", "用途と、後から識別できる名前を指定してください。");
    const name = input("", "例: デスクトップ GPU ワーカー");
    name.required = true;
    name.maxLength = 200;
    const kind = select(
      [
        ["worker", "ワーカー接続"],
        ["api", "外部アプリケーション API"],
      ],
      "worker",
    );
    modal.body.append(field("名前", name), field("用途", kind));
    modal.onSubmit("作成", async () => {
      if (!name.value.trim()) throw new Error("名前を入力してください。");
      const result = await this.api<{ id: string; token: string }>("tokens", "POST", {
        name: name.value.trim(),
        kind: kind.value,
      });
      modal.node.close();
      const reveal = dialog(
        "トークンを作成しました",
        "この画面を閉じると、トークンの値は再表示できません。接続先の設定に保存してください。",
      );
      const token = textarea(result.token, 4, true);
      token.readOnly = true;
      token.addEventListener("focus", () => token.select());
      reveal.body.append(field("アクセストークン", token));
      reveal.footer.append(
        button("コピー", async () => {
          try {
            await navigator.clipboard.writeText(result.token);
            toast("トークンをコピーしました。");
          } catch {
            token.focus();
            token.select();
            toast("トークンを選択しました。コピー操作で保存してください。");
          }
        }),
        button("閉じる", () => reveal.node.close(), "primary"),
      );
      reveal.show();
      await this.currentRefresh?.();
    });
    modal.show();
  }

  private confirm(title: string, description: string, label: string, action: () => Promise<void>) {
    const modal = dialog(title, description);
    modal.onSubmit(label, async () => {
      await action();
      modal.node.close();
    });
    modal.show();
  }

  private settings(isCurrent: () => boolean) {
    this.header(
      "SERVER SETTINGS",
      "設定",
      "新しい JOB の割り当てと、現在のサーバー設定を確認できます。",
    );
    const scheduler = el("section", "panel scheduler-panel");
    const content = el("section", "panel", empty("読み込み中", "サーバー設定を取得しています。"));
    this.main.append(
      scheduler,
      content,
      el(
        "p",
        "page-footnote",
        "公開パスや保存先などの基本設定は、サーバーの環境変数で変更します。",
      ),
    );
    this.refresh(
      async () => {
        const settings = await this.api<Settings>("settings");
        if (!isCurrent()) return;
        scheduler.replaceChildren(
          el(
            "div",
            "",
            el("h2", "", "JOB の割り当て"),
            el(
              "p",
              "field-help",
              settings.scheduler_paused
                ? "新しい JOB の割り当てを一時停止しています。すでに実行中の JOB は続行されます。"
                : "待機中の JOB を、条件の合う空きワーカーへ割り当てます。",
            ),
          ),
          el(
            "div",
            "actions",
            badge(
              settings.scheduler_paused ? "offline" : "online",
              settings.scheduler_paused ? "一時停止中" : "稼働中",
            ),
            button(
              settings.scheduler_paused ? "割り当てを再開" : "割り当てを一時停止",
              async () => {
                await this.api("settings", "PATCH", {
                  scheduler_paused: !settings.scheduler_paused,
                });
                toast(
                  settings.scheduler_paused
                    ? "JOB の割り当てを再開しました。"
                    : "新しい JOB の割り当てを一時停止しました。",
                );
                await this.currentRefresh?.();
              },
            ),
          ),
        );
        const values = [
          ["バージョン", settings.version],
          ["公開ベースパス", settings.base_path || "/"],
          ["API ベースパス", this.config.api_base],
          [
            "管理画面の認証",
            this.config.auth_mode === "none" ? "開発モード（認証なし）" : "前段プロキシで管理",
          ],
          ["Heartbeat タイムアウト", `${settings.heartbeat_timeout_seconds} 秒`],
          ["成果物の最大サイズ", bytes(settings.max_artifact_bytes)],
          ["自動再試行", settings.auto_retry ? "有効" : "無効（手動で再実行）"],
          [
            "ストレージ",
            typeof settings.storage === "string" ? settings.storage : json(settings.storage),
          ],
        ];
        content.replaceChildren(
          el("div", "panel-header", el("h2", "", "サーバー"), badge("online", "接続済み")),
          el(
            "dl",
            "settings-list",
            ...values.flatMap(([label, value]) => [el("dt", "", label), el("dd", "", value)]),
          ),
        );
      },
      false,
      isCurrent,
    );
  }
}

customElements.define("job-app", JobApp);
