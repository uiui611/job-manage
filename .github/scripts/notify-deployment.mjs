import { pathToFileURL } from "node:url";

const warning =
  "::warning::Deployment notification failed. Both images are already published; check the deployment receiver separately.";

function required(env, name) {
  const value = env[name];
  if (typeof value !== "string" || !value.trim()) {
    throw new Error("Deployment notification configuration is incomplete");
  }
  return value;
}

function httpsUrl(value) {
  const url = new URL(value);
  if (url.protocol !== "https:" || url.username || url.password) {
    throw new Error("Deployment notification requires an HTTPS endpoint");
  }
  return url;
}

export async function notifyDeployment({
  env = process.env,
  fetchImpl = fetch,
  log = console.log,
} = {}) {
  // Validate configuration before obtaining a credential. No values are logged.
  const oidcUrl = httpsUrl(required(env, "ACTIONS_ID_TOKEN_REQUEST_URL"));
  const requestToken = required(env, "ACTIONS_ID_TOKEN_REQUEST_TOKEN");
  const webhookUrl = httpsUrl(required(env, "DEPLOY_WEBHOOK_URL"));
  const audience = required(env, "DEPLOY_OIDC_AUDIENCE");
  const service = required(env, "DEPLOY_SERVICE_ID");
  oidcUrl.searchParams.set("audience", audience);

  const tokenResponse = await fetchImpl(oidcUrl, {
    headers: { Authorization: `Bearer ${requestToken}` },
    redirect: "error",
    signal: AbortSignal.timeout(10_000),
  });
  log(`OIDC token request returned HTTP ${tokenResponse.status}.`);
  if (!tokenResponse.ok) throw new Error("OIDC token request failed");

  const { value: token } = await tokenResponse.json();
  if (typeof token !== "string" || !token || /[\r\n]/.test(token)) {
    throw new Error("OIDC token response was invalid");
  }
  log(`::add-mask::${token}`);

  // The receiver selects registered Deployments. Image tags, manifests, and
  // Kubernetes credentials are deliberately not part of this request.
  const response = await fetchImpl(webhookUrl, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ service }),
    redirect: "error",
    signal: AbortSignal.timeout(10_000),
  });
  log(`Deployment webhook returned HTTP ${response.status}.`);
  if (!response.ok) throw new Error("Deployment webhook rejected the request");
  return response.status;
}

export async function runNotification(options = {}) {
  try {
    await notifyDeployment(options);
    return 0;
  } catch {
    // Fetch errors and response bodies can contain credentials. Never echo them.
    (options.log ?? console.log)(warning);
    return 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await runNotification();
}
