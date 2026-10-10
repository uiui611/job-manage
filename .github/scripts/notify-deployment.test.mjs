import assert from "node:assert/strict";
import test from "node:test";
import { notifyDeployment, runNotification } from "./notify-deployment.mjs";

// Test-only placeholders. No requests reach GitHub or a deployment receiver.
const env = {
  ACTIONS_ID_TOKEN_REQUEST_URL: "https://token.actions.example/oidc?existing=1",
  ACTIONS_ID_TOKEN_REQUEST_TOKEN: "test-request-token",
  DEPLOY_WEBHOOK_URL: "https://deploy.example/v1/deployments",
  DEPLOY_OIDC_AUDIENCE: "https://deploy.example",
  DEPLOY_SERVICE_ID: "job-manage-server",
};

function fixture(responses, customEnv = env) {
  const calls = [];
  const logs = [];
  return {
    calls,
    logs,
    options: {
      env: customEnv,
      log: (line) => logs.push(line),
      fetchImpl: async (url, options) => {
        calls.push({ url: new URL(url), ...options });
        const next = responses.shift();
        if (next instanceof Error) throw next;
        assert.ok(next instanceof Response, "Unexpected extra network request");
        return next;
      },
    },
  };
}

test("obtains a scoped OIDC token and makes one exact deployment request", async () => {
  const f = fixture([
    Response.json({ value: "test-oidc-token" }),
    new Response(null, { status: 202 }),
  ]);
  assert.equal(await notifyDeployment(f.options), 202);
  assert.equal(f.calls.length, 2);
  const [oidc, webhook] = f.calls;
  assert.equal(oidc.url.searchParams.get("audience"), env.DEPLOY_OIDC_AUDIENCE);
  assert.equal(oidc.url.searchParams.get("existing"), "1");
  assert.equal(oidc.headers.Authorization, "Bearer test-request-token");
  assert.equal(webhook.url.href, env.DEPLOY_WEBHOOK_URL);
  assert.equal(webhook.method, "POST");
  assert.equal(webhook.headers.Authorization, "Bearer test-oidc-token");
  assert.equal(webhook.headers["Content-Type"], "application/json");
  assert.deepEqual(JSON.parse(webhook.body), { service: "job-manage-server" });
  for (const request of f.calls) {
    assert.equal(request.redirect, "error");
    assert.ok(request.signal instanceof AbortSignal);
  }
  assert.ok(f.logs.includes("::add-mask::test-oidc-token"));
  assert.ok(!f.logs.join("\n").includes("test-request-token"));
});

test("notifies the separately registered worker service", async () => {
  const f = fixture([Response.json({ value: "test-token" }), new Response(null, { status: 204 })], {
    ...env,
    DEPLOY_SERVICE_ID: "job-manage-worker",
  });
  assert.equal(await runNotification(f.options), 0);
  assert.deepEqual(JSON.parse(f.calls[1].body), { service: "job-manage-worker" });
});

test("OIDC rejection stops before the webhook and never logs its response body", async () => {
  const f = fixture([new Response("sensitive-oidc-body", { status: 403 })]);
  assert.equal(await runNotification(f.options), 1);
  assert.equal(f.calls.length, 1);
  assert.ok(f.logs.some((line) => line.startsWith("::warning::")));
  assert.ok(!f.logs.join("\n").includes("sensitive-oidc-body"));
});

test("webhook rejection is not retried and does not leak the response body", async () => {
  const f = fixture([
    Response.json({ value: "test-token" }),
    new Response("sensitive-webhook-body", { status: 502 }),
  ]);
  assert.equal(await runNotification(f.options), 1);
  assert.equal(f.calls.length, 2);
  assert.ok(f.logs.some((line) => line.includes("HTTP 502")));
  assert.ok(f.logs.at(-1).startsWith("::warning::"));
  assert.ok(!f.logs.join("\n").includes("sensitive-webhook-body"));
});

test("transport exceptions produce only a generic warning", async () => {
  const f = fixture([new Error("sensitive-network-detail")]);
  assert.equal(await runNotification(f.options), 1);
  assert.equal(f.calls.length, 1);
  assert.equal(f.logs.length, 1);
  assert.ok(f.logs[0].startsWith("::warning::"));
  assert.ok(!f.logs[0].includes("sensitive-network-detail"));
});

test("missing, malformed, and multiline tokens never reach the receiver", async () => {
  for (const response of [
    Response.json({}),
    Response.json({ value: "" }),
    Response.json({ value: "test\n::warning::unexpected" }),
    new Response("malformed-json"),
  ]) {
    const f = fixture([response]);
    assert.equal(await runNotification(f.options), 1);
    assert.equal(f.calls.length, 1);
    assert.ok(!f.logs.some((line) => line.startsWith("::add-mask::")));
  }
});

test("missing configuration fails before obtaining a credential", async () => {
  for (const name of Object.keys(env)) {
    const f = fixture([], { ...env, [name]: "" });
    assert.equal(await runNotification(f.options), 1);
    assert.equal(f.calls.length, 0);
    assert.equal(f.logs.length, 1);
  }
});

test("non-HTTPS endpoints and embedded credentials are rejected before any request", async () => {
  for (const override of [
    { DEPLOY_WEBHOOK_URL: "http://deploy.example/v1/deployments" },
    { ACTIONS_ID_TOKEN_REQUEST_URL: "http://token.actions.example/oidc" },
    { DEPLOY_WEBHOOK_URL: "https://user:private-password@deploy.example/v1/deployments" },
  ]) {
    const f = fixture([], { ...env, ...override });
    assert.equal(await runNotification(f.options), 1);
    assert.equal(f.calls.length, 0);
    assert.ok(!f.logs.join("\n").includes("private-password"));
  }
});
