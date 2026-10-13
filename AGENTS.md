# Project guide

- This is the source repository for `uiui611/job-manage`.
- Keep the existing MIT `LICENSE` and copyright notice intact.
- The Rust workspace contains the shared protocol, server and Linux/Windows worker. Keep their application versions equal.
- The UI uses Bun 1.4.2, Vite, TypeScript and Web Components. Use committed lockfiles.
- Check Rust with `cargo fmt --all -- --check`, `cargo test --workspace --locked`, and `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- Check the UI in `web/` with `bun install --frozen-lockfile`, `bun run format:check`, and `bun run build`.
- A lost worker connection fails its active run; never automatically requeue or resume it. Manual retry creates a new run.
- `main` publishes both server and worker images before requesting deployment through the OIDC webhook. Pull requests must never publish images or notify deployments.
- Deployment updates preserve existing databases, buckets, credentials and queued runs. Do not bootstrap or reset storage as part of an image update.
- Public deployment manifests are examples. Keep real environment configuration outside this repository and verify the context and namespace before applying changes.
- Never commit secrets, environment files, private keys, kubeconfig, internal infrastructure inventories, build outputs or container archives. Inspect staged changes and run a redacted secret scan before pushing.
